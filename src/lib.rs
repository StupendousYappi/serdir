// Copyright (c) 2016-2021 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Helpers for serving HTTP GET and HEAD responses asynchronously with the
//! [http](http://crates.io/crates/http) crate and [tokio](https://crates.io/crates/tokio).
//! Works well with [hyper](https://crates.io/crates/hyper) 1.x.
//!
//! This crate supplies a way to respond to HTTP GET and HEAD requests:
//!
//! *   the `serve` function can be used to serve an `Entity`, a trait representing reusable,
//!     byte-rangeable HTTP entities. `Entity` must be able to produce exactly the same data on
//!     every call, know its size in advance, and be able to produce portions of the data on demand.
//!
//! It supplies a static file `Entity` implementation and a (currently Unix-only)
//! helper for serving a full directory tree from the local filesystem, including
//! automatically looking for `.gz`-suffixed files when the client advertises
//! `Accept-Encoding: gzip`.
//!
//! # Why the weird type bounds? Why not use `hyper::Body` and `BoxError`?
//!
//! These bounds are compatible with `bytes::Bytes` and `BoxError`, and most callers will use
//! those types.
//!
//! There are times when it's desirable to have more flexible ownership provided by a
//! type such as `reffers::ARefs<'static, [u8]>`. One is `mmap`-based file serving:
//! `bytes::Bytes` would require copying the data in each chunk. An implementation with `ARefs`
//! could instead `mmap` and `mlock` the data on another thread and provide chunks which `munmap`
//! when dropped. In these cases, the caller can supply an alternate implementation of the
//! `http_body::Body` trait which uses a different `Data` type than `bytes::Bytes`.

#![deny(missing_docs, clippy::print_stderr, clippy::print_stdout)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use bytes::Buf;
use futures_core::Stream;
use http::header::{HeaderMap, HeaderValue};
use std::error::Error;
use std::fmt::Display;
use std::io::Error as IOError;
use std::io::ErrorKind;
use std::ops::Range;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::SystemTime;

/// Returns a HeaderValue for the given formatted data.
/// Caller must make two guarantees:
///    * The data fits within `max_len` (or the write will panic).
///    * The data are ASCII (or HeaderValue's safety will be violated).
macro_rules! unsafe_fmt_ascii_val {
    ($max_len:expr, $fmt:expr, $($arg:tt)+) => {{
        let mut buf = bytes::BytesMut::with_capacity($max_len);
        use std::fmt::Write;
        write!(buf, $fmt, $($arg)*).expect("fmt_val fits within provided max len");
        unsafe {
            http::header::HeaderValue::from_maybe_shared_unchecked(buf.freeze())
        }
    }}
}

fn as_u64(len: usize) -> u64 {
    const {
        assert!(std::mem::size_of::<usize>() <= std::mem::size_of::<u64>());
    };
    len as u64
}

/// A type-erased error.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Error returned by this crate's public APIs.
#[derive(Debug)]
pub enum ServeFilesError {
    /// The path exists but is not a regular file.
    NotAFile(PathBuf),
    /// The input path is not a directory.
    NotADirectory(PathBuf),
    /// The requested file was not found.
    NotFound,
    /// The input path is invalid (e.g., contains NUL bytes or ".." segments).
    InvalidPath(String),
    /// An unexpected I/O error occurred.
    IOError(IOError),
}

impl Display for ServeFilesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServeFilesError::NotAFile(path) => {
                write!(f, "Path is not a file: {}", path.display())
            }
            ServeFilesError::NotADirectory(path) => {
                write!(f, "Path is not a directory: {}", path.display())
            }
            ServeFilesError::NotFound => write!(f, "File not found"),
            ServeFilesError::InvalidPath(msg) => write!(f, "Invalid path: {}", msg),
            ServeFilesError::IOError(err) => write!(f, "I/O error: {}", err),
        }
    }
}

