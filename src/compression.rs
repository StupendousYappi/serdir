// Copyright (c) 2016-2021 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use http::header::{self, HeaderMap};
use std::str::FromStr;

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
}

/// Returns the preferred compression to use when responding to the given request, if any.
///
/// Use via `detect_compression_support(req.headers())`.
///
/// Follows the rules of [RFC 7231 section
/// 5.3.4](https://tools.ietf.org/html/rfc7231#section-5.3.4).
///
/// Note that if both gzip and brotli are supported, brotli will be preferred by the server.
pub fn detect_compression_support(headers: &HeaderMap) -> CompressionSupport {
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
            detect_compression_support(&header::HeaderMap::new()),
            CompressionSupport::None
        ));

        // "If the representation's content-coding is one of the
        // content-codings listed in the Accept-Encoding field, then it is
        // acceptable unless it is accompanied by a qvalue of 0.  (As
        // defined in Section 5.3.1, a qvalue of 0 means "not acceptable".)"
        assert_matches!(
            detect_compression_support(&ae_hdrs("gzip")),
            CompressionSupport::Gzip
        );
        assert_matches!(
            detect_compression_support(&ae_hdrs("gzip;q=0.001")),
            CompressionSupport::Gzip
        );
        assert_matches!(
            detect_compression_support(&ae_hdrs("br;q=0.001")),
            CompressionSupport::Brotli
        );
        assert_matches!(
            detect_compression_support(&ae_hdrs("br, gzip")),
            CompressionSupport::Both
        );

        assert_matches!(
            detect_compression_support(&ae_hdrs("gzip;q=0")),
            CompressionSupport::None
        );

        // "An Accept-Encoding header field with a combined field-value that is
        // empty implies that the user agent does not want any content-coding in
        // response."
        assert_matches!(
            detect_compression_support(&ae_hdrs("")),
            CompressionSupport::None
        );

        // The asterisk "*" symbol matches any available content-coding not
        // explicitly listed. identity is a content-coding.
        // If * is q=1000, then identity_q=1000, and neither gzip nor br is
        // strictly greater than 1000.
        assert_matches!(
            detect_compression_support(&ae_hdrs("*")),
            CompressionSupport::Both
        );
        assert_matches!(
            detect_compression_support(&ae_hdrs("gzip;q=0, *")),
            CompressionSupport::Brotli
        );
        assert_matches!(
            detect_compression_support(&ae_hdrs("identity;q=0, *")),
            CompressionSupport::Both
        );

        // "If multiple content-codings are acceptable, then the acceptable
        // content-coding with the highest non-zero qvalue is preferred."
        assert_matches!(
            detect_compression_support(&ae_hdrs("identity;q=0.5, gzip;q=1.0")),
            CompressionSupport::Gzip
        );
        assert_matches!(
            detect_compression_support(&ae_hdrs("identity;q=1.0, gzip;q=0.5")),
            CompressionSupport::None
        );
        assert_matches!(
            detect_compression_support(&ae_hdrs("br;q=1.0, gzip;q=0.5, identity;q=0.1")),
            CompressionSupport::Both
        );
        assert_matches!(
            detect_compression_support(&ae_hdrs("br;q=0.5, gzip;q=1.0, identity;q=0.1")),
            CompressionSupport::Both
        );

        // "If an Accept-Encoding header field is present in a request
        // and none of the available representations for the response have a
        // content-coding that is listed as acceptable, the origin server SHOULD
        // send a response without any content-coding."
        assert_matches!(
            detect_compression_support(&ae_hdrs("*;q=0")),
            CompressionSupport::None
        );
        assert_matches!(
            detect_compression_support(&ae_hdrs("gzip;q=0.002")), // q=2
            CompressionSupport::Gzip
        );
    }
}
