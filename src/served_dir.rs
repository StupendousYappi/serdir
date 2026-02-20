//! Serves files from a local directory.

use std::convert::Infallible;
use std::fmt::Debug;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Instant;
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
use crate::{Body, ETag, ErrorHandler, FileInfo, Resource, ResourceHasher, SerdirError};
use http::{header, HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode};
use log::error;

/// Returns [`Resource`] values for file paths within a directory.
///
/// A `ServedDir` is created using a [ServedDirBuilder], and must be configured with
/// the path to the static file root directory. When [ServedDir::get] is called,
/// it will attempt to find a file in the root directory with that relative path,
/// detect its content type using the filename extension, and return a [Resource]
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
///
/// ## Path cleanup and validation
///
/// The [ServedDir::get] and [ServedDir:get_response] first cleanup the input
/// path by stripping the configured `strip_prefix` value from the start of the
/// path. If a `strip_prefix` value is defined but the input path doesn't start
/// with it, those methods return `SerdirError::InvalidPath`. Then, they strip
/// a single leading slash from the path if present- this means that paths `foo`
/// and `/foo` are equivalent.
///
/// The cleaned path value is then validated for safety- those methods will return
/// `SerdirError::InvalidPath` if the cleaned path:
/// - Contains a null byte
/// - Contains a parent directory component (i.e. "..")
/// - Starts with a Windows path prefix (e.g. "C:\" or "\\"")
///
/// If the cleaned path passes those checks, it is a relative path, and is resolved
/// relative to the `ServedDir` instance's configured directory. The `ServedDir` will
/// then attempt to find a encoding variant of that path that is compatible with
/// the request client's accepted compression encodings.
pub struct ServedDir {
    dirpath: PathBuf,
    compression_strategy: CompressionStrategyInner,
    file_hasher: ResourceHasher,
    error_handler: Option<ErrorHandler>,
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
pub(crate) fn default_hasher(file: &mut dyn Read) -> Result<Option<u64>, std::io::Error> {
    let hash = rapidhash::v3::rapidhash_v3_file(file)?;
    Ok(Some(hash))
}

impl ServedDir {
    /// Returns a builder for `ServedDir` that will serve files from `path`.
    ///
    /// This function returns an [`SerdirError::ConfigError`] if `path` is not a
    /// directory or a symlink to a directory.
    ///
    /// Updates to the directory content while it's being served are safe, as
    /// long as content is changed by deleting and moving paths, rather than
    /// writing to existing files. If a file is written to while a request is
    /// reading it, in addition to the risk of the file content being corrupt,
    /// the file's ETag and modification date metadata will not match the served
    /// contents. The recommended way to update served content live is to pass a
    /// symlink to the target directory to this function, and then update the
    /// symlink to point to the new directory when needed.
    pub fn builder(path: impl Into<PathBuf>) -> Result<ServedDirBuilder, SerdirError> {
        let dirpath = path.into();
        if !dirpath.is_dir() {
            let msg = format!("path is not a directory: {}", dirpath.display());
            return Err(SerdirError::config_error(msg));
        }
        Ok(ServedDirBuilder {
            dirpath,
            compression_strategy: CompressionStrategy::none(),
            file_hasher: None,
            error_handler: None,
            strip_prefix: None,
            known_extensions: ServedDirBuilder::default_extensions(),
            default_content_type: OCTET_STREAM.clone(),
            common_headers: HeaderMap::new(),
            append_index_html: false,
            not_found_path: None,
        })
    }

    /// Returns the static file root directory used by this `ServedDir`
    pub fn dir(&self) -> &Path {
        &self.dirpath
    }

