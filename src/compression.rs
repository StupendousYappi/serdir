// Copyright (c) 2016-2021 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use http::header::{self, HeaderMap, HeaderValue};
use std::fs::File;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

#[cfg(feature = "runtime-compression")]
use crate::brotli_cache::BrotliCache;
use crate::{FileEntity, ServeFilesError};

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

/// The compression styles supported by the client of a request.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CompressionSupport {
    /// Use gzip compression.
    Gzip,
    /// Use brotli compression
    Brotli,
    /// Use brotli if available, otherwise gzip
    Both,
    /// Do not use compression.
    None,
}

impl CompressionSupport {
    /// Returns true if Brotli compression is supported.
    pub fn brotli(&self) -> bool {
        matches!(self, CompressionSupport::Brotli | CompressionSupport::Both)
    }

    /// Returns true if Gzip compression is supported.
    pub fn gzip(&self) -> bool {
        matches!(self, CompressionSupport::Gzip | CompressionSupport::Both)
    }

    /// Returns the preferred compression to use when responding to the given request, if any.
    ///
    /// Follows the rules of [RFC 7231 section
    /// 5.3.4](https://tools.ietf.org/html/rfc7231#section-5.3.4).
    ///
    /// Note that if both gzip and brotli are supported, brotli will be preferred by the server.
    pub fn detect(headers: &HeaderMap) -> CompressionSupport {
        let v = match headers.get(header::ACCEPT_ENCODING) {
            None => return CompressionSupport::None,
            Some(v) if v.is_empty() => return CompressionSupport::None,
            Some(v) => v,
        };
        let (mut gzip_q, mut br_q, mut identity_q, mut star_q) = (None, None, None, None);
        let parts = match v.to_str() {
            Ok(s) => s.split(','),
            Err(_) => return CompressionSupport::None,
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
                        return CompressionSupport::None; // unparseable.
                    };
                    quality = q;
                }
            };

            if coding == "gzip" {
                gzip_q = Some(quality);
            } else if coding == "br" {
                br_q = Some(quality);
            } else if coding == "identity" {
                identity_q = Some(quality);
            } else if coding == "*" {
                star_q = Some(quality);
            }
        }

        let gzip_q = gzip_q.or(star_q).unwrap_or(0);
        let br_q = br_q.or(star_q).unwrap_or(0);

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

        match (use_gzip, use_br) {
            (true, false) => CompressionSupport::Gzip,
            (false, true) => CompressionSupport::Brotli,
            (true, true) => CompressionSupport::Both,
            (false, false) => CompressionSupport::None,
        }
    }
}

/// The strategy used to obtain compressed files compatible with the request's
/// supported encodings.
#[derive(Debug, Clone)]
pub(crate) enum CompressionStrategy {
    /// Look for pre-compressed versions of the original file by adding the appropriate filename
    /// extension to the original file name.
    Static,

    /// Compresses supported file types at runtime using Brotli, and caches the
    /// compressed versions for reuse.
    #[cfg(feature = "runtime-compression")]
    Dynamic(Arc<BrotliCache>),

    /// Do not use compression, only return the original file, if available.
    None,
}

