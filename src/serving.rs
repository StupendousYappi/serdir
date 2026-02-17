// Copyright (c) 2016-2018 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::body::Body;
use crate::etag;
use crate::range;
use crate::Resource;
use bytes::Buf;
use futures_util::stream::StreamExt as _;
use http::header::{self, HeaderMap, HeaderValue};
use http::{self, Method, Response, StatusCode};
use httpdate::{fmt_http_date, parse_http_date};
use std::io::Write;
use std::ops::Range;
use std::pin::Pin;
use std::task::Poll;
use std::time::{Duration, SystemTime};

const MAX_DECIMAL_U64_BYTES: usize = 20; // u64::max_value().to_string().len()

fn parse_modified_hdrs(
    etag: &Option<HeaderValue>,
    req_hdrs: &HeaderMap,
    last_modified: Option<SystemTime>,
) -> Result<(bool, bool), &'static str> {
    let last_modified = last_modified.map(|m| {
        let secs = m
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    });

    let precondition_failed = if !etag::any_match(etag, req_hdrs)? {
        true
    } else if let (Some(ref m), Some(since)) =
        (last_modified, req_hdrs.get(header::IF_UNMODIFIED_SINCE))
    {
        const ERR: &str = "Unparseable If-Unmodified-Since";
        *m > parse_http_date(since.to_str().map_err(|_| ERR)?).map_err(|_| ERR)?
    } else {
        false
    };

    let not_modified = match etag::none_match(etag, req_hdrs) {
        // See RFC 7233 section 14.26 <https://tools.ietf.org/html/rfc7233#section-14.26>:
        // "If none of the entity tags match, then the server MAY perform the
        // requested method as if the If-None-Match header field did not exist,
        // but MUST also ignore any If-Modified-Since header field(s) in the
        // request. That is, if no entity tags match, then the server MUST NOT
        // return a 304 (Not Modified) response."
        Some(true) => false,

        Some(false) => true,

        None => {
            if let (Some(ref m), Some(since)) =
                (last_modified, req_hdrs.get(header::IF_MODIFIED_SINCE))
            {
                const ERR: &str = "Unparseable If-Modified-Since";
                *m <= parse_http_date(since.to_str().map_err(|_| ERR)?).map_err(|_| ERR)?
            } else {
                false
            }
        }
    };
    Ok((precondition_failed, not_modified))
}

/// Serves GET and HEAD requests for a given byte-ranged entity.
/// Handles conditional & subrange requests.
/// The caller is expected to have already determined the correct entity and appended
/// `Expires`, `Cache-Control`, and `Vary` headers if desired.
pub(crate) fn serve(
    entity: Resource,
    req: &http::Request<()>,
    status_code: http::StatusCode,
) -> http::Response<Body> {
    let res = match serve_inner(&entity, req.method(), req.headers(), status_code) {
        ServeInner::Simple(res) => *res,
        ServeInner::Multipart {
            res,
            part_headers,
            ranges,
            len,
        } => res
            .body(crate::body::Body {
                stream: crate::body::BodyStream::Multipart {
                    s: MultipartStream::new(entity, part_headers, ranges, len),
                },
                tracking: crate::body::TrackingGuard::new(None),
            })
            .expect("multipart response should be valid"),
    };
    attach_tracking(res, req)
}

pub(crate) fn attach_tracking(mut res: Response<Body>, req: &http::Request<()>) -> Response<Body> {
    let start_time = req
        .extensions()
        .get::<crate::RequestStartTime>()
        .expect("RequestStartTime extension missing; forgot to call request_head?")
        .0;

    if log::log_enabled!(log::Level::Trace) {
        let tracking = crate::body::RequestTracking {
            method: req.method().clone(),
            uri: req.uri().clone(),
            start_time,
            encoding: res.headers().get(http::header::CONTENT_ENCODING).cloned(),
            bytes_sent: 0,
        };
        let body = std::mem::replace(res.body_mut(), Body::empty());
        *res.body_mut() = body.with_tracking(tracking);
    }
    res
}