    /// Returns a `Resource` for the given path and request headers.
    ///
    /// This method searches for a file with the given relative path in this
    /// instance's static files directory. If a file is found, a [Resource]
    /// will be returned that can be used to serve its content in a HTTP
    /// response. This method will also choose a `Content-Type` header value
    /// based on the extension of the matched file.
    ///
    /// This method will return an error with kind `ErrorKind::NotFound` if the
    /// file is not found.  It will return an error with kind
    /// `ErrorKind::InvalidInput` if the path is invalid.
    ///
    /// ## Compression
    ///
    /// If static compression is configured, you can provide pre-compressed
    /// variants of any file by creating a file with the same name as the
    /// original, with the appropriate extension added. For example, if you have
    /// a static file with path `settings/index.html`, this method will look
    /// for:
    /// - the file `settings/index.html.zstd` if the client supports zstandard
    ///   compression
    /// - the file `settings/index.html.br` if the client supports brotli
    ///   compression
    /// - the file `settings/index.html.gz` if the client supports gzip
    ///   compression
    ///
    /// Under static compression, this method will always search for those
    /// variants in the above order (zstd, then br, then gz), regardless of the
    /// prioritization the `Accept-Encoding` header provided. If no compatible
    /// compressed version can be found, the original file will be used (even if
    /// the `Accept-Encoding` header explicitly rejects the `identity`
    /// encoding).
    ///
    /// If cached compression is configured, this method will perform Brotli
    /// compression of matched files on the fly if the client supports Brotli
    /// compression, and will cache the compressed versions for reuse (Brotli is
    /// the only compression algorithm for which runtime compression is
    /// supported). The generated Brotli contents will be cached in unlinked
    /// tempfiles that have no visible path on the filesystem, and will be
    /// cleaned up automatically by the operating system when the application
    /// exits, regardless of how it exits (i.e. they will be cleaned up even if
    /// Rust panics in abort mode). Disk usage can be controlled by limiting the
    /// maximum size of files that should be compressed, and the maximum number
    /// of compressed files to cache, using the `CachedCompression` type. The
    /// Brotli compression level can also be configured, allowing you to choose
    /// your own balance of compression speed and compressed size.
    pub async fn get(&self, path: &str, req_hdrs: &HeaderMap) -> Result<Resource, SerdirError> {
        match self.resolve(path, req_hdrs).await {
            Ok(entity) => Ok(entity),
            Err(err) => {
                error!("File resolution error, {} url_path={}", err, path);
                match self.error_handler {
                    Some(error_handler) => error_handler(err),
                    None => Err(err),
                }
            }
        }
    }

    async fn resolve(&self, path: &str, req_hdrs: &HeaderMap) -> Result<Resource, SerdirError> {
        let path = match self.strip_prefix.as_deref() {
            Some(prefix) if path == prefix => ".",
            Some(prefix) => path
                .strip_prefix(prefix)
                .ok_or(SerdirError::not_found(None))?,
            None => path,
        };
        let path = path.strip_prefix('/').unwrap_or(path);

        let full_path = self.validate_path(path)?;

        let preferred = CompressionSupport::detect(req_hdrs);

        let res = self.find_file(&full_path, preferred).await;
        let matched_file = match res {
            Ok(mf) => mf,
            Err(SerdirError::IsDirectory(_)) if self.append_index_html => {
                let index_path = full_path.join("index.html");
                self.find_file(&index_path, preferred).await?
            }
            Err(SerdirError::NotFound(_)) if self.not_found_path.is_some() => {
                let not_found_path = self.not_found_path.as_ref().unwrap();
                let matched_file = self.find_file(not_found_path, preferred).await?;
                let entity = self.create_entity(matched_file)?;
                return Err(SerdirError::not_found(Some(entity)));
            }
            Err(e) => return Err(e),
        };

        self.create_entity(matched_file)
    }

    /// Returns an HTTP response for a request path and headers.
    ///
    /// A convenience wrapper method for [ServedDir::get], converting a HTTP request
    /// into a HTTP response by serving static files using this `ServedDir`.
    pub async fn get_response<B>(&self, req: &Request<B>) -> Result<Response<Body>, Infallible> {
        let start_time = Instant::now();
        let result = self.get(req.uri().path(), req.headers()).await;
        let resp = match result {
            Ok(resource) => resource.into_response(req, StatusCode::OK),
            Err(SerdirError::NotFound(Some(resource))) => {
                resource.into_response(req, StatusCode::NOT_FOUND)
            }
            Err(err) => Self::make_status_response(err.status_code()),
        };
        let status = resp.status();
        let resp = resp.map(|body| body.enable_trace_log(req, status, start_time));
        Ok(resp)
    }

    fn make_status_response(status: StatusCode) -> Response<Body> {
        let reason = status.canonical_reason().unwrap_or("Unknown");
        Response::builder()
            .status(status)
            .body(Body::from(reason))
            .expect("status response should be valid")
    }

    fn create_entity(&self, matched_file: MatchedFile) -> Result<Resource, SerdirError> {
        let content_type = self
            .known_extensions
            .get(&matched_file.extension)
            .cloned()
            .unwrap_or_else(|| self.default_content_type.clone());
        let mut headers: Vec<(HeaderName, HeaderValue)> = self
            .common_headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        headers.push((http::header::CONTENT_TYPE, content_type));

        // Add `Content-Encoding` and `Vary` headers for the encoding to `hdrs`.
        if let Some(value) = matched_file.content_encoding.get_header_value() {
            headers.push((header::CONTENT_ENCODING, value));
        }
        if !self.compression_strategy.is_none() {
            headers.push((header::VARY, HeaderValue::from_static("Accept-Encoding")));
        }
        let etag = self.calculate_etag(matched_file.file_info, matched_file.file.as_ref())?;
        Ok(Resource::for_file_with_metadata(
            matched_file.file,
            matched_file.file_info,
            headers,
            etag,
        ))
    }

