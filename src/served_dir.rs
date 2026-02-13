//! Serves files from a local directory.

use std::convert::Infallible;
use std::fmt::Debug;
use std::fs::File;
use std::path::Path;
use std::{collections::HashMap, path::PathBuf};

#[cfg(feature = "runtime-compression")]
use crate::compression::BrotliLevel;
use crate::compression::{
    CompressionStrategy, CompressionStrategyInner, CompressionSupport, MatchedFile,
    StaticCompression,
};
#[cfg(feature = "hyper")]
use crate::integration::HyperService;
#[cfg(feature = "tower")]
use crate::integration::{TowerLayer, TowerService};

use crate::etag::EtagCache;
use crate::{Body, ETag, FileEntity, FileHasher, FileInfo, SerdirError};
use http::{header, HeaderMap, HeaderValue, Request, Response, StatusCode};

/// Returns [`FileEntity`] values for file paths within a directory.
///
/// A `ServedDir` is created using a [ServedDirBuilder], and must be configured with
/// the path to the static file root directory. When [ServedDir::get] is called,
/// it will attempt to find a file in the root directory with that relative path,
/// detect its content type using the filename extension, and return a [FileEntity]
/// that can be used to serve its content.
///
/// Its behavior can be optionally customized in the following ways:
///
/// - Defining a common prefix that should be stripped from all paths before performing
///   path matching
/// - Defining common headers that should be added to all successful responses
/// - Defining custom mappings from filename extensions to HTTP content type headers
/// - Auto-appending "/index.html" to any path that maps to a directory
/// - Setting a file whose content will be used for 404 (file not found) responses
/// - Response compression settings, including support for using pre-created compressed
///   variants of files (i.e. "index.html.gz" can be served instead of "index.html"),
///   and for performing cached compression of files using Brotli and caching the
///   compressed output for reuse
pub struct ServedDir {
    dirpath: PathBuf,
    compression_strategy: CompressionStrategyInner,
    file_hasher: FileHasher,
    strip_prefix: Option<String>,
    known_extensions: HashMap<String, HeaderValue>,
    default_content_type: HeaderValue,
    common_headers: HeaderMap,
    append_index_html: bool,
    not_found_path: Option<PathBuf>,
    etag_cache: EtagCache,
}

impl Debug for ServedDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let strategy = match self.compression_strategy {
            CompressionStrategyInner::Static(_) => "static",
            CompressionStrategyInner::None => "none",
            #[cfg(feature = "runtime-compression")]
            CompressionStrategyInner::Cached(_) => "cached",
        };

        f.debug_struct("ServedDir")
            .field("dirpath", &self.dirpath)
            .field("strip_prefix", &self.strip_prefix)
            .field("append_index_html", &self.append_index_html)
            .field("not_found_path", &self.not_found_path)
            .field("default_content_type", &self.default_content_type)
            .field("compression_strategy", &strategy)
            .finish()
    }
}

static OCTET_STREAM: HeaderValue = HeaderValue::from_static("application/octet-stream");

/// The default function used to create ETag values by hashing file contents
pub(crate) fn default_hasher(file: &File) -> Result<Option<u64>, std::io::Error> {
    let hash = rapidhash::v3::rapidhash_v3_file(file)?;
    Ok(Some(hash))
}

impl ServedDir {
    /// Returns a builder for `ServedDir`.
    pub fn builder(path: impl Into<PathBuf>) -> Result<ServedDirBuilder, SerdirError> {
        ServedDirBuilder::new(path.into())
    }

    /// Returns the static file root directory used by this `ServedDir`
    pub fn dir(&self) -> &Path {
        &self.dirpath
    }

