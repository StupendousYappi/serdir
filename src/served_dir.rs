//! Serves files from a local directory.

#[cfg(feature = "runtime-compression")]
use std::sync::Arc;
use std::{collections::HashMap, io::ErrorKind, path::PathBuf};

use crate::compression::{CompressionStrategy, CompressionSupport, MatchedFile};

#[cfg(feature = "runtime-compression")]
use crate::brotli_cache::BrotliCache;

use crate::{FileEntity, ServeFilesError};
use bytes::Bytes;
use http::{header, HeaderMap, HeaderValue};
use std::io::Error as IOError;

/// Returns `FileEntity` values for file paths within a directory.
pub struct ServedDir {
    compression_strategy: CompressionStrategy,
    dirpath: PathBuf,
    strip_prefix: Option<String>,
    known_extensions: Option<HashMap<String, HeaderValue>>,
    default_content_type: HeaderValue,
    common_headers: HeaderMap,
    append_index_html: bool,
}

static OCTET_STREAM: HeaderValue = HeaderValue::from_static("application/octet-stream");

impl ServedDir {
    /// Returns a builder for `ServedDir`.
    pub fn builder(path: impl Into<PathBuf>) -> Result<ServedDirBuilder, ServeFilesError> {
        ServedDirBuilder::new(path.into())
    }

    /// Returns a `FileEntity` for the given path and request headers.
    ///
    /// The `Accept-Encoding` request header is used to determine whether to serve the file
    /// gzipped if possible. If gzip encoding is requested, and `auto_gzip` is enabled, this
    /// method will look for a file with the same name but a `.gz` extension. If found, it will
    /// serve that file instead of the primary file. If gzip encoding is not requested,
    /// `auto_gzip` is disabled, or the file is not found, the primary file will be served.
    ///
    /// This method will return an error with kind `ErrorKind::NotFound` if the file is not found.
    /// It will return an error with kind `ErrorKind::InvalidInput` if the path is invalid.
    pub async fn get(
        &self,
        path: &str,
        req_hdrs: &HeaderMap,
    ) -> Result<FileEntity<Bytes>, ServeFilesError> {
        let path = match self.strip_prefix.as_deref() {
            Some(prefix) if path == prefix => ".",
            Some(prefix) => path.strip_prefix(prefix).ok_or(ServeFilesError::NotFound)?,
            None => path,
        };

        self.validate_path(path)?;

        let preferred = CompressionSupport::detect(req_hdrs);
        let full_path = self.dirpath.join(path);

        let res = self.find_file(full_path.clone(), preferred).await;
        let matched_file = match res {
            Ok(mf) => mf,
            Err(ServeFilesError::IsDirectory(_)) if self.append_index_html => {
                let index_path = full_path.join("index.html");
                self.find_file(index_path, preferred).await?
            }
            Err(e) => return Err(e),
        };
        let content_type: HeaderValue = self.get_content_type(&matched_file.extension);
        let mut headers = self.common_headers.clone();
        headers.insert(http::header::CONTENT_TYPE, content_type);

        // Add `Content-Encoding` and `Vary` headers for the encoding to `hdrs`.
        if let Some(value) = matched_file.content_encoding.get_header_value() {
            headers.insert(header::CONTENT_ENCODING, value);
        }
        if matches!(self.compression_strategy, CompressionStrategy::Static) {
            headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        }

        matched_file.into_file_entity(headers)
    }

    async fn find_file(
        &self,
        path: impl Into<PathBuf>,
        preferred: CompressionSupport,
    ) -> Result<MatchedFile, ServeFilesError> {
        let path = path.into();
        let strategy = self.compression_strategy.clone();
        tokio::task::spawn_blocking(move || strategy.find_file(path, preferred))
            .await
            .map_err(|e: tokio::task::JoinError| IOError::new(ErrorKind::Other, e))?
    }

    fn get_content_type(&self, extension: &str) -> HeaderValue {
        self.known_extensions
            .as_ref()
            .and_then(|exts| exts.get(extension).cloned())
            .or_else(|| Self::guess_content_type(extension))
            .unwrap_or_else(|| self.default_content_type.clone())
    }