    async fn find_file(
        &self,
        path: &Path,
        preferred: CompressionSupport,
    ) -> Result<MatchedFile, SerdirError> {
        self.compression_strategy.find_file(path, preferred).await
    }

    fn calculate_etag(
        &self,
        file_info: FileInfo,
        file: &File,
    ) -> Result<Option<ETag>, std::io::Error> {
        if let Some(etag) = self.etag_cache.get(&file_info) {
            return Ok(etag);
        }

        let etag = tokio::task::block_in_place(|| {
            let mut reader = file;
            (self.file_hasher)(&mut reader).map(|hash| hash.map(ETag::from))
        })?;
        self.etag_cache.insert(file_info, etag);
        Ok(etag)
    }

    /// Ensures path is safe (no NUL bytes, not absolute, no `..` segments) and
    /// returns the full path joined to the root directory.
    fn validate_path(&self, path: &str) -> Result<PathBuf, SerdirError> {
        if path.as_bytes().contains(&0) {
            return Err(SerdirError::invalid_path(
                "path contains NUL byte".to_string(),
            ));
        }

        let mut full_path = self.dirpath.clone();
        for component in Path::new(path).components() {
            match component {
                std::path::Component::Normal(seg) => full_path.push(seg),
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    return Err(SerdirError::invalid_path(
                        "path contains .. segment".to_string(),
                    ));
                }
                std::path::Component::RootDir => {
                    return Err(SerdirError::invalid_path("path is absolute".to_string()));
                }
                std::path::Component::Prefix(_) => {
                    return Err(SerdirError::invalid_path(
                        "path contains a prefix".to_string(),
                    ));
                }
            }
        }
        Ok(full_path)
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

    /// Returns a Tower layer (i.e. middleware) that serves files from this
    /// `ServedDir` and delegates unmatched requests to an inner service.
    ///
    /// This method can function as a simple alternative to a URL router in
    /// Tower-based applications that serve both static UI files and dynamic
    /// APIs.  It allows static files to be easily mixed with dynamic URL
    /// handlers, and allows UI developers to easily modify static content
    /// across the site without requiring any changes to Rust code. By default,
    /// the middleware will perform a file existence check on the filesystem for
    /// every URL path it receives. This performance cost can be avoided if
    /// define the [`strip_prefix`](Self::strip_prefix) option; if set, the
    /// middleware will immediately pass on reqeusts whose path does not start
    /// with that prefix, without any filesystem interaction.
    #[cfg(feature = "tower")]
    pub fn into_tower_layer(self) -> TowerLayer {
        TowerLayer::new(self)
    }
}

/// A builder for [`ServedDir`].
///
/// Created via the [`ServedDir::builder`] constructor.
#[derive(Debug)]
pub struct ServedDirBuilder {
    dirpath: PathBuf,
    compression_strategy: CompressionStrategy,
    file_hasher: Option<ResourceHasher>,
    error_handler: Option<ErrorHandler>,
    strip_prefix: Option<String>,
    known_extensions: HashMap<String, HeaderValue>,
    default_content_type: HeaderValue,
    common_headers: HeaderMap,
    append_index_html: bool,
    not_found_path: Option<PathBuf>,
}

impl ServedDirBuilder {
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
    pub fn file_hasher(mut self, file_hasher: ResourceHasher) -> Self {
        self.file_hasher = Some(file_hasher);
        self
    }

    /// Sets a function that can transform certain errors into servable resources.
    pub fn error_handler(mut self, error_handler: ErrorHandler) -> Self {
        self.error_handler = Some(error_handler);
        self
    }

    /// Sets a prefix to strip from the request path.
    ///
    /// If this value is defined, [`ServedDir::get`] and
    /// [`ServedDir::get_response`] will return a [`SerdirError::InvalidPath`]
    /// error for any path that doesn't begin with the given prefix.
    ///
    /// This setting can serve as a useful performance optimization when using a
    /// `ServedDir` as Tower middleware via the [`ServedDir::into_tower_layer`]
    /// method; if the input path doesn't begin with the prefix, the request
    /// handling logic will return quickly, without performing any file system
    /// lookups, and fall through to the next Tower handler.
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
    /// The header will added to the `Resource` if the `ServedDir` returns one, but
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
            return Err(SerdirError::config_error(
                "not_found_path must be relative".to_string(),
            ));
        }
        let full_path = self.dirpath.join(path);
        if !full_path.is_file() {
            return Err(SerdirError::config_error(format!(
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
            error_handler: self.error_handler,
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
        static DEFAULT_EXTENSIONS: LazyLock<HashMap<String, HeaderValue>> = LazyLock::new(|| {
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

            extensions
                .iter()
                .map(|&(ext, ct)| (ext.to_string(), HeaderValue::from_static(ct)))
                .collect()
        });

        DEFAULT_EXTENSIONS.clone()
    }
}