    /// Returns a `FileEntity` for the given path and request headers.
    ///
    /// This method searches for a file with the given relative path in this instance's
    /// static files directory. If a file is found, a [FileEntity] will be returned that
    /// can be used to serve its content in a HTTP response. This method will also
    /// choose a `Content-Type` header value based on the extension of the matched file.
    ///
    /// If static compression is configured, you can provide pre-compressed variants of
    /// any file by creating a file with the same name as the original, with the appropriate
    /// extension added. For example, if you have a static file with path `settings/index.html`,
    /// this method will look for:
    /// - the file `settings/index.html.zstd` if the client supports zstandard compression
    /// - the file `settings/index.html.br` if the client supports brotli compression
    /// - the file `settings/index.html.gz` if the client supports gzip compression
    ///
    /// Under static compression, this method will always search for those variants in
    /// the above order (zstd, then br, then gz), regardless of the prioritization the
    /// `Accept-Encoding` header provided. If no compatible compressed version can be
    /// found, the original file will be used (even if the `Accept-Encoding` header
    /// explicitly rejects the `identity` encoding).
    ///
    /// If cached compression is configured, this method will perform Brotli compression of matched
    /// files on the fly if the client supports Brotli compression, and will cache the compressed
    /// versions for reuse (Brotli is the only compression algorithm for which runtime compression
    /// is supported). The generated Brotli contents will be cached in unlinked tempfiles that have
    /// no visible path on the filesystem, and will be cleaned up automatically by the operating
    /// system when the application exits, regardless of how it exits (i.e. they will be cleaned up
    /// even if Rust panics in abort mode). Disk usage can be controlled by limiting the maximum
    /// size of files that should be compressed, and the maximum number of compressed files to
    /// cache, using the `CachedCompression` type. The Brotli compression level can
    /// also be configured, allowing you to choose your own balance of compression speed and
    /// compressed size.
    ///
    /// This method will return an error with kind `ErrorKind::NotFound` if the file is not found.
    /// It will return an error with kind `ErrorKind::InvalidInput` if the path is invalid.
    pub async fn get(&self, path: &str, req_hdrs: &HeaderMap) -> Result<FileEntity, SerdirError> {
        let path = match self.strip_prefix.as_deref() {
            Some(prefix) if path == prefix => ".",
            Some(prefix) => path
                .strip_prefix(prefix)
                .ok_or(SerdirError::NotFound(None))?,
            None => path,
        };
        let path = path.strip_prefix('/').unwrap_or(path);

        self.validate_path(path)?;

        let preferred = CompressionSupport::detect(req_hdrs);
        let full_path = self.dirpath.join(path);

        tokio::task::block_in_place(|| {
            let res = self.find_file(&full_path, preferred);
            let matched_file = match res {
                Ok(mf) => mf,
                Err(SerdirError::IsDirectory(_)) if self.append_index_html => {
                    let index_path = full_path.join("index.html");
                    self.find_file(&index_path, preferred)?
                }
                Err(SerdirError::NotFound(_)) if self.not_found_path.is_some() => {
                    let not_found_path = self.not_found_path.as_ref().unwrap();
                    let matched_file = self.find_file(not_found_path, preferred)?;
                    let entity = self.create_entity(matched_file)?;
                    return Err(SerdirError::NotFound(Some(entity)));
                }
                Err(e) => return Err(e),
            };

            self.create_entity(matched_file)
        })
    }

    /// Returns an HTTP response for a request path and headers.
    ///
    /// A convenience wrapper method for [ServedDir::get], converting a HTTP request
    /// into a HTTP response by serving static files using this `ServedDir`.
    pub async fn get_response<B>(&self, req: &Request<B>) -> Result<Response<Body>, Infallible> {
        match self.get(req.uri().path(), req.headers()).await {
            Ok(entity) => Ok(entity.serve_request(req, StatusCode::OK)),
            Err(SerdirError::NotFound(Some(entity))) => {
                Ok(entity.serve_request(req, StatusCode::NOT_FOUND))
            }
            Err(SerdirError::NotFound(None)) | Err(SerdirError::IsDirectory(_)) => {
                Ok(Self::make_status_response(StatusCode::NOT_FOUND))
            }
            Err(SerdirError::InvalidPath(_)) => {
                // TODO: log the error
                Ok(Self::make_status_response(StatusCode::BAD_REQUEST))
            }
            Err(_) => Ok(Self::make_status_response(
                // TODO: log the error
                StatusCode::INTERNAL_SERVER_ERROR,
            )),
        }
    }

