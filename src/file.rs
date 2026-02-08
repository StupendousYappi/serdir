// Copyright (c) 2020 The http-serve developers
// Copyright (c) 2026 Greg Steffensen
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
use std::ops::Range;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use crate::etag::ETag;
use crate::{Entity, ServeFilesError};
use http::{Request, Response, StatusCode};

// This stream breaks apart the file into chunks of at most CHUNK_SIZE. This size is
// a tradeoff between memory usage and thread handoffs.
static CHUNK_SIZE: u64 = 65_536;

/// HTTP entity created from a [`std::fs::File`] which reads the file chunk-by-chunk within
/// a [`tokio::task::block_in_place`] closure.
///
/// Expects to be served from a tokio threadpool.
///
/// Most serve-files users will not need to use `FileEntity` directly, and will instead use
/// higher-level APIs like [`ServedDir::get_response`]. However, `FileEntity` is available for
/// advanced use cases, like manually modifying the etag or response headers before serving.
///
/// A [`FileEntity`] references its file via an open [`File`] handle, not a [`Path`], so it will be
/// resilient against attempts to delete or rename its file as long as it exists. Reading data
/// from a [`FileEntity`] does not affects its file position, so it is [`Sync`] and can, if needed,
/// be used to serve many requests at once (though this crate's own request handling code doesn't
/// attempt that).
///
/// However, file metadata such as the length, last modified time and ETag are cached when the
/// [`FileEntity`] is created, so if the underlying file is written to after the [`FileEntity`] is
/// created, it's possible for it to return a corrupt response, with an ETag or last modified time
/// that doesn't match the served contents. As such, while it is safe to replace existing static
/// content with new files at runtime, users of this crate should do that by moving files or
/// directories, and not by writing to existing static content files after the server has started.
///
/// # Example
///
/// ```
/// # use http::{Request, StatusCode};
/// # use http::header::{HeaderMap, CONTENT_TYPE};
/// # use serve_files::{Body, FileEntity};
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let tmp = tempfile::NamedTempFile::new()?;
/// std::fs::write(tmp.path(), b"Hello, world!")?;
///
/// let mut headers = HeaderMap::new();
/// headers.insert(CONTENT_TYPE, "text/plain".parse()?);
///
/// let entity = FileEntity::new(tmp.path(), headers)?;
/// let request = Request::get("/").body(())?;
/// let response: http::Response<Body> = entity.serve_request(&request, StatusCode::OK);
///
/// assert_eq!(response.status(), StatusCode::OK);
/// assert_eq!(response.headers().get(CONTENT_TYPE).unwrap(), "text/plain");
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct FileEntity {
    len: u64,
    mtime: SystemTime,
    /// An open file handle to the file being served.
    pub f: Arc<std::fs::File>,
    /// The HTTP response headers to include when serving this file
    pub headers: HeaderMap,
    /// The ETag for the file, if any
    pub etag: Option<ETag>,
}

impl FileEntity {
    /// Creates a new FileEntity that serves the file at the given path.
    ///
    /// The `headers` value specifies HTTP response headers that should be included whenever serving
    /// this file, such as the `Content-Type`, `Encoding` and `Vary` headers.
    ///
    /// This function performs blocking disk IO- calls to it from an async context should be wrapped in a
    /// call to [`tokio::task::block_in_place`] to avoid blocking the tokio reactor thread. Attempts
    /// to read file data via [`FileEntity::serve_request`] will also block, and should also be wrapped
    /// in [`tokio::task::block_in_place`] if called from an async context.
    pub fn new(path: impl AsRef<Path>, headers: HeaderMap) -> Result<Self, ServeFilesError> {
        let path = path.as_ref();
        let file = File::open(path)?;
        let file_info = FileInfo::open_file(path, &file)?;

        let etag: Option<ETag> = default_hasher(&file)?.map(Into::into);

        FileEntity::new_with_metadata(Arc::new(file), file_info, headers, etag)
    }

    /// Creates a new FileEntity, with presupplied metadata and a pre-opened file.
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
    pub(crate) fn new_with_metadata(
        file: Arc<std::fs::File>,
        file_info: FileInfo,
        headers: HeaderMap,
        etag: Option<ETag>,
    ) -> Result<Self, ServeFilesError> {
        debug_assert!(file.metadata().unwrap().is_file());

        Ok(FileEntity {
            len: file_info.len(),
            mtime: file_info.mtime(),
            headers,
            f: file,
            etag,
        })
    }