impl CompressionStrategy {
    pub(crate) fn find_file(
        &self,
        path: PathBuf,
        supported: crate::CompressionSupport,
    ) -> Result<MatchedFile, ServeFilesError> {
        match self {
            CompressionStrategy::Static => {
                if supported.brotli() {
                    let br_path = path.with_added_extension("br");
                    match Self::try_path(&br_path, ContentEncoding::Brotli) {
                        Ok(f) => return Ok(f),
                        Err(ServeFilesError::NotFound) | Err(ServeFilesError::IsDirectory(_)) => {}
                        Err(e) => return Err(e),
                    }
                }

                if supported.gzip() {
                    let gz_path = path.with_added_extension("gz");
                    match Self::try_path(&gz_path, ContentEncoding::Gzip) {
                        Ok(f) => return Ok(f),
                        Err(ServeFilesError::NotFound) | Err(ServeFilesError::IsDirectory(_)) => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            #[cfg(feature = "runtime-compression")]
            CompressionStrategy::Dynamic(cache) => {
                if supported.brotli() {
                    let matched = cache.get(&path)?;
                    return Ok(matched);
                }
            }
            CompressionStrategy::None => {}
        }

        Self::try_path(&path, ContentEncoding::Identity)
    }

    fn try_path(p: &Path, encoding: ContentEncoding) -> Result<MatchedFile, ServeFilesError> {
        // we want to read the file metadata from the open file handle, rather than calling
        // `std::fs::metadata` on the path, to guarantee that the metadata and the file contents
        // are consistent (otherwise, if the file is modified, there could be a race condition
        // that causes a mismatch between etag values and file contents, which could cause corrupt
        // behavior for clients)
        match File::open(p) {
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
            Err(ref e) if e.kind() == ErrorKind::NotFound => Err(ServeFilesError::NotFound),
            Err(e) => Err(ServeFilesError::IOError(e)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ContentEncoding {
    Gzip,
    Brotli,
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
            ContentEncoding::Identity => None,
        }
    }
}

/// An opened file handle to a file, as returned by `ServedDir::open`.
///
/// This is not necessarily a plain file; it could also be a directory, for example.
///
/// The caller can inspect it as desired. If it is a directory, the caller might pass the result of
/// `into_file()` to `nix::dir::Dir::from`. If it is a plain file, the caller might create an
/// `serve_files::Entity` with `into_file_entity()`.
#[derive(Clone)]
pub(crate) struct MatchedFile {
    pub(crate) file_info: crate::FileInfo,
    pub(crate) file: Arc<File>,
    pub(crate) content_encoding: ContentEncoding,
    pub(crate) extension: String,
}

impl MatchedFile {
    /// Converts this MatchedFile (which must represent a plain file) into a `FileEntity`.
    /// The caller is expected to supply all headers. The function `add_encoding_headers`
    /// may be useful.
    pub(crate) fn into_file_entity<D>(
        self,
        headers: HeaderMap,
    ) -> Result<FileEntity<D>, ServeFilesError>
    where
        D: 'static + Send + Sync + bytes::Buf + From<Vec<u8>> + From<&'static [u8]>,
    {
        FileEntity::new_with_metadata(self.file, self.file_info, headers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::HeaderValue;
    use http::{self, header};
    use pretty_assertions::assert_matches;

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
        assert!(matches!(
            CompressionSupport::detect(&header::HeaderMap::new()),
            CompressionSupport::None
        ));

        // "If the representation's content-coding is one of the
        // content-codings listed in the Accept-Encoding field, then it is
        // acceptable unless it is accompanied by a qvalue of 0.  (As
        // defined in Section 5.3.1, a qvalue of 0 means "not acceptable".)"
        assert_matches!(
            CompressionSupport::detect(&ae_hdrs("gzip")),
            CompressionSupport::Gzip
        );
        assert_matches!(
            CompressionSupport::detect(&ae_hdrs("gzip;q=0.001")),
            CompressionSupport::Gzip
        );
        assert_matches!(
            CompressionSupport::detect(&ae_hdrs("br;q=0.001")),
            CompressionSupport::Brotli
        );
        assert_matches!(
            CompressionSupport::detect(&ae_hdrs("br, gzip")),
            CompressionSupport::Both
        );

        assert_matches!(
            CompressionSupport::detect(&ae_hdrs("gzip;q=0")),
            CompressionSupport::None
        );

        // "An Accept-Encoding header field with a combined field-value that is
        // empty implies that the user agent does not want any content-coding in
        // response."
        assert_matches!(
            CompressionSupport::detect(&ae_hdrs("")),
            CompressionSupport::None
        );

        // The asterisk "*" symbol matches any available content-coding not
        // explicitly listed. identity is a content-coding.
        // If * is q=1000, then identity_q=1000, and neither gzip nor br is
        // strictly greater than 1000.
        assert_matches!(
            CompressionSupport::detect(&ae_hdrs("*")),
            CompressionSupport::Both
        );
        assert_matches!(
            CompressionSupport::detect(&ae_hdrs("gzip;q=0, *")),
            CompressionSupport::Brotli
        );
        assert_matches!(
            CompressionSupport::detect(&ae_hdrs("identity;q=0, *")),
            CompressionSupport::Both
        );

        // "If multiple content-codings are acceptable, then the acceptable
        // content-coding with the highest non-zero qvalue is preferred."
        assert_matches!(
            CompressionSupport::detect(&ae_hdrs("identity;q=0.5, gzip;q=1.0")),
            CompressionSupport::Gzip
        );
        assert_matches!(
            CompressionSupport::detect(&ae_hdrs("identity;q=1.0, gzip;q=0.5")),
            CompressionSupport::None
        );
        assert_matches!(
            CompressionSupport::detect(&ae_hdrs("br;q=1.0, gzip;q=0.5, identity;q=0.1")),
            CompressionSupport::Both
        );
        assert_matches!(
            CompressionSupport::detect(&ae_hdrs("br;q=0.5, gzip;q=1.0, identity;q=0.1")),
            CompressionSupport::Both
        );

        // "If an Accept-Encoding header field is present in a request
        // and none of the available representations for the response have a
        // content-coding that is listed as acceptable, the origin server SHOULD
        // send a response without any content-coding."
        assert_matches!(
            CompressionSupport::detect(&ae_hdrs("*;q=0")),
            CompressionSupport::None
        );
        assert_matches!(
            CompressionSupport::detect(&ae_hdrs("gzip;q=0.002")), // q=2
            CompressionSupport::Gzip
        );
    }
}