    pub(crate) fn make_status_response(status: StatusCode) -> Response<Body> {
        let reason = status.canonical_reason().unwrap_or("Unknown");
        Response::builder()
            .status(status)
            .body(Body::from(reason))
            .expect("status response should be valid")
    }

    fn create_entity(&self, matched_file: MatchedFile) -> Result<FileEntity, SerdirError> {
        let content_type: HeaderValue = self.get_content_type(&matched_file.extension);
        let mut headers = self.common_headers.clone();
        headers.insert(http::header::CONTENT_TYPE, content_type);

        // Add `Content-Encoding` and `Vary` headers for the encoding to `hdrs`.
        if let Some(value) = matched_file.content_encoding.get_header_value() {
            headers.insert(header::CONTENT_ENCODING, value);
        }
        if !self.compression_strategy.is_none() {
            headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        }
        let etag = self.calculate_etag(matched_file.file_info, matched_file.file.as_ref())?;
        Ok(FileEntity::new_with_metadata(
            matched_file.file,
            matched_file.file_info,
            headers,
            etag,
        ))
    }

    fn find_file(
        &self,
        path: &Path,
        preferred: CompressionSupport,
    ) -> Result<MatchedFile, SerdirError> {
        self.compression_strategy.find_file(path, preferred)
    }

    fn calculate_etag(
        &self,
        file_info: FileInfo,
        file: &File,
    ) -> Result<Option<ETag>, std::io::Error> {
        self.etag_cache
            .get_or_insert(file_info, file, self.file_hasher)
    }

    fn get_content_type(&self, extension: &str) -> HeaderValue {
        self.known_extensions
            .get(extension)
            .cloned()
            .unwrap_or_else(|| self.default_content_type.clone())
    }

