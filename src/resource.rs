// Copyright (c) 2016-2026 Greg Steffensen and the http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::platform::FileExt;
use crate::served_dir::default_hasher;
use crate::FileInfo;
use futures_core::Stream;
use futures_util::stream;
use http::header::{HeaderMap, HeaderValue};
use http::HeaderName;
use std::fs::File;
use std::io::{Error, ErrorKind};
use std::ops::Range;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use crate::etag::ETag;
use crate::SerdirError;
use http::{Request, Response, StatusCode};

// This stream breaks apart the file into chunks of at most CHUNK_SIZE. This size is
// a tradeoff between memory usage and thread handoffs.
const CHUNK_SIZE: u64 = 65_536;

#[derive(Debug, Clone)]
enum ResourceContent {
    File(Arc<std::fs::File>),
    Bytes(bytes::Bytes),
}

impl ResourceContent {
    fn for_file(file: Arc<std::fs::File>) -> Self {
        Self::File(file)
    }

    fn for_bytes(bytes: bytes::Bytes) -> Self {
        Self::Bytes(bytes)
    }

    fn read_range(&self, range: Range<u64>) -> Result<bytes::Bytes, crate::IOError> {
        let start = usize::try_from(range.start)
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "range start cannot fit in usize"))?;
        let end = usize::try_from(range.end)
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "range end cannot fit in usize"))?;
        if start > end {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "range start is greater than range end",
            ));
        }
        let len = end - start;

        match self {
            Self::File(f) => f.read_range(len, range.start).map(Into::into),
            Self::Bytes(bytes) => {
                if end > bytes.len() {
                    return Err(Error::new(
                        ErrorKind::UnexpectedEof,
                        "range exceeds bytes length",
                    ));
                }
                Ok(bytes.slice(start..end))
            }
        }
    }
}

/// HTTP entity created from a [`std::fs::File`] which reads the file chunk-by-chunk within
/// a [`tokio::task::block_in_place`] closure.
///
/// Expects to be served from a tokio threadpool.
///
/// Most serdir users will not need to use `Resource` directly, and will
/// instead use higher-level APIs like
/// [`ServedDir::get_response`](crate::ServedDir::get_response). However,
/// `Resource` is available for advanced use cases, like manually modifying
/// the etag or response headers before serving.
///
/// A `Resource` references its file via an open [`File`] handle, not a [`Path`], so it will be
/// resilient against attempts to delete or rename its file as long as it exists. Reading data
/// from a `Resource` does not affects its file position, so it is [`Sync`] and can, if needed,
/// be used to serve many requests at once (though this crate's own request handling code doesn't
/// attempt that).
///
/// However, file metadata such as the length, last modified time and ETag are cached when the
/// `Resource` is created, so if the underlying file is written to after the `Resource` is
/// created, it's possible for it to return a corrupt response, with an ETag or last modified time
/// that doesn't match the served contents. As such, while it is safe to replace existing static
/// content with new files at runtime, users of this crate should do that by moving files or
/// directories, and not by writing to existing static content files after the server has started.
///
/// # Example
///
/// ```
/// # use http::{Request, StatusCode};
/// # use http::header::CONTENT_TYPE;
/// # use serdir::{Body, ResourceBuilder};
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let tmp = tempfile::NamedTempFile::new()?;
/// std::fs::write(tmp.path(), b"Hello, world!")?;
///
/// let entity = ResourceBuilder::for_file(tmp.path())?
///     .content_type(http::header::HeaderValue::from_static("text/plain"))
///     .build();
/// let request = Request::get("/").body(())?;
/// let response: http::Response<Body> = entity.serve_request(&request, StatusCode::OK);
///
/// assert_eq!(response.status(), StatusCode::OK);
/// assert_eq!(response.headers().get(CONTENT_TYPE).unwrap(), "text/plain");
/// # Ok(())
/// # }
/// ```
pub struct ResourceBuilder {
    len: u64,
    mtime: SystemTime,
    content: ResourceContent,
    headers: HeaderMap,
    etag: Option<ETag>,
}