    /// Returns the value of the response header with the given name, if it exists.
    pub fn header(&self, name: &HeaderName) -> Option<&HeaderValue> {
        self.headers.get(name)
    }

    /// Serves this entity as a response to the given request.
    ///
    /// Consumes the `FileEntity` and returns an HTTP response.
    pub fn serve_request<B, D>(self, req: &Request<B>, status: StatusCode) -> Response<D>
    where
        D: From<crate::Body>,
    {
        crate::serving::serve(self, req, status).map(Into::into)
    }
}

impl Entity for FileEntity {
    type Data = bytes::Bytes;
    type Error = crate::IOError;

    fn len(&self) -> u64 {
        self.len
    }

    // Reads the bytes of the given range from this entity's file.
    fn get_range(
        &self,
        range: Range<u64>,
    ) -> Pin<Box<dyn Stream<Item = Result<Self::Data, Self::Error>> + Send + Sync>> {
        let stream = stream::unfold((range, Arc::clone(&self.f)), move |(left, f)| async {
            if left.start == left.end {
                return None;
            }
            let chunk_size = std::cmp::min(CHUNK_SIZE, left.end - left.start) as usize;
            Some(tokio::task::block_in_place(move || {
                match f.read_range(chunk_size, left.start) {
                    Err(e) => (Err(e), (left, f)),
                    Ok(v) => {
                        let bytes_read = v.len();
                        (Ok(v.into()), (left.start + bytes_read as u64..left.end, f))
                    }
                }
            }))
        });
        let _: &dyn Stream<Item = Result<Self::Data, Self::Error>> = &stream;
        Box::pin(stream)
    }

    fn add_headers(&self, h: &mut HeaderMap) {
        h.extend(self.headers.iter().map(|(k, v)| (k.clone(), v.clone())));
    }

    fn etag(&self) -> Option<HeaderValue> {
        self.etag.map(|e| e.into())
    }

    fn last_modified(&self) -> Option<SystemTime> {
        Some(self.mtime)
    }
}

#[cfg(test)]
mod tests {
    use super::Entity;
    use super::FileEntity;
    use bytes::Bytes;
    use futures_core::Stream;
    use futures_util::stream::TryStreamExt;
    use http::header::HeaderMap;
    use std::fs::File;
    use std::io::{Seek, SeekFrom, Write};
    use std::pin::Pin;
    use std::time::Duration;
    use std::time::SystemTime;

    type CRF = FileEntity;

    async fn to_bytes(
        s: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>,
    ) -> Result<Bytes, std::io::Error> {
        let concat = Pin::from(s)
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
            let mut headers = HeaderMap::new();
            headers.insert(http::header::CONTENT_TYPE, "text/plain".parse().unwrap());
            let crf1 = CRF::new(&p, headers).unwrap();
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

            // A FileEntity constructed from a modified file should have a different etag.
            f.write_all(b"jkl;").unwrap();
            let crf2 = CRF::new(&p, HeaderMap::new()).unwrap();
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
            let crf1 = CRF::new(&p, HeaderMap::new()).unwrap();
            let etag1 = crf1.etag().expect("etag1 was None");
            assert_eq!(r#""928c5c44c1689e3f""#, etag1.to_str().unwrap());

            f.seek(SeekFrom::Start(0)).unwrap();
            f.set_len(0).unwrap();

            f.write_all(b"another value").unwrap();
            let crf2 = CRF::new(&p, HeaderMap::new()).unwrap();
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

            let crf1 = CRF::new(&p, HeaderMap::new()).unwrap();
            let expected = f.metadata().unwrap().modified().ok();
            assert_eq!(expected, crf1.last_modified());

            let fifty_hours = Duration::from_secs(50 * 60 * 60);
            let t = SystemTime::UNIX_EPOCH + fifty_hours;
            f.set_modified(t).unwrap();

            let crf2 = CRF::new(&p, HeaderMap::new()).unwrap();
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

            let crf = CRF::new(&p, HeaderMap::new()).unwrap();
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

            let entity = CRF::new(&p, HeaderMap::new()).unwrap();
            let req = http::Request::get("/").body(()).unwrap();

            let res: http::Response<crate::Body> = entity.serve_request(&req, http::StatusCode::OK);
            assert_eq!(res.status(), http::StatusCode::OK);
            let body = res.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(body, "hello world");
        })
        .await
        .unwrap();
    }
}