    #[cfg(feature = "mime_guess")]
    fn guess_content_type(ext: &str) -> Option<HeaderValue> {
        mime_guess::from_ext(ext)
            .first_raw()
            .map(|s| HeaderValue::from_str(s).unwrap())
    }

    #[cfg(not(feature = "mime_guess"))]
    fn guess_content_type(ext: &str) -> Option<HeaderValue> {
        let guess = match ext {
            "html" => Some("text/html"),
            "htm" => Some("text/html"),
            "hxt" => Some("text/html"),
            "css" => Some("text/css"),
            "js" => Some("text/javascript"),
            "es" => Some("text/javascript"),
            "ecma" => Some("text/javascript"),
            "jsm" => Some("text/javascript"),
            "jsx" => Some("text/javascript"),
            "png" => Some("image/png"),
            "apng" => Some("image/apng"),
            "avif" => Some("image/avif"),
            "gif" => Some("image/gif"),
            "ico" => Some("image/x-icon"),
            "jpeg" => Some("image/jpeg"),
            "jfif" => Some("image/jpeg"),
            "pjpeg" => Some("image/jpeg"),
            "pjp" => Some("image/jpeg"),
            "jpg" => Some("image/jpeg"),
            "svg" => Some("image/svg+xml"),
            "tiff" => Some("image/tiff"),
            "webp" => Some("image/webp"),
            "bmp" => Some("image/bmp"),
            "pdf" => Some("application/pdf"),
            "zip" => Some("application/zip"),
            "gz" => Some("application/gzip"),
            "tar" => Some("application/tar"),
            "bz" => Some("application/x-bzip"),
            "bz2" => Some("application/x-bzip2"),
            "xz" => Some("application/x-xz"),
            "csv" => Some("text/csv"),
            "txt" => Some("text/plain"),
            "text" => Some("text/plain"),
            "log" => Some("text/plain"),
            "md" => Some("text/markdown"),
            "markdown" => Some("text/x-markdown"),
            "mkd" => Some("text/x-markdown"),
            "mp4" => Some("video/mp4"),
            "webm" => Some("video/webm"),
            "mpeg" => Some("video/mpeg"),
            "mpg" => Some("video/mpeg"),
            "mpg4" => Some("video/mp4"),
            "xml" => Some("application/xml"),
            "json" => Some("application/json"),
            "yaml" => Some("application/yaml"),
            "yml" => Some("application/yaml"),
            "toml" => Some("application/toml"),
            "ini" => Some("application/ini"),
            "ics" => Some("text/calendar"),
            "doc" => Some("application/msword"),
            "docx" => {
                Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document")
            }
            "xls" => Some("application/vnd.ms-excel"),
            "xlsx" => Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
            "ppt" => Some("application/vnd.ms-powerpoint"),
            "pptx" => {
                Some("application/vnd.openxmlformats-officedocument.presentationml.presentation")
            }
            _ => None,
        };
        guess.map(|s| HeaderValue::from_str(s).unwrap())
    }

    /// Ensures path is safe: no NUL bytes, not absolute, no `..` segments.
    fn validate_path(&self, path: &str) -> Result<(), ServeFilesError> {
        if memchr::memchr(0, path.as_bytes()).is_some() {
            return Err(ServeFilesError::InvalidPath(
                "path contains NUL byte".to_string(),
            ));
        }
        if path.as_bytes().first() == Some(&b'/') {
            return Err(ServeFilesError::InvalidPath("path is absolute".to_string()));
        }
        let mut left = path.as_bytes();
        loop {
            let next = memchr::memchr(b'/', left);
            let seg = &left[0..next.unwrap_or(left.len())];
            if seg == b".." {
                return Err(ServeFilesError::InvalidPath(
                    "path contains .. segment".to_string(),
                ));
            }
            match next {
                None => break,
                Some(n) => left = &left[n + 1..],
            };
        }
        Ok(())
    }
}

/// A builder for [`ServedDir`].
pub struct ServedDirBuilder {
    dirpath: PathBuf,
    compression_strategy: CompressionStrategy,
    strip_prefix: Option<String>,
    known_extensions: Option<HashMap<String, HeaderValue>>,
    default_content_type: HeaderValue,
    common_headers: HeaderMap,
    append_index_html: bool,
}

