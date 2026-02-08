// Copyright (c) 2016-2021 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use http::header::{self, HeaderMap, HeaderValue};
#[cfg(feature = "runtime-compression")]
use std::collections::HashSet;
use std::fs::File;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

trait PathBufExt {
    fn append_extension(&self, extension: impl AsRef<std::ffi::OsStr>) -> PathBuf;
}

impl PathBufExt for Path {
    fn append_extension(&self, extension: impl AsRef<std::ffi::OsStr>) -> PathBuf {
        match self.file_name() {
            Some(file_name) => {
                let mut new_file_name = file_name.to_os_string();
                new_file_name.push(".");
                new_file_name.push(extension.as_ref());
                self.with_file_name(new_file_name)
            }
            None => self.to_path_buf(),
        }
    }
}

#[cfg(feature = "runtime-compression")]
use crate::brotli_cache::BrotliCache;
use crate::ServeFilesError;

/// Builder-style settings for static (pre-compressed file) lookup.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct StaticCompression {
    gzip: bool,
    br: bool,
    zstd: bool,
}

impl StaticCompression {
    /// Creates a new static compression settings value with all encodings disabled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets whether Brotli (`.br`) files should be considered.
    pub fn brotli(mut self, enabled: bool) -> Self {
        self.br = enabled;
        self
    }

    /// Sets whether gzip (`.gz`) files should be considered.
    pub fn gzip(mut self, enabled: bool) -> Self {
        self.gzip = enabled;
        self
    }

    /// Sets whether zstandard (`.zstd`) files should be considered.
    pub fn zstd(mut self, enabled: bool) -> Self {
        self.zstd = enabled;
        self
    }
}

/// Builder-style settings for runtime Brotli compression and caching.
#[cfg(feature = "runtime-compression")]
#[derive(Debug, Clone)]
pub struct CachedCompression {
    cache_size: u16,
    compression_level: u8,
    supported_extensions: Option<HashSet<&'static str>>,
    max_file_size: u64,
}

#[cfg(feature = "runtime-compression")]
impl CachedCompression {
    /// Creates runtime compression settings with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the maximum number of items in the cache.
    ///
    /// Must be at least 4 and a power of 2.
    ///
    /// # Panics
    ///
    /// Panics if the value is invalid.
    pub fn max_size(mut self, size: u16) -> Self {
        assert!(size >= 4, "cache_size must be at least 4");
        assert!(size.is_power_of_two(), "cache_size must be a power of two");
        self.cache_size = size;
        self
    }

    /// Sets the Brotli compression level (0-11).
    ///
    /// # Panics
    ///
    /// Panics if the value is invalid.
    pub fn compression_level(mut self, level: u8) -> Self {
        assert!(level <= 11, "compression_level must be between 0 and 11");
        self.compression_level = level;
        self
    }

    /// Sets the file extensions that are eligible for compression.
    ///
    /// If `None`, all file extensions will be compressed.
    pub fn supported_extensions(mut self, extensions: Option<HashSet<&'static str>>) -> Self {
        self.supported_extensions = extensions;
        self
    }

    /// Sets the maximum file size for compression.
    ///
    /// Files larger than this value will skip compression and be served
    /// in their original form.
    pub fn max_file_size(mut self, size: u64) -> Self {
        self.max_file_size = size;
        self
    }
}

#[cfg(feature = "runtime-compression")]
impl Default for CachedCompression {
    fn default() -> Self {
        Self {
            cache_size: 128,
            compression_level: 3,
            supported_extensions: None,
            max_file_size: u64::MAX,
        }
    }
}

/// Parses an RFC 7231 section 5.3.1 `qvalue` into an integer in [0, 1000].
/// ```text
/// qvalue = ( "0" [ "." 0*3DIGIT ] )
///        / ( "1" [ "." 0*3("0") ] )
/// ```
pub(crate) fn parse_qvalue(s: &str) -> Result<u16, ()> {
    match s {
        "1" | "1." | "1.0" | "1.00" | "1.000" => return Ok(1000),
        "0" | "0." => return Ok(0),
        s if !s.starts_with("0.") => return Err(()),
        _ => {}
    };
    let v = &s[2..];
    let factor = match v.len() {
        1 /* 0.x */ => 100,
        2 /* 0.xx */ => 10,
        3 /* 0.xxx */ => 1,
        _ => return Err(()),
    };
    let v = u16::from_str(v).map_err(|_| ())?;
    let q = v * factor;
    Ok(q)
}