impl ResourceBuilder {
    /// Creates a new `ResourceBuilder` that serves the file at the given path.
    pub fn for_file(path: impl AsRef<Path>) -> Result<Self, SerdirError> {
        let path = path.as_ref();
        let file = File::open(path)?;
        let file_info = FileInfo::open_file(path, &file)?;

        let mut reader = &file;
        let etag: Option<ETag> = default_hasher(&mut reader)?.map(Into::into);

        Ok(Self {
            len: file_info.len(),
            mtime: file_info.mtime(),
            content: ResourceContent::for_file(Arc::new(file)),
            headers: HeaderMap::new(),
            etag,
        })
    }

    /// Creates a new `ResourceBuilder` backed by in-memory bytes.
    ///
    /// The provided `mtime` is used for conditional request handling.
    /// If ETag hashing fails unexpectedly, ETag is omitted.
    pub fn for_bytes(bytes: impl Into<bytes::Bytes>, mtime: SystemTime) -> Self {
        let bytes = bytes.into();
        let mut reader = std::io::Cursor::new(bytes.clone());
        let etag: Option<ETag> = default_hasher(&mut reader).ok().flatten().map(Into::into);

        Self {
            len: crate::as_u64(bytes.len()),
            mtime,
            content: ResourceContent::for_bytes(bytes),
            headers: HeaderMap::new(),
            etag,
        }
    }

    /// Creates a new `ResourceBuilder` backed by UTF-8 string content.
    pub fn for_str(value: &str, mtime: SystemTime) -> Self {
        Self::for_bytes(value.as_bytes().to_vec(), mtime)
    }

    /// Adds or replaces one response header.
    pub fn header(mut self, name: HeaderName, value: impl Into<HeaderValue>) -> Self {
        self.headers.insert(name, value.into());
        self
    }

    /// Convenience method for setting the `Content-Type` response header.
    pub fn content_type(self, value: impl Into<HeaderValue>) -> Self {
        self.header(http::header::CONTENT_TYPE, value)
    }

    /// Builds a `Resource`.
    pub fn build(self) -> Resource {
        Resource {
            len: self.len,
            mtime: self.mtime,
            content: self.content,
            headers: self.headers,
            etag: self.etag,
        }
    }
}

/// A static HTTP resource with body content, headers, ETag and last-modified metadata.
#[derive(Debug, Clone)]
pub struct Resource {
    len: u64,
    mtime: SystemTime,
    content: ResourceContent,
    /// The HTTP response headers to include when serving this file
    pub headers: HeaderMap,
    /// The ETag for the file, if any
    pub etag: Option<ETag>,
}

impl Resource {
    /// Creates a new Resource, with presupplied metadata and a pre-opened file.
    ///
    /// The `headers` value specifies HTTP response headers that should be included whenever serving
    /// this file, such as the `Content-Type`, `Encoding` and `Vary` headers.
    ///
    /// This is an optimization for the case where the caller has already opened the file and read
    /// the metadata from the opened file handle.  Note that on Windows, this still may perform a
    /// blocking file operation, so it should still be wrapped in [`tokio::task::block_in_place`].
    ///
    /// It is the caller's responsibility to ensure that the the path, file and metadata all refer
    /// to the same file- the metadata should be retrieved from the opened file handle to ensure
    /// this.
    pub(crate) fn for_file_with_metadata(
        file: Arc<std::fs::File>,
        file_info: FileInfo,
        headers: HeaderMap,
        etag: Option<ETag>,
    ) -> Self {
        debug_assert!(file.metadata().unwrap().is_file());

        Resource {
            len: file_info.len(),
            mtime: file_info.mtime(),
            headers,
            content: ResourceContent::for_file(file),
            etag,
        }
    }

    /// Returns the value of the response header with the given name, if it exists.
    pub fn header(&self, name: &HeaderName) -> Option<&HeaderValue> {
        self.headers.get(name)
    }