    /// Ensures path is safe: no NUL bytes, not absolute, no `..` segments.
    fn validate_path(&self, path: &str) -> Result<(), SerdirError> {
        if memchr::memchr(0, path.as_bytes()).is_some() {
            return Err(SerdirError::InvalidPath(
                "path contains NUL byte".to_string(),
            ));
        }
        if path.as_bytes().first() == Some(&b'/') {
            return Err(SerdirError::InvalidPath("path is absolute".to_string()));
        }
        let mut left = path.as_bytes();
        loop {
            let next = memchr::memchr(b'/', left);
            let seg = &left[0..next.unwrap_or(left.len())];
            if seg == b".." {
                return Err(SerdirError::InvalidPath(
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

    /// Returns a Tower service that serves files from this `ServedDir`.
    #[cfg(feature = "tower")]
    pub fn into_tower_service(self) -> TowerService {
        TowerService::new(self)
    }

    /// Returns a Hyper service that serves files from this `ServedDir`.
    #[cfg(feature = "hyper")]
    pub fn into_hyper_service(self) -> HyperService {
        HyperService::new(self)
    }

    /// Returns a Tower layer that serves files from this `ServedDir` and
    /// delegates unmatched requests to an inner service.
    #[cfg(feature = "tower")]
    pub fn into_tower_layer(self) -> TowerLayer {
        TowerLayer::new(self)
    }
}

/// A builder for [`ServedDir`].
#[derive(Debug)]
pub struct ServedDirBuilder {
    dirpath: PathBuf,
    compression_strategy: CompressionStrategy,
    file_hasher: Option<FileHasher>,
    strip_prefix: Option<String>,
    known_extensions: HashMap<String, HeaderValue>,
    default_content_type: HeaderValue,
    common_headers: HeaderMap,
    append_index_html: bool,
    not_found_path: Option<PathBuf>,
}

impl ServedDirBuilder {
    /// Creates a builder for a `ServedDir` that will serve files from `dirpath`.
    ///
    /// This function returns an error if `dirpath` is not a directory or a
    /// symlink to a directory.
    ///
    /// Updates to the directory content while it's being served are safe, as
    /// long as content is changed by deleting and moving paths, rather than
    /// writing to existing files. If a file is written to while a request is
    /// reading it, in addition to the risk of the file content being corrupt,
    /// the file's ETag and modification date metadata will not match the served
    /// contents. The recommended way to update served content live is to pass a
    /// symlink to the target directory to this function, and then update the
    /// symlink to point to the new directory when needed.
    pub fn new(dirpath: impl Into<PathBuf>) -> Result<Self, SerdirError> {
        let dirpath = dirpath.into();
        if !dirpath.is_dir() {
            let msg = format!("path is not a directory: {}", dirpath.display());
            return Err(SerdirError::ConfigError(msg));
        }
        Ok(Self {
            dirpath,
            compression_strategy: CompressionStrategy::none(),
            file_hasher: None,
            strip_prefix: None,
            known_extensions: Self::default_extensions(),
            default_content_type: OCTET_STREAM.clone(),
            common_headers: HeaderMap::new(),
            append_index_html: false,
            not_found_path: None,
        })
    }

    /// Sets the compression strategy (i.e. static, cached or none)for the
    /// served directory.
    pub fn compression(mut self, strategy: impl Into<CompressionStrategy>) -> Self {
        self.compression_strategy = strategy.into();
        self
    }

    /// Convenience method for configuring static pre-compressed file lookup.
    pub fn static_compression(self, br: bool, gzip: bool, zstd: bool) -> Self {
        let strategy = StaticCompression::none().brotli(br).gzip(gzip).zstd(zstd);
        self.compression(strategy)
    }

    /// Convenience method for configuring cached runtime Brotli compression.
    #[cfg(feature = "runtime-compression")]
    pub fn cached_compression(self, level: BrotliLevel) -> Self {
        use crate::compression::CachedCompression;

        let strategy = CachedCompression::new().compression_level(level);
        self.compression(strategy)
    }

    /// Convenience method for disabling compression.
    pub fn no_compression(self) -> Self {
        self.compression(CompressionStrategy::none())
    }

    /// If true, causes the `ServedDir` to append "/index.html" to directory
    /// paths.
    ///
    /// If false, a 404 will be returned for directory paths.
    pub fn append_index_html(mut self, append: bool) -> Self {
        self.append_index_html = append;
        self
    }

    /// Sets the hash function used to compute ETag values for served files.
    ///
    /// The function receives the opened file handle and should return:
    /// - `Ok(Some(hash))` to set an ETag using the provided 64-bit hash value.
    /// - `Ok(None)` to suppress ETag for that file.
    /// - `Err(e)` to propagate an I/O error.
    ///
    /// If not set, `ServedDir` defaults to hashing file contents with `rapidhash`.
    pub fn file_hasher(mut self, file_hasher: FileHasher) -> Self {
        self.file_hasher = Some(file_hasher);
        self
    }

    /// Sets a prefix to strip from the request path.
    ///
    /// If this value is defined, [`ServedDir::get`] will return a [`SerdirError::InvalidPath`]
    /// error for any path that doesn't begin with the given prefix.
    pub fn strip_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.strip_prefix = Some(prefix.into());
        self
    }

    /// Defines a mapping from file extensions to HTTP content types.
    ///
    /// If not set, will use a smaller builtin extension mapping of common web file extensions.
    ///
    /// The extensions recognized by the builtin mapping are:
    /// - apng
    /// - avif
    /// - bmp
    /// - bz
    /// - bz2
    /// - css
    /// - csv
    /// - doc
    /// - docx
    /// - ecma
    /// - es
    /// - gif
    /// - gz
    /// - htm
    /// - html
    /// - hxt
    /// - ico
    /// - ics
    /// - ini
    /// - jfif
    /// - jpeg
    /// - jpg
    /// - js
    /// - jsm
    /// - json
    /// - jsx
    /// - log
    /// - markdown
    /// - md
    /// - mkd
    /// - mp4
    /// - mpeg
    /// - mpg
    /// - mpg4
    /// - pdf
    /// - pjp
    /// - pjpeg
    /// - png
    /// - ppt
    /// - pptx
    /// - svg
    /// - tar
    /// - text
    /// - tiff
    /// - toml
    /// - txt
    /// - webm
    /// - webp
    /// - xls
    /// - xlsx
    /// - xml
    /// - xz
    /// - yaml
    /// - yml
    /// - zip
    pub fn known_extensions(mut self, extensions: HashMap<String, HeaderValue>) -> Self {
        self.known_extensions = extensions;
        self
    }

    /// Adds a single mapping from a file extension to an HTTP content type.
    ///
    /// The extension should be provide without a leading dot (e.g. "html" instead of ".html").
    pub fn known_extension(
        mut self,
        extension: impl Into<String>,
        content_type: HeaderValue,
    ) -> Self {
        self.known_extensions.insert(extension.into(), content_type);
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

    /// Sets a path to a file to serve for 404 Not Found errors.
    ///
    /// The path must be relative to the directory being served.
    ///
    /// The extension of the file will be used to determine the content type of all 404 responses.
    pub fn not_found_path(mut self, path: impl Into<PathBuf>) -> Result<Self, SerdirError> {
        let path = path.into();
        if path.is_absolute() || path.has_root() {
            return Err(SerdirError::ConfigError(
                "not_found_path must be relative".to_string(),
            ));
        }
        let full_path = self.dirpath.join(path);
        if !full_path.is_file() {
            return Err(SerdirError::ConfigError(format!(
                "not_found_path is not a file: {}",
                full_path.display()
            )));
        }
        self.not_found_path = Some(full_path);
        Ok(self)
    }

    /// Builds the [`ServedDir`].
    pub fn build(self) -> ServedDir {
        ServedDir {
            dirpath: self.dirpath,
            compression_strategy: self.compression_strategy.into_inner(),
            file_hasher: self.file_hasher.unwrap_or(default_hasher),
            strip_prefix: self.strip_prefix,
            known_extensions: self.known_extensions,
            default_content_type: self.default_content_type,
            common_headers: self.common_headers,
            append_index_html: self.append_index_html,
            not_found_path: self.not_found_path,
            etag_cache: EtagCache::new(),
        }
    }

    fn default_extensions() -> HashMap<String, HeaderValue> {
        let mut m = HashMap::new();
        let extensions = [
            ("html", "text/html"),
            ("htm", "text/html"),
            ("hxt", "text/html"),
            ("css", "text/css"),
            ("js", "text/javascript"),
            ("es", "text/javascript"),
            ("ecma", "text/javascript"),
            ("jsm", "text/javascript"),
            ("jsx", "text/javascript"),
            ("png", "image/png"),
            ("apng", "image/apng"),
            ("avif", "image/avif"),
            ("gif", "image/gif"),
            ("ico", "image/x-icon"),
            ("jpeg", "image/jpeg"),
            ("jfif", "image/jpeg"),
            ("pjpeg", "image/jpeg"),
            ("pjp", "image/jpeg"),
            ("jpg", "image/jpeg"),
            ("svg", "image/svg+xml"),
            ("tiff", "image/tiff"),
            ("webp", "image/webp"),
            ("bmp", "image/bmp"),
            ("pdf", "application/pdf"),
            ("zip", "application/zip"),
            ("gz", "application/gzip"),
            ("tar", "application/tar"),
            ("bz", "application/x-bzip"),
            ("bz2", "application/x-bzip2"),
            ("xz", "application/x-xz"),
            ("csv", "text/csv"),
            ("txt", "text/plain"),
            ("text", "text/plain"),
            ("log", "text/plain"),
            ("md", "text/markdown"),
            ("markdown", "text/x-markdown"),
            ("mkd", "text/x-markdown"),
            ("mp4", "video/mp4"),
            ("webm", "video/webm"),
            ("mpeg", "video/mpeg"),
            ("mpg", "video/mpeg"),
            ("mpg4", "video/mp4"),
            ("xml", "application/xml"),
            ("json", "application/json"),
            ("yaml", "application/yaml"),
            ("yml", "application/yaml"),
            ("toml", "application/toml"),
            ("ini", "application/ini"),
            ("ics", "text/calendar"),
            ("doc", "application/msword"),
            (
                "docx",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            ("xls", "application/vnd.ms-excel"),
            (
                "xlsx",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            ("ppt", "application/vnd.ms-powerpoint"),
            (
                "pptx",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            ),
        ];

        for (ext, ct) in extensions {
            m.insert(ext.to_string(), HeaderValue::from_static(ct));
        }
        m
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

    fn fixed_hash(_: &std::fs::File) -> Result<Option<u64>, std::io::Error> {
        Ok(Some(0x1234))
    }

    fn no_hash(_: &std::fs::File) -> Result<Option<u64>, std::io::Error> {
        Ok(None)
    }

    fn hash_error(_: &std::fs::File) -> Result<Option<u64>, std::io::Error> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "hash calculation failed",
        ))
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
        assert!(matches!(err, SerdirError::NotFound(_)));
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
        assert!(matches!(err, SerdirError::InvalidPath(msg) if msg.contains("..")));

        // 2. Contains null byte
        let err = served_dir.get("test\0file.txt", &hdrs).await.unwrap_err();
        assert!(matches!(err, SerdirError::InvalidPath(msg) if msg.contains("NUL byte")));
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
        assert!(matches!(err, SerdirError::NotFound(_)));
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

        let served_dir = context.builder.static_compression(true, true, true).build();
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

        let served_dir = context.builder.static_compression(true, true, true).build();
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
        assert!(matches!(err, SerdirError::IsDirectory(_)));
        if let SerdirError::IsDirectory(err_path) = err {
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
        assert!(matches!(err, SerdirError::IsDirectory(_)));

        // 2. append_index_html enabled
        let builder = ServedDir::builder(path.to_path_buf()).unwrap();
        let served_dir = builder.append_index_html(true).build();
        let e = served_dir.get("subdir", &hdrs).await.unwrap();
        assert_eq!(e.read_body().await.unwrap(), "index content");
        assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "text/html");

        // 3. append_index_html enabled, but index.html missing
        std::fs::create_dir(path.join("empty_subdir")).unwrap();
        let err = served_dir.get("empty_subdir", &hdrs).await.unwrap_err();
        assert!(matches!(err, SerdirError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_compression_priority() {
        let context = TestContext::new();
        context.write_file("test.txt", "raw content");
        context.write_file("test.txt.gz", "fake gzip content");
        context.write_file("test.txt.br", "fake brotli content");
        context.write_file("test.txt.zstd", "fake zstd content");

        let served_dir = context.builder.static_compression(true, true, true).build();
        let mut hdrs = HeaderMap::new();
        hdrs.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("br, gzip, zstd"),
        );

        // 1. All 3 available -> Brotli used
        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "br");
        assert_eq!(e.read_body().await.unwrap(), "fake brotli content");

        // 2. Zstandard and Gzip available -> Zstandard used
        std::fs::remove_file(context.tmp.path().join("test.txt.br")).unwrap();
        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "zstd");
        assert_eq!(e.read_body().await.unwrap(), "fake zstd content");

        // 3. Only Gzip available -> Gzip used
        std::fs::remove_file(context.tmp.path().join("test.txt.zstd")).unwrap();
        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "gzip");
        assert_eq!(e.read_body().await.unwrap(), "fake gzip content");

        // 4. None available -> Raw used
        std::fs::remove_file(context.tmp.path().join("test.txt.gz")).unwrap();
        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert!(e.header(&header::CONTENT_ENCODING).is_none());
        assert_eq!(e.read_body().await.unwrap(), "raw content");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_not_found_path_config() {
        let context = TestContext::new();
        context.write_file("404.html", "not found");
        std::fs::create_dir(context.tmp.path().join("subdir")).unwrap();

        let path = context.tmp.path().to_path_buf();
        let builder = ServedDir::builder(path).unwrap();

        // 1. Successfully set relative path to file
        let builder = builder
            .not_found_path("404.html")
            .expect("failed to set not_found_path");
        let served_dir = builder.build();
        assert_eq!(
            served_dir.not_found_path,
            Some(context.tmp.path().join("404.html"))
        );

        // 2. Error on absolute path
        let err = ServedDir::builder(context.tmp.path().to_path_buf())
            .unwrap()
            .not_found_path("/abs/path")
            .unwrap_err();
        assert!(matches!(err, SerdirError::ConfigError(msg) if msg.contains("must be relative")));

        // 3. Error on directory
        let err = ServedDir::builder(context.tmp.path().to_path_buf())
            .unwrap()
            .not_found_path("subdir")
            .unwrap_err();
        assert!(matches!(err, SerdirError::ConfigError(msg) if msg.contains("is not a file")));

        // 4. Error on non-existent path
        let err = ServedDir::builder(context.tmp.path().to_path_buf())
            .unwrap()
            .not_found_path("missing.html")
            .unwrap_err();
        assert!(matches!(err, SerdirError::ConfigError(msg) if msg.contains("not a file")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_static_compression_config() {
        let context = TestContext::new();
        context.write_file("test.txt", "raw content");
        context.write_file("test.txt.br", "fake brotli content");
        context.write_file("test.txt.gz", "fake gzip content");
        context.write_file("test.txt.zstd", "fake zstd content");

        let mut hdrs = HeaderMap::new();
        hdrs.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("br, gzip, zstd"),
        );

        // 1. Only Brotli enabled
        let served_dir = context
            .builder
            .static_compression(true, false, false)
            .build();
        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "br");

        // 2. Only Zstandard enabled
        let builder = ServedDir::builder(context.tmp.path().to_path_buf()).unwrap();
        let served_dir = builder.static_compression(false, false, true).build();
        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "zstd");

        // 3. Only Gzip enabled
        let builder = ServedDir::builder(context.tmp.path().to_path_buf()).unwrap();
        let served_dir = builder.static_compression(false, true, false).build();
        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "gzip");

        // 4. None enabled (even if files exist and client supports them)
        let builder = ServedDir::builder(context.tmp.path().to_path_buf()).unwrap();
        let served_dir = builder.static_compression(false, false, false).build();
        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert!(e.header(&header::CONTENT_ENCODING).is_none());

        // 5. Both Brotli and Zstandard enabled (Brotli preferred)
        let builder = ServedDir::builder(context.tmp.path().to_path_buf()).unwrap();
        let served_dir = builder.static_compression(true, false, true).build();
        let e = served_dir.get("test.txt", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_ENCODING).unwrap(), "br");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_not_found_path_behavior() {
        let context = TestContext::new();
        context.write_file("404.html", "custom 404 content");
        context.write_file("exists.txt", "found");

        let served_dir = context.builder.not_found_path("404.html").unwrap().build();
        let hdrs = HeaderMap::new();

        // 1. Existing file -> Ok
        let e = served_dir.get("exists.txt", &hdrs).await.unwrap();
        assert_eq!(e.read_body().await.unwrap(), "found");

        // 2. Non-existent file -> NotFound(Some(entity))
        let err = served_dir.get("missing.txt", &hdrs).await.unwrap_err();
        if let SerdirError::NotFound(Some(entity)) = err {
            assert_eq!(entity.read_body().await.unwrap(), "custom 404 content");
            assert_eq!(entity.header(&header::CONTENT_TYPE).unwrap(), "text/html");
        } else {
            panic!("expected NotFound(Some(_)), got {:?}", err);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_default_hash_function_sets_etag() {
        let context = TestContext::new();
        context.write_file("one.txt", "one");
        let served_dir = context.builder.build();
        let hdrs = HeaderMap::new();

        let entity = served_dir.get("one.txt", &hdrs).await.unwrap();
        assert!(entity.etag().is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_custom_hash_function_sets_etag() {
        let context = TestContext::new();
        context.write_file("one.txt", "one");
        let served_dir = context.builder.file_hasher(fixed_hash).build();
        let hdrs = HeaderMap::new();

        let entity = served_dir.get("one.txt", &hdrs).await.unwrap();
        let etag = entity.etag().expect("etag should be present");
        assert_eq!(etag.to_str().unwrap(), r#""0000000000001234""#);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_custom_hash_function_can_disable_etag() {
        let context = TestContext::new();
        context.write_file("one.txt", "one");
        let served_dir = context.builder.file_hasher(no_hash).build();
        let hdrs = HeaderMap::new();

        let entity = served_dir.get("one.txt", &hdrs).await.unwrap();
        assert!(entity.etag().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_hash_function_error_is_propagated() {
        let context = TestContext::new();
        context.write_file("one.txt", "one");
        let served_dir = context.builder.file_hasher(hash_error).build();
        let hdrs = HeaderMap::new();

        let err = served_dir.get("one.txt", &hdrs).await.unwrap_err();
        match err {
            SerdirError::IOError(inner) => {
                assert_eq!(inner.kind(), std::io::ErrorKind::Other);
                assert_eq!(inner.to_string(), "hash calculation failed");
            }
            other => panic!("expected IOError, got {:?}", other),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_not_found_path_uses_custom_hash_function() {
        let context = TestContext::new();
        context.write_file("404.html", "custom 404 content");
        let served_dir = context
            .builder
            .file_hasher(fixed_hash)
            .not_found_path("404.html")
            .unwrap()
            .build();
        let hdrs = HeaderMap::new();

        let err = served_dir.get("missing.txt", &hdrs).await.unwrap_err();
        if let SerdirError::NotFound(Some(entity)) = err {
            let etag = entity.etag().expect("etag should be present");
            assert_eq!(etag.to_str().unwrap(), r#""0000000000001234""#);
        } else {
            panic!("expected NotFound(Some(_)), got {:?}", err);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_etag_cache_is_used() {
        let context = TestContext::new();
        context.write_file("test.txt", "content");

        static CALL_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

        fn counting_hasher(_: &File) -> Result<Option<u64>, std::io::Error> {
            CALL_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Some(0x1234))
        }

        let served_dir = context.builder.file_hasher(counting_hasher).build();
        let hdrs = HeaderMap::new();

        // First call should trigger hasher
        served_dir.get("test.txt", &hdrs).await.unwrap();
        assert_eq!(CALL_COUNT.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Second call should use cached value
        served_dir.get("test.txt", &hdrs).await.unwrap();
        assert_eq!(CALL_COUNT.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_dir_custom_extensions() {
        let context = TestContext::new();
        context.write_file("test.custom", "custom content");
        context.write_file("another.thing", "thing content");
        context.write_file("test.html", "html content");

        let mut custom_extensions = HashMap::new();
        custom_extensions.insert(
            "custom".to_string(),
            HeaderValue::from_static("text/custom"),
        );

        let served_dir = context
            .builder
            .known_extensions(custom_extensions)
            .known_extension("thing", HeaderValue::from_static("application/thing"))
            .build();
        let hdrs = HeaderMap::new();

        // 1. Verify known_extensions works
        let e = served_dir.get("test.custom", &hdrs).await.unwrap();
        assert_eq!(e.header(&header::CONTENT_TYPE).unwrap(), "text/custom");

        // 2. Verify known_extension works
        let e = served_dir.get("another.thing", &hdrs).await.unwrap();
        assert_eq!(
            e.header(&header::CONTENT_TYPE).unwrap(),
            "application/thing"
        );

        // 3. Verify defaults are gone because known_extensions replaced the map
        // (Wait, I should check if that's what we want. The user said: "The known_extensions
        // setter method should still replace the known_extensions field entirely")
        let e = served_dir.get("test.html", &hdrs).await.unwrap();
        assert_eq!(
            e.header(&header::CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
    }
}