/// An instruction from `serve_inner` to `serve` on how to respond.
enum ServeInner {
    Simple(Box<Response<Body>>),
    Multipart {
        res: http::response::Builder,
        part_headers: Vec<Vec<u8>>,
        ranges: Vec<Range<u64>>,
        len: u64,
    },
}

/// Runs inner logic for `serve`.
fn serve_inner(
    ent: &Resource,
    method: &http::Method,
    req_hdrs: &http::HeaderMap,
    status_code: StatusCode,
) -> ServeInner {
    if method != Method::GET && method != Method::HEAD {
        return ServeInner::Simple(Box::new(
            Response::builder()
                .status(StatusCode::METHOD_NOT_ALLOWED)
                .header(header::ALLOW, HeaderValue::from_static("get, head"))
                .body(Body::from("This resource only supports GET and HEAD."))
                .unwrap(),
        ));
    }

    let last_modified = ent.last_modified();
    let etag = ent.etag();

    let (precondition_failed, not_modified) =
        match parse_modified_hdrs(&etag, req_hdrs, last_modified) {
            Err(s) => {
                return ServeInner::Simple(Box::new(
                    Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .body(Body::from(s))
                        .unwrap(),
                ))
            }
            Ok(p) => p,
        };

    // See RFC 7233 section 4.1 <https://tools.ietf.org/html/rfc7233#section-4.1>: a Partial
    // Content response should include other representation header fields (aka entity-headers in
    // RFC 2616) iff the client didn't specify If-Range.
    let mut range_hdr = req_hdrs.get(header::RANGE);
    let include_entity_headers_on_range = match req_hdrs.get(header::IF_RANGE) {
        Some(if_range) => {
            let if_range = if_range.as_bytes();
            if if_range.starts_with(b"W/\"") || if_range.starts_with(b"\"") {
                // etag case.
                if let Some(ref some_etag) = etag {
                    if etag::strong_eq(if_range, some_etag.as_bytes()) {
                        false
                    } else {
                        range_hdr = None;
                        true
                    }
                } else {
                    range_hdr = None;
                    true
                }
            } else {
                // Date case.
                // Use the strong validation rules for an origin server:
                // <https://tools.ietf.org/html/rfc7232#section-2.2.2>.
                // The resource could have changed twice in the supplied second, so never match.
                range_hdr = None;
                true
            }
        }
        None => true,
    };

    let mut res = Response::builder()
        .status(status_code)
        .header(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(m) = last_modified {
        // See RFC 7232 section 2.2.1 <https://tools.ietf.org/html/rfc7232#section-2.2.1>: the
        // Last-Modified must not exceed the Date. To guarantee this, set the Date now rather than
        // let hyper set it.
        let d = SystemTime::now();
        res = res.header(header::DATE, fmt_http_date(d));
        let clamped_m = std::cmp::min(m, d);
        res = res.header(header::LAST_MODIFIED, fmt_http_date(clamped_m));
    }
    if let Some(e) = etag {
        res = res.header(http::header::ETAG, e);
    }

    if precondition_failed {
        res = res.status(StatusCode::PRECONDITION_FAILED);
        return ServeInner::Simple(Box::new(
            res.body(Body::from("Precondition failed")).unwrap(),
        ));
    }

    if not_modified {
        res = res.status(StatusCode::NOT_MODIFIED);
        return ServeInner::Simple(Box::new(res.body(Body::empty()).unwrap()));
    }

    let len = ent.len();
    let (range, include_entity_headers) = match range::parse(range_hdr, len) {
        range::ResolvedRanges::None => (0..len, true),
        range::ResolvedRanges::Single(range) => {
            res = res.header(
                header::CONTENT_RANGE,
                unsafe_fmt_ascii_val!(
                    MAX_DECIMAL_U64_BYTES * 3 + "bytes -/".len(),
                    "bytes {}-{}/{}",
                    range.start,
                    range.end - 1,
                    len
                ),
            );
            res = res.status(StatusCode::PARTIAL_CONTENT);
            (range, include_entity_headers_on_range)
        }
        range::ResolvedRanges::Multiple(ranges) => {
            // Before serving multiple ranges via multipart/byteranges, estimate the total
            // length. ("80" is the RFC's estimate of the size of each part's header.) If it's
            // more than simply serving the whole entity, do that instead.
            let est_len = ranges.iter().try_fold(0u64, |acc, r| {
                acc.checked_add(80)
                    .and_then(|a| a.checked_add(r.end - r.start))
            });
            if matches!(est_len, Some(l) if l < len) {
                let each_part_hdrs = include_entity_headers_on_range.then(|| {
                    let mut h = HeaderMap::new();
                    ent.add_headers(&mut h);
                    h
                });
                let (res, part_headers, len) =
                    match prepare_multipart(res, &ranges[..], len, each_part_hdrs) {
                        Ok(v) => v,
                        Err(MultipartLenOverflowError) => {
                            return ServeInner::Simple(Box::new(
                                Response::builder()
                                    .status(StatusCode::PAYLOAD_TOO_LARGE)
                                    .body(Body::from("Multipart response too large"))
                                    .unwrap(),
                            ));
                        }
                    };
                if method == Method::HEAD {
                    return ServeInner::Simple(Box::new(res.body(Body::empty()).unwrap()));
                }
                return ServeInner::Multipart {
                    res,
                    part_headers,
                    ranges,
                    len,
                };
            }

            (0..len, true)
        }
        range::ResolvedRanges::NotSatisfiable => {
            res = res.header(
                http::header::CONTENT_RANGE,
                unsafe_fmt_ascii_val!(MAX_DECIMAL_U64_BYTES + "bytes */".len(), "bytes */{}", len),
            );
            res = res.status(StatusCode::RANGE_NOT_SATISFIABLE);
            return ServeInner::Simple(Box::new(res.body(Body::empty()).unwrap()));
        }
    };
    let len = range.end - range.start;
    res = res.header(
        header::CONTENT_LENGTH,
        unsafe_fmt_ascii_val!(MAX_DECIMAL_U64_BYTES, "{}", len),
    );
    let body = match *method {
        Method::HEAD => Body::empty(),
        _ => Body {
            stream: crate::body::BodyStream::ExactLen {
                s: crate::body::ExactLenStream::new(range.end - range.start, ent.get_range(range)),
            },
            tracking: crate::body::TrackingGuard::new(None),
        },
    };
    let mut res = res.body(body).unwrap();
    if include_entity_headers {
        ent.add_headers(res.headers_mut());
    }
    ServeInner::Simple(Box::new(res))
}

struct MultipartLenOverflowError;

/// Prepares to send a `multipart/mixed` response.
/// Returns the response builder (with overall headers added), each part's headers, and overall len.
fn prepare_multipart(
    mut res: http::response::Builder,
    ranges: &[Range<u64>],
    len: u64,
    include_entity_headers: Option<http::header::HeaderMap>,
) -> Result<(http::response::Builder, Vec<Vec<u8>>, u64), MultipartLenOverflowError> {
    let mut each_part_headers = Vec::new();
    if let Some(h) = include_entity_headers {
        each_part_headers.reserve(
            h.iter()
                .map(|(k, v)| k.as_str().len() + v.as_bytes().len() + 4)
                .sum::<usize>(),
        );
        for (k, v) in &h {
            each_part_headers.extend_from_slice(k.as_str().as_bytes());
            each_part_headers.extend_from_slice(b": ");
            each_part_headers.extend_from_slice(v.as_bytes());
            each_part_headers.extend_from_slice(b"\r\n");
        }
    }

    let mut body_len: u64 = 0;
    let mut part_headers: Vec<Vec<u8>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        let mut buf = Vec::with_capacity(
            "\r\n--B\r\nContent-Range: bytes -/\r\n".len()
                + 3 * MAX_DECIMAL_U64_BYTES
                + each_part_headers.len()
                + "\r\n".len(),
        );
        write!(
            &mut buf,
            "\r\n--B\r\nContent-Range: bytes {}-{}/{}\r\n",
            r.start,
            r.end - 1,
            len
        )
        .unwrap();
        buf.extend_from_slice(&each_part_headers);
        buf.extend_from_slice(b"\r\n");
        body_len = body_len
            .checked_add(crate::as_u64(buf.len()))
            .and_then(|l| l.checked_add(r.end - r.start))
            .ok_or(MultipartLenOverflowError)?;
        part_headers.push(buf);
    }
    body_len = body_len
        .checked_add(crate::as_u64(PART_TRAILER.len()))
        .ok_or(MultipartLenOverflowError)?;

    res = res.header(
        header::CONTENT_LENGTH,
        unsafe_fmt_ascii_val!(MAX_DECIMAL_U64_BYTES, "{}", body_len),
    );
    res = res.header(
        header::CONTENT_TYPE,
        HeaderValue::from_static("multipart/byteranges; boundary=B"),
    );
    res = res.status(StatusCode::PARTIAL_CONTENT);

    Ok((res, part_headers, body_len))
}

