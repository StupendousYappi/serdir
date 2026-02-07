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
use http::{Request, Response, StatusCode};
use std::error::Error;
use std::fmt::Display;
use std::fs::File;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Error as IOError;
use std::io::ErrorKind;
use std::ops::Range;
use std::path::{Path, PathBuf};
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
    /// Returned by ServedDirBuilder if configuration is invalid.
    ConfigError(String),
    /// The path is a directory.
    IsDirectory(PathBuf),
    /// The requested file was not found.
    NotFound(Option<FileEntity<bytes::Bytes>>),
    /// Error compressing file with Brotli.
    CompressionError(String, IOError),
    /// The input path is invalid (e.g., contains NUL bytes or ".." segments).
    InvalidPath(String),
    /// An unexpected I/O error occurred.
    IOError(IOError),
}

impl Display for ServeFilesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServeFilesError::ConfigError(msg) => write!(f, "{}", msg),
            ServeFilesError::IsDirectory(path) => {
                write!(f, "Path is a directory: {}", path.display())
            }
            ServeFilesError::NotFound(_) => write!(f, "File not found"),
            ServeFilesError::InvalidPath(msg) => write!(f, "Invalid path: {}", msg),
            ServeFilesError::IOError(err) => write!(f, "I/O error: {}", err),
            ServeFilesError::CompressionError(msg, err) => {
                write!(f, "Brotli compression error: {} (I/O error: {})", msg, err)
            }
        }
    }
}

impl Error for ServeFilesError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ServeFilesError::IOError(err) => Some(err),
            ServeFilesError::CompressionError(_, err) => Some(err),
            _ => None,
        }
    }
}

impl From<IOError> for ServeFilesError {
    fn from(err: IOError) -> Self {
        if err.kind() == ErrorKind::NotFound {
            ServeFilesError::NotFound(None)
        } else {
            ServeFilesError::IOError(err)
        }
    }
}

mod body;

pub mod served_dir;

#[cfg(feature = "hyper")]
/// Hyper service integration.
pub mod hyper;

#[cfg(feature = "tower")]
/// Tower service integration.
pub mod tower;

#[cfg(feature = "runtime-compression")]
mod brotli_cache;
mod compression;
mod etag;
mod file;
mod platform;
mod range;
mod serving;

pub use crate::body::Body;
pub use crate::compression::CompressionSupport;
pub use crate::file::FileEntity;
pub use crate::serving::serve;

/// Returns a Request based on the input request, but with an empty body.
pub(crate) fn request_head<B>(req: &Request<B>) -> Request<()> {
    let mut request = Request::builder()
        .method(req.method().clone())
        .uri(req.uri().clone())
        .version(req.version())
        .body(())
        .expect("request head should be valid");
    *request.headers_mut() = req.headers().clone();
    request
}

/// Returns a basic `Response` with the given status code.
pub(crate) fn status_response(status: StatusCode) -> Response<Body> {
    let reason = status.canonical_reason().unwrap_or("Unknown");
    Response::builder()
        .status(status)
        .body(Body::from(reason))
        .expect("status response should be valid")
}

/// Basic metadata about a particular version of a file, used as a cache key.
#[derive(Hash, Eq, PartialEq, Clone, Copy)]
pub(crate) struct FileInfo {
    path_hash: u64,
    len: u64,
    mtime: SystemTime,
}

impl FileInfo {
    /// Creates a new FileInfo if the path is a file, otherwise returns an error.
    pub(crate) fn open_file(path: &Path, file: &File) -> Result<Self, ServeFilesError> {
        // Rust's default hasher is relatively secure... it uses a random seed
        // and currently uses the siphash algorithm, which is secure as long as
        // the seed is random. The file hash is only used internally in memory,
        // and doesn't to be consistent across reboots or different machines.
        // Because we throw away the full path and just use the hash instead, in
        // theory, an attacker who has the ability to write new static files to
        // one directory could force a collision with a file in another
        // directory if this hash function were not secure.
        // We use rapidhash (a non-cryptographic hash) to hash file contents
        // because we need an algorithm with portable output (consistent across
        // reboots and machines) and because performance matters more there, but
        // because it's only used for etag values for a single resource, an
        // attacker in that scenario wouldn't be able to change the observed
        // contents of any file other than the one he modified.
        let mut hasher = DefaultHasher::new();
        path.hash(&mut hasher);
        let path_hash: u64 = hasher.finish();
        let metadata = file.metadata()?;
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            return Err(ServeFilesError::IsDirectory(path.to_path_buf()));
        }
        // if it's not a directory and not a file, we don't want to handle it
        // and behave as if it doesn't exist
        if !file_type.is_file() {
            return Err(ServeFilesError::NotFound(None));
        }
        Ok(Self {
            path_hash,
            len: metadata.len(),
            mtime: metadata.modified()?,
        })
    }

    pub(crate) fn for_path(path: &Path) -> Result<Self, ServeFilesError> {
        let file = File::open(path)?;
        Self::open_file(path, &file)
    }

    /// Returns the length of the file in bytes.
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// Returns the last modification time of the file.
    pub(crate) fn mtime(&self) -> SystemTime {
        self.mtime
    }

    pub(crate) fn get_hash(&self) -> u64 {
        use std::hash::Hash;
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

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
    type Error: 'static + Send + Sync;

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
