// Copyright (c) 2016-2026 Greg Steffensen and the http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Tools for serving directories of static files over HTTP using
//! [http](http://crates.io/crates/http) and
//! [tokio](https://crates.io/crates/tokio).
//! Works well with [hyper](https://crates.io/crates/hyper) 1.x and
//! [tower](https://crates.io/crates/tower).
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
//!   - [`tower::Service`](https://docs.rs/tower/latest/tower/trait.Service.html)
//!   - [`tower::Layer`](https://docs.rs/tower/latest/tower/trait.Layer.html) (i.e. fallback to a `tower::Service` if no file is found)
//!   - [`hyper::service::Service`](https://docs.rs/hyper/latest/hyper/service/trait.Service.html)
//!
//! This crate is gratefully derived from [http-serve](https://github.com/scottlamb/http-serve/).
//!
//! # Examples
//!
//! As seen below, the main entry point for this crate is [`ServedDir`], which is created
//! via a [`ServedDirBuilder`].
//!
//! Serve files via Hyper:
//!
//! ```no_run
//! # #[cfg(feature = "hyper")]
//! # {
//! use hyper::server::conn;
//! use hyper_util::rt::TokioIo;
//! use serdir::ServedDir;
//! use serdir::compression::BrotliLevel;
//! use std::net::{Ipv4Addr, SocketAddr};
//! use tokio::net::TcpListener;
//!
//! #[tokio::main]
//! async fn main() {
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
//!             if let Err(err) = conn::http1::Builder::new()
//!                 .serve_connection(io, service)
//!                 .await
//!             {
//!                 eprintln!("connection error: {err}");
//!             }
//!         });
//!     }
//! }
//! # }
//! ```
//! Serve files via native `ServedDir` API:
//!
//! ```no_run
//! # #[cfg(feature = "hyper")]
//! # {
//! use hyper::server::conn;
//! use hyper::service::service_fn;
//! use hyper_util::rt::TokioIo;
//! use serdir::ServedDir;
//! use std::net::{Ipv4Addr, SocketAddr};
//! use std::sync::Arc;
//! use tokio::net::TcpListener;
//!
//! #[tokio::main]
//! async fn main() {
//!     let served_dir = ServedDir::builder("./static")
//!             .unwrap()
//!             .append_index_html(true)
//!             .build();
//!     let served_dir = Arc::new(served_dir);
//!
//!     let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 1337));
//!     let listener = TcpListener::bind(addr).await.unwrap();
//!
//!     loop {
//!         let (tcp, _) = listener.accept().await.unwrap();
//!         let served_dir = Arc::clone(&served_dir);
//!         let service = service_fn(move |req| {
//!             let served_dir = Arc::clone(&served_dir);
//!             async move { served_dir.get_response(&req).await }
//!         });
//!         tokio::spawn(async move {
//!             let io = TokioIo::new(tcp);
//!             if let Err(err) = conn::http1::Builder::new()
//!                 .serve_connection(io, service)
//!                 .await
//!             {
//!                 eprintln!("connection error: {err}");
//!             }
//!         });
//!     }
//! }
//! # }
//! ```
//!
//! ## Logging
//!
//! This crate provides basic logs for debugging purposes via the `log` crate.
//! Errors are logged at the `ERROR` level, and details of all served requests,
//! including path, response status and duration, are logged at the `TRACE`
//! level.

#![deny(missing_docs, clippy::print_stderr, clippy::print_stdout)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(clippy::result_large_err)]

use std::error::Error;
use std::fmt::Display;
use std::fs::File;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Error as IOError;
use std::io::ErrorKind;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use http::StatusCode;

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
    NotFound(Option<Resource>),
    /// Error compressing file with Brotli.
    CompressionError(String, IOError),
    /// The input path is invalid (e.g., contains NUL bytes or ".." segments).
    InvalidPath(String),
    /// An unexpected I/O error occurred.
    IOError(IOError),
}

impl SerdirError {
    /// Constructor for ConfigError variant, logs a config error.
    pub fn config_error(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        let err = SerdirError::ConfigError(msg);
        log::error!("{err}");
        err
    }

    /// Constructor for IsDirectory variant.
    pub fn is_directory(path: PathBuf) -> Self {
        SerdirError::IsDirectory(path)
    }

    /// Constructor for NotFound variant.
    pub fn not_found(resource: Option<Resource>) -> Self {
        SerdirError::NotFound(resource)
    }

    /// Constructor for CompressionError variant, logs a compression error.
    pub fn compression_error(msg: impl Into<String>, io_err: IOError) -> Self {
        let msg = msg.into();
        SerdirError::CompressionError(msg, io_err)
    }

    /// Constructor for InvalidPath variant, logs an invalid path error.
    pub fn invalid_path(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        SerdirError::InvalidPath(msg)
    }

    /// Constructor for IOError variant.
    pub fn io_error(err: IOError) -> Self {
        SerdirError::IOError(err)
    }

    /// Returns the HTTP status code most appropriate for this error.
    pub fn status_code(&self) -> StatusCode {
        match self {
            SerdirError::NotFound(_) | SerdirError::IsDirectory(_) => StatusCode::NOT_FOUND,
            SerdirError::InvalidPath(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl Display for SerdirError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SerdirError::ConfigError(msg) => write!(f, "Config error: {msg}"),
            SerdirError::IsDirectory(path) => {
                write!(f, "Path is a directory: {}", path.display())
            }
            SerdirError::NotFound(_) => write!(f, "File not found"),
            SerdirError::InvalidPath(msg) => write!(f, "Invalid path: {msg}"),
            SerdirError::IOError(err) => write!(f, "IO error: {err}"),
            SerdirError::CompressionError(msg, err) => {
                write!(f, "Compression error: {msg} (I/O error: {err})")
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
            SerdirError::not_found(None)
        } else {
            SerdirError::io_error(err)
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
mod platform;
mod range;
mod resource;
/// Logic for serving entities.
mod serving;

pub use crate::body::Body;
pub use crate::etag::ETag;
pub use crate::resource::{Resource, ResourceBuilder};
pub use crate::served_dir::{ServedDir, ServedDirBuilder};

/// Function pointer type used to calculate ETag hash values from opened resources.
///
/// The function should return a 64-bit hash of the full resource contents,
/// or `None` to suppress ETag emission for that resource.
pub type ResourceHasher = fn(&mut dyn Read) -> Result<Option<u64>, std::io::Error>;

/// A callback that can convert certain errors into servable resources.
///
/// The second argument is the original path passed to [`ServedDir::get`].
pub type ErrorHandler =
    dyn Fn(SerdirError, &str) -> Result<Resource, SerdirError> + Send + Sync + 'static;

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
            return Err(SerdirError::is_directory(path.to_path_buf()));
        }
        // if it's not a directory and not a file, we don't want to handle it
        // and behave as if it doesn't exist
        if !file_type.is_file() {
            return Err(SerdirError::not_found(None));
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