pub(crate) struct MultipartStream {
    /// The current part's body stream, set in "send body" state.
    cur: Option<crate::body::ExactLenStream>,

    /// Current state:
    ///
    /// * `i << 1` for `i` in `[0, ranges.len())`: send part headers.
    /// * `i << 1 | 1` for `i` in `[0, ranges.len())`: send body.
    /// * `ranges.len() << 1`: send trailer
    /// * `ranges.len() << 1 | 1`: end
    state: usize,
    part_headers: Vec<Vec<u8>>,
    ranges: Vec<std::ops::Range<u64>>,
    entity: Resource,
    remaining: u64,
}

impl MultipartStream {
    pub(crate) fn new(
        entity: Resource,
        part_headers: Vec<Vec<u8>>,
        ranges: Vec<std::ops::Range<u64>>,
        len: u64,
    ) -> Self {
        Self {
            cur: None,
            state: 0,
            part_headers,
            ranges,
            entity,
            remaining: len,
        }
    }

    pub(crate) fn remaining(&self) -> u64 {
        self.remaining
    }
}

/// The trailer after all `multipart/byteranges` body parts.
const PART_TRAILER: &[u8] = b"\r\n--B--\r\n";

impl futures_core::Stream for MultipartStream {
    type Item = Result<bytes::Bytes, crate::IOError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);
        loop {
            if let Some(ref mut cur) = this.cur {
                match cur.poll_next_unpin(cx) {
                    Poll::Ready(Some(Ok(d))) => {
                        this.remaining -= crate::as_u64(d.remaining());
                        return Poll::Ready(Some(Ok(d)));
                    }
                    Poll::Ready(Some(Err(e))) => {
                        // Fuse.
                        this.remaining = 0;
                        this.state = this.ranges.len() << 1 | 1;
                        return Poll::Ready(Some(Err(e)));
                    }
                    Poll::Ready(None) => {
                        this.cur = None;
                        this.state += 1;
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            let i = this.state >> 1;
            let odd = (this.state & 1) == 1;
            if i == this.ranges.len() && odd {
                debug_assert_eq!(this.remaining, 0);
                return Poll::Ready(None);
            }
            if i == this.ranges.len() {
                this.state += 1;
                this.remaining -= crate::as_u64(PART_TRAILER.len());
                return Poll::Ready(Some(Ok(PART_TRAILER.into())));
            } else if odd {
                let r = &this.ranges[i];
                this.cur = Some(crate::body::ExactLenStream::new(
                    r.end - r.start,
                    this.entity.get_range(r.clone()),
                ));
            } else {
                let v = std::mem::take(&mut this.part_headers[i]);
                this.state += 1;
                this.remaining -= crate::as_u64(v.len());
                return Poll::Ready(Some(Ok(v.into())));
            };
        }
    }
}