impl ServedDirBuilder {
    fn new(dirpath: PathBuf) -> Result<Self, ServeFilesError> {
        if !dirpath.is_dir() {
            let msg = format!("path is not a directory: {}", dirpath.display());
            return Err(ServeFilesError::ConfigError(msg));
        }
        Ok(Self {
            dirpath,
            compression_strategy: CompressionStrategy::Static,
            strip_prefix: None,
            known_extensions: None,
            default_content_type: OCTET_STREAM.clone(),
            common_headers: HeaderMap::new(),
            append_index_html: false,
        })
    }

    /// Sets whether to automatically serve compressed versions of files.
    /// Defaults to `true`.
    pub fn auto_compress(mut self, auto_compress: bool) -> Self {
        self.compression_strategy = if auto_compress {
            CompressionStrategy::Static
        } else {
            CompressionStrategy::None
        };
        self
    }

    /// Enables use of pre-compressed files based on file extensions.
    pub fn static_compression(mut self) -> Self {
        self.compression_strategy = CompressionStrategy::Static;
        self
    }

    /// Appends "/index.html" to directory paths.
    pub fn append_index_html(mut self, append: bool) -> Self {
        self.append_index_html = append;
        self
    }

    /// Enables dynamic compression using Brotli.
    ///
    /// This will compress files on the fly and cache the results.
    ///
    /// # Arguments
    ///
    /// * `cache_size` - The number of files to cache. Must be a power of two and at least 4.
    /// * `compression_level` - The compression level to use. 0 is fastest, 11 is best compression.
    #[cfg(feature = "runtime-compression")]
    pub fn dynamic_compression(mut self, cache_size: u16, compression_level: u8) -> Self {
        let brotli_cache = BrotliCache::builder()
            .cache_size(cache_size)
            .compression_level(compression_level)
            .build();
        let strategy = CompressionStrategy::Dynamic(Arc::new(brotli_cache));
        self.compression_strategy = strategy;
        self
    }

    /// Enables dynamic compression using a pre-built Brotli cache.
    #[cfg(feature = "runtime-compression")]
    pub fn dynamic_compression_with_cache(mut self, cache: BrotliCache) -> Self {
        let strategy = CompressionStrategy::Dynamic(Arc::new(cache));
        self.compression_strategy = strategy;
        self
    }