    /// Serves this entity as a response to the given request.
    ///
    /// Consumes the `Resource` and returns an HTTP response.
    pub fn serve_request<B, D>(self, req: &Request<B>, status: StatusCode) -> Response<D>
    where
        D: From<crate::Body>,
    {
        crate::serving::serve(self, req.method(), req.headers(), status).map(Into::into)
    }

    /// Returns the length of the file in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Returns true if the file is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the last modification time of the file.
    pub fn mtime(&self) -> SystemTime {
        self.mtime
    }

    /// Reads and returns the complete content as bytes.
    ///
    /// For file-backed resources, this method performs blocking disk IO.
    pub fn read_bytes(&self) -> Result<bytes::Bytes, std::io::Error> {
        match &self.content {
            ResourceContent::File(f) => {
                tokio::task::block_in_place(|| f.read_range(self.len as usize, 0).map(Into::into))
            }
            ResourceContent::Bytes(bytes) => Ok(bytes.clone()),
        }
    }

    // Reads the bytes of the given range from this entity.
    pub(crate) fn get_range(
        &self,
        range: Range<u64>,
    ) -> Pin<Box<dyn Stream<Item = Result<bytes::Bytes, crate::IOError>> + Send>> {
        let content = self.content.clone();
        let stream = stream::unfold((range, content), move |(left, content)| async move {
            if left.start == left.end {
                return None;
            }
            let chunk_size = std::cmp::min(CHUNK_SIZE, left.end - left.start);
            let chunk_range = left.start..left.start + chunk_size;
            let result = tokio::task::block_in_place(|| content.read_range(chunk_range));
            let next = match &result {
                Ok(bytes) => left.start + crate::as_u64(bytes.len())..left.end,
                Err(_) => left,
            };
            Some((result, (next, content)))
        });
        Box::pin(stream)
    }

    pub(crate) fn add_headers(&self, h: &mut HeaderMap) {
        h.extend(self.headers.iter().map(|(k, v)| (k.clone(), v.clone())));
    }

    pub(crate) fn etag(&self) -> Option<HeaderValue> {
        self.etag.map(|e| e.into())
    }

    pub(crate) fn last_modified(&self) -> Option<SystemTime> {
        Some(self.mtime)
    }
}

#[cfg(test)]
mod tests {
    use super::ResourceBuilder;
    use bytes::Bytes;
    use futures_core::Stream;
    use futures_util::stream::TryStreamExt;
    use http::header::HeaderValue;
    use std::fs::File;
    use std::io::{Seek, SeekFrom, Write};
    use std::pin::Pin;
    use std::time::Duration;
    use std::time::SystemTime;