/// A struct representing which compression encodings are supported by the
/// client or the server.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub(crate) struct CompressionSupport {
    gzip: bool,
    br: bool,
    zstd: bool,
}

impl CompressionSupport {
    /// Returns a new `CompressionSupport` with the given settings.
    pub fn new(br: bool, gzip: bool, zstd: bool) -> Self {
        Self { gzip, br, zstd }
    }

    /// Returns true if Brotli compression is supported.
    pub fn brotli(&self) -> bool {
        self.br
    }

    /// Returns true if Gzip compression is supported.
    pub fn gzip(&self) -> bool {
        self.gzip
    }

    /// Returns true if Zstandard compression is supported.
    pub fn zstd(&self) -> bool {
        self.zstd
    }

    /// Returns the preferred compression to use when responding to the given request, if any.
    ///
    /// Follows the rules of [RFC 7231 section
    /// 5.3.4](https://tools.ietf.org/html/rfc7231#section-5.3.4).
    ///
    /// Note that if both gzip and brotli are supported, brotli will be preferred by the server.
    pub fn detect(headers: &HeaderMap) -> CompressionSupport {
        let v = match headers.get(header::ACCEPT_ENCODING) {
            None => return CompressionSupport::default(),
            Some(v) if v.is_empty() => return CompressionSupport::default(),
            Some(v) => v,
        };
        let (mut gzip_q, mut br_q, mut zstd_q, mut identity_q, mut star_q) =
            (None, None, None, None, None);
        let parts = match v.to_str() {
            Ok(s) => s.split(','),
            Err(_) => return CompressionSupport::default(),
        };
        for qi in parts {
            // Parse.
            let coding;
            let quality;
            match qi.split_once(';') {
                None => {
                    coding = qi.trim();
                    quality = 1000;
                }
                Some((c, q)) => {
                    coding = c.trim();
                    let Some(q) = q
                        .trim()
                        .strip_prefix("q=")
                        .and_then(|q| parse_qvalue(q).ok())
                    else {
                        return CompressionSupport::default(); // unparseable.
                    };
                    quality = q;
                }
            };

            if coding == "gzip" {
                gzip_q = Some(quality);
            } else if coding == "br" {
                br_q = Some(quality);
            } else if coding == "zstd" {
                zstd_q = Some(quality);
            } else if coding == "identity" {
                identity_q = Some(quality);
            } else if coding == "*" {
                star_q = Some(quality);
            }
        }

        let gzip_q = gzip_q.or(star_q).unwrap_or(0);
        let br_q = br_q.or(star_q).unwrap_or(0);
        let zstd_q = zstd_q.or(star_q).unwrap_or(0);

        // "If the representation has no content-coding, then it is
        // acceptable by default unless specifically excluded by the
        // Accept-Encoding field stating either "identity;q=0" or "*;q=0"
        // without a more specific entry for "identity"."
        let identity_q = identity_q.or(star_q).unwrap_or(0);

        // The server will always have identity coding available, so if it's
        // higher priority than another coding, there's no need to enable
        // the other coding. According to the RFC, it's possible for a client
        // to support other codings while not supporting identity coding, but
        // that seems very unlikely in practice and we don't support that.
        let use_gzip = gzip_q > 0 && gzip_q >= identity_q;
        let use_br = br_q > 0 && br_q >= identity_q;
        let use_zstd = zstd_q > 0 && zstd_q >= identity_q;

        CompressionSupport {
            gzip: use_gzip,
            br: use_br,
            zstd: use_zstd,
        }
    }
}

/// The strategy used to obtain compressed files compatible with the request's
/// supported encodings.
#[derive(Debug, Clone)]
pub struct CompressionStrategy(CompressionStrategyInner);

impl CompressionStrategy {
    /// Returns a strategy that only serves the original uncompressed file.
    pub fn none() -> Self {
        Self(CompressionStrategyInner::None)
    }

    /// Returns a static compression strategy with all encodings disabled.
    ///
    /// Enable specific encodings by converting a [`StaticCompression`] value
    /// into a strategy.
    pub fn static_compression() -> Self {
        StaticCompression::new().into()
    }

    /// Returns a strategy that performs runtime Brotli compression with caching.
    #[cfg(feature = "runtime-compression")]
    pub fn cached_compression(cache_size: u16, compression_level: u8) -> Self {
        CachedCompression::new()
            .max_size(cache_size)
            .compression_level(compression_level)
            .into()
    }