    /// Sets a prefix to strip from the request path.
    pub fn strip_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.strip_prefix = Some(prefix.into());
        self
    }

    /// Sets a map of file extensions to content types.
    pub fn known_extensions(mut self, extensions: HashMap<String, HeaderValue>) -> Self {
        self.known_extensions = Some(extensions);
        self
    }

    /// Sets the default content type to use when the extension is unknown.
    /// Defaults to `application/octet-stream`.
    pub fn default_content_type(mut self, content_type: HeaderValue) -> Self {
        self.default_content_type = content_type;
        self
    }

    /// Adds a common header to be added to all successful responses.
    ///
    /// The header will added to the `FileEntity` if the `ServedDir` returns one, but
    /// will not be recoded anywhere in an error response.
    pub fn common_header(mut self, name: header::HeaderName, value: HeaderValue) -> Self {
        self.common_headers.insert(name, value);
        self
    }

    /// Builds the [`ServedDir`].
    pub fn build(self) -> ServedDir {
        ServedDir {
            dirpath: self.dirpath,
            compression_strategy: self.compression_strategy,
            strip_prefix: self.strip_prefix,
            known_extensions: self.known_extensions,
            default_content_type: self.default_content_type,
            common_headers: self.common_headers,
            append_index_html: self.append_index_html,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Entity;
    use tempfile::TempDir;

    struct TestContext {
        tmp: TempDir,
        builder: ServedDirBuilder,
    }

    impl TestContext {
        fn write_file(&self, name: &str, contents: &str) {
            std::fs::write(self.tmp.path().join(name), contents.as_bytes())
                .expect("failed to write test file")
        }

        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().to_path_buf();
            Self {
                tmp,
                builder: ServedDir::builder(path).expect("failed to create builder"),
            }
        }
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_get() {
        let context = TestContext::new();
        context.write_file("one.txt", "one");
        context.write_file("two.json", "more content");
        let served_dir = context.builder.build();
        let hdrs = HeaderMap::new();

        let e1 = served_dir.get("one.txt", &hdrs).await.unwrap();
        assert_eq!(e1.len(), 3);
        assert_eq!(
            e1.header(&http::header::CONTENT_TYPE).unwrap(),
            "text/plain"
        );

        let e2 = served_dir.get("two.json", &hdrs).await.unwrap();
        assert_eq!(e2.len(), 12);
        assert_eq!(
            e2.header(&http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_not_found() {
        let context = TestContext::new();
        let served_dir = context.builder.build();
        let hdrs = HeaderMap::new();

        let err = served_dir.get("non-existent.txt", &hdrs).await.unwrap_err();
        assert!(matches!(err, ServeFilesError::NotFound));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_auto_compress() {
        let context = TestContext::new();
        context.write_file("test.txt", "plain text");
        context.write_file("test.txt.gz", "fake gzip content");
        context.write_file("test.txt.br", "fake brotli content");

        // 1. auto_compress enabled (default), Accept-Encoding: gzip
        let served_dir = context.builder.build();
        let mut hdrs = HeaderMap::new();
        hdrs.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("gzip, deflate"),
        );

        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "gzip");
        assert_eq!(e.read_body().await.unwrap(), "fake gzip content");

        // 2. auto_compress enabled (default), Accept-Encoding: br
        let mut hdrs = HeaderMap::new();
        hdrs.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "br");
        assert_eq!(e.read_body().await.unwrap(), "fake brotli content");

        // 3. auto_compress enabled (default), Accept-Encoding: br, gzip (prioritizes br)
        let mut hdrs = HeaderMap::new();
        hdrs.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("br, gzip"),
        );
        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "br");
        assert_eq!(e.read_body().await.unwrap(), "fake brotli content");

        // 4. auto_compress enabled (default), no Accept-Encoding
        let hdrs = HeaderMap::new();
        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert!(e.header(&header::CONTENT_ENCODING).is_none());
        assert_eq!(e.read_body().await.unwrap(), "plain text");

        // 5. auto_compress disabled
        let served_dir = ServedDir::builder(context.tmp.path().to_path_buf())
            .unwrap()
            .auto_compress(false)
            .build();
        let mut hdrs = HeaderMap::new();
        hdrs.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("br, gzip"),
        );

        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert!(e.header(&header::CONTENT_ENCODING).is_none());
        assert_eq!(e.read_body().await.unwrap(), "plain text");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_forbidden_paths() {
        let context = TestContext::new();
        let served_dir = context.builder.build();
        let hdrs = HeaderMap::new();

        // 1. Contains ".."
        let err = served_dir
            .get("include/../etc/passwd", &hdrs)
            .await
            .unwrap_err();
        assert!(matches!(err, ServeFilesError::InvalidPath(msg) if msg.contains("..")));

        // 2. Contains null byte
        let err = served_dir.get("test\0file.txt", &hdrs).await.unwrap_err();
        assert!(matches!(err, ServeFilesError::InvalidPath(msg) if msg.contains("NUL byte")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_strip_prefix() {
        let context = TestContext::new();
        context.write_file("real.txt", "real content");

        let served_dir = context.builder.strip_prefix("/static/").build();
        let hdrs = HeaderMap::new();

        // Should work with the prefix
        let e = served_dir.get("/static/real.txt", &hdrs).await.unwrap();
        assert_eq!(e.read_body().await.unwrap(), "real content");

        // Should fail without the prefix
        let err = served_dir.get("real.txt", &hdrs).await.unwrap_err();
        assert!(matches!(err, ServeFilesError::NotFound));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_content_types() {
        let context = TestContext::new();
        context.write_file("index.html", "html");
        context.write_file("style.css", "css");
        context.write_file("script.js", "js");
        context.write_file("image.webp", "webp");
        context.write_file("unknown.foo", "foo");

        let served_dir = context.builder.build();
        let hdrs = HeaderMap::new();

        let e = served_dir.get("index.html", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "text/html");

        let e = served_dir.get("style.css", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "text/css");

        let e = served_dir.get("script.js", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "text/javascript");

        let e = served_dir.get("image.webp", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "image/webp");

        let e = served_dir.get("unknown.foo", &hdrs).await.unwrap();
        assert_eq!(
            e.header(&header::CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_common_headers() {
        let context = TestContext::new();
        context.write_file("one.txt", "one");

        let served_dir = context
            .builder
            .common_header(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=3600"),
            )
            .common_header(
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                HeaderValue::from_static("*"),
            )
            .build();
        let hdrs = HeaderMap::new();

        let e = served_dir.get("one.txt", &hdrs).await.unwrap();
        assert_eq!(
            e.header(&header::CACHE_CONTROL).unwrap(),
            "public, max-age=3600"
        );
        assert_eq!(e.header(&header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(), "*");
        assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "text/plain");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_unexpected_br_path() {
        let context = TestContext::new();
        let path = context.tmp.path();

        // Create the raw file
        context.write_file("test.txt", "raw content");

        // Create a directory where the .br file is expected
        std::fs::create_dir(path.join("test.txt.br")).unwrap();

        let served_dir = context.builder.build();
        let mut hdrs = HeaderMap::new();
        hdrs.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));

        // Should ignore the directory and serve the raw file
        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert!(e.header(&header::CONTENT_ENCODING).is_none());
        assert_eq!(e.read_body().await.unwrap(), "raw content");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_unexpected_gz_path() {
        let context = TestContext::new();
        let path = context.tmp.path();

        // Create the raw file
        context.write_file("test.txt", "raw content");

        // Create a directory where the .gz file is expected
        std::fs::create_dir(path.join("test.txt.gz")).unwrap();

        let served_dir = context.builder.build();
        let mut hdrs = HeaderMap::new();
        hdrs.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip"));

        // Should ignore the directory and serve the raw file
        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert!(e.header(&header::CONTENT_ENCODING).is_none());
        assert_eq!(e.read_body().await.unwrap(), "raw content");
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_get_directory() {
        let context = TestContext::new();
        let path = context.tmp.path();
        std::fs::create_dir(path.join("subdir")).unwrap();

        let served_dir = context.builder.build();
        let hdrs = HeaderMap::new();

        let err = served_dir.get("subdir", &hdrs).await.unwrap_err();
        assert!(matches!(err, ServeFilesError::IsDirectory(_)));
        if let ServeFilesError::IsDirectory(err_path) = err {
            assert_eq!(err_path, path.join("subdir"));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[cfg(unix)]
    async fn test_served_dir_symlink() {
        use std::os::unix::fs::symlink;
        let context = TestContext::new();
        let path = context.tmp.path();

        // Create the raw file
        context.write_file("target.txt", "target content");

        // Create a symlink pointing to the target file
        symlink(path.join("target.txt"), path.join("link.txt")).unwrap();

        let served_dir = context.builder.build();
        let hdrs = HeaderMap::new();

        // Should follow the symlink and serve the target file
        let e = served_dir.get("link.txt", &hdrs).await.unwrap();
        assert_eq!(e.read_body().await.unwrap(), "target content");
        assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "text/plain");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_append_index_html() {
        let context = TestContext::new();
        let path = context.tmp.path();
        std::fs::create_dir(path.join("subdir")).unwrap();
        std::fs::write(path.join("subdir").join("index.html"), b"index content").unwrap();

        let builder = ServedDir::builder(path.to_path_buf()).unwrap();

        // 1. append_index_html disabled (default)
        let served_dir = builder.build();
        let hdrs = HeaderMap::new();
        let err = served_dir.get("subdir", &hdrs).await.unwrap_err();
        assert!(matches!(err, ServeFilesError::IsDirectory(_)));

        // 2. append_index_html enabled
        let builder = ServedDir::builder(path.to_path_buf()).unwrap();
        let served_dir = builder.append_index_html(true).build();
        let e = served_dir.get("subdir", &hdrs).await.unwrap();
        assert_eq!(e.read_body().await.unwrap(), "index content");
        assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "text/html");

        // 3. append_index_html enabled, but index.html missing
        std::fs::create_dir(path.join("empty_subdir")).unwrap();
        let err = served_dir.get("empty_subdir", &hdrs).await.unwrap_err();
        assert!(matches!(err, ServeFilesError::NotFound));
    }
}
