// Copyright (c) 2016-2026 Greg Steffensen and the http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Helpers for serving HTTP GET and HEAD responses asynchronously with the
//! [http](http://crates.io/crates/http) crate and [tokio](https://crates.io/crates/tokio).
//! Works well with [hyper](https://crates.io/crates/hyper) 1.x and [tower](https://crates.io/crates/tower).
//!
//! # Features
//!
//! - Range requests
//! - Large file support via chunked streaming
//! - Live content changes
//! - ETag header generation and conditional GET requests
//! - Serving files that have been pre-compressed using gzip, brotli or zstd
//! - Cached runtime compression of files using brotli
//! - Content type detection based on filename extensions
//! - Serving directory paths using `index.html` pages
//! - Customizing 404 response content
//! - Support for common Rust web APIs:
//!   -  [`tower::Service`](https://docs.rs/tower/latest/tower/trait.Service.html)
//!   - [`tower::Layer`](https://docs.rs/tower/latest/tower/trait.Layer.html)
//!   - [`hyper::service::Service`](https://docs.rs/hyper/latest/hyper/service/trait.Service.html)
//!
//! This crate is derived from [http-serve](https://github.com/scottlamb/http-serve/).
//!
//! # Examples
//!
//! Serve files via Hyper:
//!
//! ```no_run
//! # #[cfg(feature = "hyper")]
//! # {
//! use hyper_util::rt::TokioIo;
//! use serdir::ServedDir;
//! use serdir::compression::BrotliLevel;
//! use std::net::{Ipv4Addr, SocketAddr};
//! use tokio::net::TcpListener;
//!
//! let runtime = tokio::runtime::Runtime::new().unwrap();
//! runtime.block_on(async {
//!     let service = ServedDir::builder("./static")
//!         .unwrap()
//!         .append_index_html(true)
//!         .cached_compression(BrotliLevel::L5)
//!         .build()
//!         .into_hyper_service();
//!
//!     let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 1337));
//!     let listener = TcpListener::bind(addr).await.unwrap();
//!
//!     loop {
//!         let (tcp, _) = listener.accept().await.unwrap();
//!         let service = service.clone();
//!         tokio::spawn(async move {
//!             let io = TokioIo::new(tcp);
//!             if let Err(err) = hyper::server::conn::http1::Builder::new()
//!                 .serve_connection(io, service)
//!                 .await
//!             {
//!                 eprintln!("connection error: {err}");
//!             }
//!         });
//!     }
//! });
//! # }
//! ```

#![deny(missing_docs, clippy::print_stderr, clippy::print_stdout)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(clippy::result_large_err)]

use bytes::Buf;
use futures_core::Stream;
use http::header::{HeaderMap, HeaderValue};
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

/// An error returned by this crate's public APIs.
#[derive(Debug)]
pub enum SerdirError {
    /// Returned by ServedDirBuilder if configuration is invalid.
    ConfigError(String),
    /// The path is a directory when it was expected to be a file
    IsDirectory(PathBuf),
    /// The requested file was not found.
    NotFound(Option<FileEntity>),
    /// Error compressing file with Brotli.
    CompressionError(String, IOError),
    /// The input path is invalid (e.g., contains NUL bytes or ".." segments).
    InvalidPath(String),
    /// An unexpected I/O error occurred.
    IOError(IOError),
}

impl Display for SerdirError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SerdirError::ConfigError(msg) => write!(f, "{}", msg),
            SerdirError::IsDirectory(path) => {
                write!(f, "Path is a directory: {}", path.display())
            }
            SerdirError::NotFound(_) => write!(f, "File not found"),
            SerdirError::InvalidPath(msg) => write!(f, "Invalid path: {}", msg),
            SerdirError::IOError(err) => write!(f, "I/O error: {}", err),
            SerdirError::CompressionError(msg, err) => {
                write!(f, "Brotli compression error: {} (I/O error: {})", msg, err)
            }
        }
    }
}

impl Error for SerdirError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            SerdirError::IOError(err) => Some(err),
            SerdirError::CompressionError(_, err) => Some(err),
            _ => None,
        }
    }
}

impl From<IOError> for SerdirError {
    fn from(err: IOError) -> Self {
        if err.kind() == ErrorKind::NotFound {
            SerdirError::NotFound(None)
        } else {
            SerdirError::IOError(err)
        }
    }
}

mod body;

mod served_dir;

#[cfg(any(feature = "tower", feature = "hyper"))]
/// Hyper and Tower service integrations.
pub mod integration;

pub mod compression;
mod etag;
mod file;
mod platform;
mod range;
/// Logic for serving entities.
mod serving;

pub use crate::body::Body;
pub use crate::etag::{ETag, FileHasher};
pub use crate::file::FileEntity;
pub use crate::served_dir::{ServedDir, ServedDirBuilder};

#[cfg(feature = "runtime-compression")]
mod brotli_cache;

/// Basic metadata about a particular version of a file, used as a cache key.
#[derive(Hash, Eq, PartialEq, Clone, Copy)]
pub(crate) struct FileInfo {
    path_hash: u64,
    len: u64,
    mtime: SystemTime,
}

impl FileInfo {
    /// Creates a new FileInfo if the path is a file, otherwise returns an error.
    pub(crate) fn open_file(path: &Path, file: &File) -> Result<Self, SerdirError> {
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
            return Err(SerdirError::IsDirectory(path.to_path_buf()));
        }
        // if it's not a directory and not a file, we don't want to handle it
        // and behave as if it doesn't exist
        if !file_type.is_file() {
            return Err(SerdirError::NotFound(None));
        }
        Ok(Self {
            path_hash,
            len: metadata.len(),
            mtime: metadata.modified()?,
        })
    }

    /// Returns the length of the file in bytes.
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// Returns the last modification time of the file.
    pub(crate) fn mtime(&self) -> SystemTime {
        self.mtime
    }

    #[cfg(feature = "runtime-compression")]
    pub(crate) fn for_path(path: &Path) -> Result<Self, SerdirError> {
        let file = File::open(path)?;
        Self::open_file(path, &file)
    }

    #[cfg(feature = "runtime-compression")]
    pub(crate) fn get_hash(&self) -> u64 {
        use std::hash::Hash;
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

/// A reusable, read-only, byte-rangeable HTTP entity for GET and HEAD serving.
/// Must return exactly the same data on every call.
pub(crate) trait Entity: 'static + Send + Sync {
    /// The type of errors produced in [`Self::get_range`] chunks and in the final stream.
    ///
    /// [`BoxError`] is a good choice for most implementations.
    ///
    /// This must be convertable from [`BoxError`] to allow `serdir` to
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

    /// Returns true if the entity's body is empty.
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