    pub(crate) fn into_inner(self) -> CompressionStrategyInner {
        self.0
    }
}

impl Default for CompressionStrategy {
    fn default() -> Self {
        Self::none()
    }
}

impl From<StaticCompression> for CompressionStrategy {
    fn from(value: StaticCompression) -> Self {
        Self(CompressionStrategyInner::Static(CompressionSupport::new(
            value.br, value.gzip, value.zstd,
        )))
    }
}

#[cfg(feature = "runtime-compression")]
impl From<CachedCompression> for CompressionStrategy {
    fn from(value: CachedCompression) -> Self {
        let cache = BrotliCache::builder()
            .max_size(value.cache_size)
            .compression_level(value.compression_level)
            .supported_extensions(value.supported_extensions)
            .max_file_size(value.max_file_size)
            .build();
        Self(CompressionStrategyInner::Cached(Arc::new(cache)))
    }
}

/// The internal strategy used to obtain compressed files compatible with the
/// request's supported encodings.
#[derive(Debug, Clone)]
pub(crate) enum CompressionStrategyInner {
    /// Look for pre-compressed versions of the original file by adding the appropriate filename
    /// extension to the original file name.
    Static(CompressionSupport),

    /// Compresses supported file types at runtime using Brotli, and caches the
    /// compressed versions for reuse.
    #[cfg(feature = "runtime-compression")]
    Cached(Arc<BrotliCache>),

    /// Do not use compression, only return the original file, if available.
    None,
}

impl CompressionStrategyInner {
    pub(crate) fn is_none(&self) -> bool {
        matches!(self, CompressionStrategyInner::None)
    }