    async fn to_bytes(
        s: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>,
    ) -> Result<Bytes, std::io::Error> {
        let concat = s
            .try_fold(Vec::new(), |mut acc, item| async move {
                acc.extend(&item[..]);
                Ok(acc)
            })
            .await?;
        Ok(concat.into())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn basic() {
        tokio::spawn(async move {
            let tmp = tempfile::tempdir().unwrap();
            let p = tmp.path().join("f");
            let mut f = File::create(&p).unwrap();
            f.write_all(b"asdf").unwrap();
            let crf1 = ResourceBuilder::for_file(&p)
                .unwrap()
                .content_type(HeaderValue::from_static("text/plain"))
                .build();
            assert_eq!(4, crf1.len());
            assert_eq!(
                Some("text/plain"),
                crf1.header(&http::header::CONTENT_TYPE)
                    .map(|v| v.to_str().unwrap())
            );
            assert!(crf1.header(&http::header::CONTENT_LANGUAGE).is_none());

            // Test returning part/all of the stream.
            assert_eq!(
                &to_bytes(crf1.get_range(0..4)).await.unwrap().as_ref(),
                b"asdf"
            );
            assert_eq!(
                &to_bytes(crf1.get_range(1..3)).await.unwrap().as_ref(),
                b"sd"
            );

            // A Resource constructed from a modified file should have a different etag.
            f.write_all(b"jkl;").unwrap();
            let crf2 = ResourceBuilder::for_file(&p).unwrap().build();
            assert_eq!(8, crf2.len());
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn etag() {
        tokio::spawn(async move {
            let tmp = tempfile::tempdir().unwrap();
            let p = tmp.path().join("f");
            let mut f = File::create(&p).unwrap();

            f.write_all(b"first value").unwrap();
            let crf1 = ResourceBuilder::for_file(&p).unwrap().build();
            let etag1 = crf1.etag().expect("etag1 was None");
            assert_eq!(r#""928c5c44c1689e3f""#, etag1.to_str().unwrap());

            f.seek(SeekFrom::Start(0)).unwrap();
            f.set_len(0).unwrap();

            f.write_all(b"another value").unwrap();
            let crf2 = ResourceBuilder::for_file(&p).unwrap().build();
            let etag2 = crf2.etag().expect("etag2 was None");
            assert_eq!(r#""d712812bea51c2cf""#, etag2.to_str().unwrap());

            assert_eq!(
                Some(etag1),
                crf1.etag(),
                "CRF etag changed after file modification (should be immutable)"
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn last_modified() {
        tokio::spawn(async move {
            let tmp = tempfile::tempdir().unwrap();
            let p = tmp.path().join("f");
            let mut f = File::create(&p).unwrap();
            f.write_all(b"blahblah").unwrap();

            let crf1 = ResourceBuilder::for_file(&p).unwrap().build();
            let expected = f.metadata().unwrap().modified().ok();
            assert_eq!(expected, crf1.last_modified());

            let fifty_hours = Duration::from_secs(50 * 60 * 60);
            let t = SystemTime::UNIX_EPOCH + fifty_hours;
            f.set_modified(t).unwrap();

            let crf2 = ResourceBuilder::for_file(&p).unwrap().build();
            assert_eq!(Some(t), crf2.last_modified());

            assert_eq!(
                expected,
                crf1.last_modified(),
                "CRF last_modified value changed after file modification (should be immutable)"
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn truncate_race() {
        tokio::spawn(async move {
            let tmp = tempfile::tempdir().unwrap();
            let p = tmp.path().join("f");
            let mut f = File::create(&p).unwrap();
            f.write_all(b"asdf").unwrap();

            let crf = ResourceBuilder::for_file(&p).unwrap().build();
            assert_eq!(4, crf.len());
            f.set_len(3).unwrap();

            // Test that
            let e = to_bytes(crf.get_range(0..4)).await.unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_serve_request() {
        use http_body_util::BodyExt;

        tokio::spawn(async move {
            let tmp = tempfile::tempdir().unwrap();
            let p = tmp.path().join("f");
            let mut f = File::create(&p).unwrap();
            f.write_all(b"hello world").unwrap();

            let entity = ResourceBuilder::for_file(&p).unwrap().build();
            let req = http::Request::get("/").body(()).unwrap();

            let res: http::Response<crate::Body> = entity.serve_request(&req, http::StatusCode::OK);
            assert_eq!(res.status(), http::StatusCode::OK);
            let body = res.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(body, "hello world");
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn for_bytes_roundtrip() {
        tokio::spawn(async move {
            let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(42);
            let entity = ResourceBuilder::for_bytes(Bytes::from_static(b"hello bytes"), mtime)
                .content_type(HeaderValue::from_static("text/plain"))
                .build();
            assert_eq!(entity.len(), 11);
            assert_eq!(entity.mtime(), mtime);
            assert_eq!(
                entity.read_bytes().unwrap(),
                Bytes::from_static(b"hello bytes")
            );
            assert!(entity.etag().is_some());
            assert_eq!(
                &to_bytes(entity.get_range(6..11)).await.unwrap().as_ref(),
                b"bytes"
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn builder_content_type_sets_header() {
        tokio::spawn(async move {
            let entity = ResourceBuilder::for_str(
                "content",
                SystemTime::UNIX_EPOCH + Duration::from_secs(1),
            )
            .content_type(HeaderValue::from_static("text/plain"))
            .build();
            assert_eq!(
                entity.header(&http::header::CONTENT_TYPE).unwrap(),
                "text/plain"
            );
        })
        .await
        .unwrap();
    }
}