impl Error for ServeFilesError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ServeFilesError::IOError(err) => Some(err),
            _ => None,
        }
    }
}

impl From<IOError> for ServeFilesError {
    fn from(err: IOError) -> Self {
        if err.kind() == ErrorKind::NotFound {
            ServeFilesError::NotFound
        } else {
            ServeFilesError::IOError(err)
        }
    }
}

mod body;

pub mod served_dir;

mod compression;
mod etag;
mod file;
mod platform;
mod range;
mod serving;

pub use crate::body::Body;
pub use crate::compression::{detect_compression_support, CompressionSupport};
pub use crate::file::FileEntity;
pub use crate::serving::serve;

/// A reusable, read-only, byte-rangeable HTTP entity for GET and HEAD serving.
/// Must return exactly the same data on every call.
pub trait Entity: 'static + Send + Sync {
    /// The type of errors produced in [`Self::get_range`] chunks and in the final stream.
    ///
    /// [`BoxError`] is a good choice for most implementations.
    ///
    /// This must be convertable from [`BoxError`] to allow `serve_files` to
    /// inject errors.
    ///
    /// Note that errors returned directly from the body to `hyper` just drop
    /// the stream abruptly without being logged. Callers might use an
    /// intermediary service for better observability.
    type Error: 'static + From<BoxError>;

    /// The type of a data chunk.
    ///
    /// Commonly `bytes::Bytes` but may be something more exotic.
    type Data: 'static + Buf + From<Vec<u8>> + From<&'static [u8]>;

    /// Returns the length of the entity's body in bytes.
    fn len(&self) -> u64;

    /// Returns true iff the entity's body has length 0.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Gets the body bytes indicated by `range`.
    ///
    /// The stream must return exactly `range.end - range.start` bytes or fail early with an `Err`.
    #[allow(clippy::type_complexity)]
    fn get_range(
        &self,
        range: Range<u64>,
    ) -> Pin<Box<dyn Stream<Item = Result<Self::Data, Self::Error>> + Send + Sync>>;

    /// Adds entity headers such as `Content-Type` to the supplied `HeaderMap`.
    /// In particular, these headers are the "other representation header fields" described by [RFC
    /// 7233 section 4.1](https://tools.ietf.org/html/rfc7233#section-4.1); they should exclude
    /// `Content-Range`, `Date`, `Cache-Control`, `ETag`, `Expires`, `Content-Location`, and `Vary`.
    ///
    /// This function will be called only when that section says that headers such as
    /// `Content-Type` should be included in the response.
    fn add_headers(&self, _: &mut HeaderMap);

    /// Returns an etag for this entity, if available.
    /// Implementations are encouraged to provide a strong etag. [RFC 7232 section
    /// 2.1](https://tools.ietf.org/html/rfc7232#section-2.1) notes that only strong etags
    /// are usable for sub-range retrieval.
    fn etag(&self) -> Option<HeaderValue>;

    /// Returns the last modified time of this entity, if available.
    /// Note that `serve` may serve an earlier `Last-Modified:` date than the one returned here if
    /// this time is in the future, as required by [RFC 7232 section
    /// 2.2.1](https://tools.ietf.org/html/rfc7232#section-2.2.1).
    fn last_modified(&self) -> Option<SystemTime>;

    /// Reads the entire body of this entity into a `Bytes`.
    ///
    /// This is a convenience method for testing and debugging.
    #[cfg(test)]
    #[allow(async_fn_in_trait)]
    async fn read_body(&self) -> Result<bytes::Bytes, Self::Error>
    where
        Self: Sized,
    {
        use futures_util::StreamExt;

        let chunks = self.get_range(0..self.len()).collect::<Vec<_>>().await;
        let mut bytes = bytes::BytesMut::new();
        for chunk in chunks {
            match chunk {
                Ok(chunk) => bytes.extend_from_slice(chunk.chunk()),
                Err(err) => return Err(err),
            }
        }
        Ok(bytes.freeze())
    }
}