    pub(crate) fn find_file(
        &self,
        path: PathBuf,
        supported: crate::compression::CompressionSupport,
    ) -> Result<MatchedFile, ServeFilesError> {
        match self {
            CompressionStrategyInner::Static(server_support) => {
                if supported.brotli() && server_support.brotli() {
                    let br_path = path.append_extension("br");
                    match Self::try_path(&br_path, ContentEncoding::Brotli) {
                        Ok(f) => return Ok(f),
                        Err(ServeFilesError::NotFound(_))
                        | Err(ServeFilesError::IsDirectory(_)) => {}
                        Err(e) => return Err(e),
                    }
                }

                if supported.zstd() && server_support.zstd() {
                    let zstd_path = path.append_extension("zstd");
                    match Self::try_path(&zstd_path, ContentEncoding::Zstd) {
                        Ok(f) => return Ok(f),
                        Err(ServeFilesError::NotFound(_))
                        | Err(ServeFilesError::IsDirectory(_)) => {}
                        Err(e) => return Err(e),
                    }
                }

                if supported.gzip() && server_support.gzip() {
                    let gz_path = path.append_extension("gz");
                    match Self::try_path(&gz_path, ContentEncoding::Gzip) {
                        Ok(f) => return Ok(f),
                        Err(ServeFilesError::NotFound(_))
                        | Err(ServeFilesError::IsDirectory(_)) => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            #[cfg(feature = "runtime-compression")]
            CompressionStrategyInner::Cached(cache) => {
                if supported.brotli() {
                    let matched = cache.get(&path)?;
                    return Ok(matched);
                }
            }
            CompressionStrategyInner::None => {}
        }

        Self::try_path(&path, ContentEncoding::Identity)
    }

    fn try_path(p: &Path, encoding: ContentEncoding) -> Result<MatchedFile, ServeFilesError> {
        // we want to read the file metadata from the open file handle, rather than calling
        // `std::fs::metadata` on the path, to guarantee that the metadata and the file contents
        // are consistent (otherwise, if the file is modified, there could be a race condition
        // that causes a mismatch between etag values and file contents, which could cause corrupt
        // behavior for clients)
        match crate::platform::open_file(p) {
            Ok(file) => {
                let file_info = crate::FileInfo::open_file(p, &file)?;
                let extension = p
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                Ok(MatchedFile {
                    file: Arc::new(file),
                    file_info,
                    content_encoding: encoding,
                    extension,
                })
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Err(ServeFilesError::NotFound(None)),
            Err(e) => Err(ServeFilesError::IOError(e)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ContentEncoding {
    Gzip,
    Brotli,
    Zstd,
    Identity,
}

impl ContentEncoding {
    /// Returns the encoding this file is assumed to have applied to the caller's request.
    /// E.g., if automatic gzip compression is enabled and `index.html.gz` was found when the
    /// caller requested `index.html`, this will return `Some("gzip")`. If the caller requests
    /// `index.html.gz`, this will return `None` because the gzip encoding is built in to the
    /// caller's request.
    pub(crate) fn get_header_value(&self) -> Option<HeaderValue> {
        match self {
            ContentEncoding::Gzip => Some(HeaderValue::from_static("gzip")),
            ContentEncoding::Brotli => Some(HeaderValue::from_static("br")),
            ContentEncoding::Zstd => Some(HeaderValue::from_static("zstd")),
            ContentEncoding::Identity => None,
        }
    }
}

/// An opened file handle to a file, as returned by `ServedDir::open`.
///
/// This is not necessarily a plain file; it could also be a directory, for example.
///
/// The caller can inspect it as desired. If it is a directory, the caller might pass the result of
/// `into_file()` to `nix::dir::Dir::from`.
#[derive(Clone)]
pub(crate) struct MatchedFile {
    pub(crate) file_info: crate::FileInfo,
    pub(crate) file: Arc<File>,
    pub(crate) content_encoding: ContentEncoding,
    pub(crate) extension: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::HeaderValue;
    use http::{self, header};

    fn ae_hdrs(value: &'static str) -> http::HeaderMap {
        let mut h = http::HeaderMap::new();
        h.insert(header::ACCEPT_ENCODING, HeaderValue::from_static(value));
        h
    }

    #[test]
    fn test_parse_qvalue() {
        assert_eq!(parse_qvalue("0"), Ok(0));
        assert_eq!(parse_qvalue("0."), Ok(0));
        assert_eq!(parse_qvalue("0.0"), Ok(0));
        assert_eq!(parse_qvalue("0.00"), Ok(0));
        assert_eq!(parse_qvalue("0.000"), Ok(0));
        assert_eq!(parse_qvalue("0.0000"), Err(()));
        assert_eq!(parse_qvalue("0.2"), Ok(200));
        assert_eq!(parse_qvalue("0.23"), Ok(230));
        assert_eq!(parse_qvalue("0.234"), Ok(234));
        assert_eq!(parse_qvalue("1"), Ok(1000));
        assert_eq!(parse_qvalue("1."), Ok(1000));
        assert_eq!(parse_qvalue("1.0"), Ok(1000));
        assert_eq!(parse_qvalue("1.1"), Err(()));
        assert_eq!(parse_qvalue("1.00"), Ok(1000));
        assert_eq!(parse_qvalue("1.000"), Ok(1000));
        assert_eq!(parse_qvalue("1.001"), Err(()));
        assert_eq!(parse_qvalue("1.0000"), Err(()));
        assert_eq!(parse_qvalue("2"), Err(()));
    }

    #[test]
    fn test_detect_compression_support() {
        // "A request without an Accept-Encoding header field implies that the
        // user agent has no preferences regarding content-codings. Although
        // this allows the server to use any content-coding in a response, it
        // does not imply that the user agent will be able to correctly process
        // all encodings." Identity seems safer; don't compress.
        let detect = CompressionSupport::detect(&header::HeaderMap::new());
        assert!(!detect.gzip());
        assert!(!detect.brotli());
        assert!(!detect.zstd());

        // "If the representation's content-coding is one of the
        // content-codings listed in the Accept-Encoding field, then it is
        // acceptable unless it is accompanied by a qvalue of 0.  (As
        // defined in Section 5.3.1, a qvalue of 0 means "not acceptable".)"
        let detect = CompressionSupport::detect(&ae_hdrs("gzip"));
        assert!(detect.gzip());
        assert!(!detect.brotli());
        assert!(!detect.zstd());

        let detect = CompressionSupport::detect(&ae_hdrs("gzip;q=0.001"));
        assert!(detect.gzip());
        assert!(!detect.brotli());
        assert!(!detect.zstd());

        let detect = CompressionSupport::detect(&ae_hdrs("br;q=0.001"));
        assert!(!detect.gzip());
        assert!(detect.brotli());
        assert!(!detect.zstd());

        let detect = CompressionSupport::detect(&ae_hdrs("zstd;q=0.001"));
        assert!(!detect.gzip());
        assert!(!detect.brotli());
        assert!(detect.zstd());

        let detect = CompressionSupport::detect(&ae_hdrs("br, gzip, zstd"));
        assert!(detect.brotli());
        assert!(detect.gzip());
        assert!(detect.zstd());

        let detect = CompressionSupport::detect(&ae_hdrs("gzip;q=0"));
        assert!(!detect.gzip());
        assert!(!detect.brotli());
        assert!(!detect.gzip());

        // "An Accept-Encoding header field with a combined field-value that is
        // empty implies that the user agent does not want any content-coding in
        // response."
        let detect = CompressionSupport::detect(&ae_hdrs(""));
        assert!(!detect.gzip());
        assert!(!detect.brotli());
        assert!(!detect.zstd());

        // The asterisk "*" symbol matches any available content-coding not
        // explicitly listed. identity is a content-coding.
        // If * is q=1000, then identity_q=1000, and neither gzip nor br is
        // strictly greater than 1000.
        let detect = CompressionSupport::detect(&ae_hdrs("*"));
        assert!(detect.gzip());
        assert!(detect.brotli());
        assert!(detect.zstd());

        let detect = CompressionSupport::detect(&ae_hdrs("gzip;q=0, *"));
        assert!(!detect.gzip());
        assert!(detect.brotli());
        assert!(detect.zstd());

        let detect = CompressionSupport::detect(&ae_hdrs("identity;q=0, *"));
        assert!(detect.gzip());
        assert!(detect.brotli());
        assert!(detect.zstd());

        // "If multiple content-codings are acceptable, then the acceptable
        // content-coding with the highest non-zero qvalue is preferred."
        let detect = CompressionSupport::detect(&ae_hdrs("identity;q=0.5, gzip;q=1.0"));
        assert!(detect.gzip());

        let detect = CompressionSupport::detect(&ae_hdrs("identity;q=1.0, gzip;q=0.5"));
        assert!(!detect.gzip());

        let detect = CompressionSupport::detect(&ae_hdrs("br;q=1.0, gzip;q=0.5, identity;q=0.1"));
        assert!(detect.brotli());
        assert!(detect.gzip());
        assert!(!detect.zstd());

        let detect = CompressionSupport::detect(&ae_hdrs("zstd;q=1.0, gzip;q=0.5, identity;q=0.1"));
        assert!(!detect.brotli());
        assert!(detect.gzip());
        assert!(detect.zstd());

        let detect = CompressionSupport::detect(&ae_hdrs("br;q=0.5, gzip;q=1.0, identity;q=0.1"));
        assert!(detect.brotli());
        assert!(detect.gzip());
        assert!(!detect.zstd());

        let detect = CompressionSupport::detect(&ae_hdrs("zstd;q=1.0, gzip;q=0.5, identity;q=0.1"));
        assert!(detect.zstd());
        assert!(detect.gzip());
        assert!(!detect.brotli());

        // "If an Accept-Encoding header field is present in a request
        // and none of the available representations for the response have a
        // content-coding that is listed as acceptable, the origin server SHOULD
        // send a response without any content-coding."
        let detect = CompressionSupport::detect(&ae_hdrs("*;q=0"));
        assert!(!detect.gzip());
        assert!(!detect.brotli());
        assert!(!detect.zstd());

        let detect = CompressionSupport::detect(&ae_hdrs("gzip;q=0.002")); // q=2
        assert!(detect.gzip());
    }

    #[test]
    fn test_static_compression_into_strategy() {
        let strategy: CompressionStrategy = StaticCompression::new()
            .brotli(true)
            .gzip(false)
            .zstd(true)
            .into();
        let inner = strategy.into_inner();

        match inner {
            CompressionStrategyInner::Static(support) => {
                assert!(support.brotli());
                assert!(!support.gzip());
                assert!(support.zstd());
            }
            _ => panic!("expected static compression strategy"),
        }
    }

    #[test]
    fn test_static_compression_constructor_disables_all_encodings() {
        let strategy = CompressionStrategy::static_compression();
        let inner = strategy.into_inner();

        match inner {
            CompressionStrategyInner::Static(support) => {
                assert!(!support.brotli());
                assert!(!support.gzip());
                assert!(!support.zstd());
            }
            _ => panic!("expected static compression strategy"),
        }
    }

    #[test]
    #[cfg(feature = "runtime-compression")]
    fn test_cached_compression_into_strategy() {
        let strategy: CompressionStrategy = CachedCompression::new()
            .max_size(16)
            .compression_level(5)
            .into();
        let inner = strategy.into_inner();
        assert!(matches!(inner, CompressionStrategyInner::Cached(_)));
    }
}
