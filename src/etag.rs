// Copyright (c) 2016-2026 Greg Steffensen and the http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::FileInfo;
use http::header::{self, HeaderMap, HeaderValue};
use sieve_cache::ShardedSieveCache;
use std::fs::File;
use std::io;

const CACHE_SIZE: usize = 256;

/// A strong HTTP ETag (entity tag) value based on hashing the file contents.
///
/// By default, ETag values are created by hashing the file contents with
/// [`rapidhash`](https://docs.rs/rapidhash/latest/rapidhash/), but the hashing
/// algorithm can be customized by calling the
/// [`ServedDirBuilder::file_hasher`](crate::ServedDirBuilder::file_hasher)
/// method.
///
/// ETags emitted by this crate are always strong, meaning that they are
/// compatible with range requests, but will not allow reuse of responses for
/// different encodings (i.e. compression algorithms).
#[derive(Hash, Eq, PartialEq, Clone, Copy, Debug)]
pub struct ETag(pub u64);

/// Function pointer type used to calculate ETag hash values from opened files.
///
/// The function should return a u64 based on hashing the complete contents of the file,
/// or None if no ETag should be used for this file.
pub type FileHasher = fn(&File) -> Result<Option<u64>, io::Error>;

/// A cache for ETag values, indexed by file metadata.
pub(crate) struct EtagCache(ShardedSieveCache<FileInfo, Option<ETag>>);

impl EtagCache {
    pub(crate) fn new() -> Self {
        let cache = ShardedSieveCache::new(CACHE_SIZE).expect("etag cache capacity must be valid");
        Self(cache)
    }

    pub(crate) fn get_or_insert(
        &self,
        info: FileInfo,
        file: &File,
        hasher: FileHasher,
    ) -> Result<Option<ETag>, io::Error> {
        if let Some(etag) = self.0.get(&info) {
            return Ok(etag);
        }

        let etag = hasher(file).map(|hash| hash.map(ETag::from))?;
        self.0.insert(info, etag);
        Ok(etag)
    }
}

impl ETag {
    fn to_bytes(self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        let hex_chars = b"0123456789abcdef";
        let val = self.0;
        for (i, slot) in buf.iter_mut().enumerate() {
            let nibble = (val >> ((15 - i) * 4)) & 0xf;
            *slot = hex_chars[nibble as usize];
        }
        buf
    }
}

impl From<ETag> for HeaderValue {
    fn from(etag: ETag) -> Self {
        let mut buf = [0u8; 18];
        buf[0] = b'"';
        buf[17] = b'"';
        let bytes = etag.to_bytes();
        buf[1..17].copy_from_slice(&bytes);
        HeaderValue::from_bytes(&buf).expect("failed to serialize etag")
    }
}

impl From<u64> for ETag {
    fn from(hash: u64) -> Self {
        ETag(hash)
    }
}

impl From<ETag> for u64 {
    fn from(etag: ETag) -> Self {
        etag.0
    }
}

/// Performs weak validation of two etags (such as `B"W/\"foo\""`` or `B"\"bar\""``)
/// as in [RFC 7232 section 2.3.2](https://datatracker.ietf.org/doc/html/rfc7232#section-2.3.2).
pub fn weak_eq(a: &[u8], b: &[u8]) -> bool {
    let a = a.strip_prefix(b"W/").unwrap_or(a);
    let b = b.strip_prefix(b"W/").unwrap_or(b);
    a == b
}

/// Performs strong validation of two etags (such as `B"W/\"foo\""` or `B"\"bar\""``)
/// as in [RFC 7232 section 2.3.2](https://datatracker.ietf.org/doc/html/rfc7232#section-2.3.2).
pub fn strong_eq(a: &[u8], b: &[u8]) -> bool {
    a == b && !a.starts_with(b"W/")
}

/// Matches a `1#entity-tag`, where `#` is as specified in RFC 7230 section 7.
///
/// > A construct `#` is defined, similar to `*`, for defining
/// > comma-delimited lists of elements.  The full form is `<n>#<m>element`
/// > indicating at least `<n>` and at most `<m>` elements, each separated by a
/// > single comma (`,`) and optional whitespace (OWS).
///
/// > `OWS = *( SP / HTAB )`
struct List<'a> {
    remaining: &'a [u8],
    corrupt: bool,
}

impl List<'_> {
    fn from(l: &'_ [u8]) -> List<'_> {
        List {
            remaining: l,
            corrupt: false,
        }
    }
}

impl<'a> Iterator for List<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }

        // If on an etag, find its end. Note the `"` can't be escaped, simplifying matters.
        let end = if self.remaining.starts_with(b"W/\"") {
            self.remaining[3..]
                .iter()
                .position(|&b| b == b'"')
                .map(|p| p + 3)
        } else if self.remaining.starts_with(b"\"") {
            self.remaining[1..]
                .iter()
                .position(|&b| b == b'"')
                .map(|p| p + 1)
        } else {
            None
        };
        let Some(end) = end else {
            self.corrupt = true;
            return None;
        };
        let (etag, mut rem) = self.remaining.split_at(end + 1);
        if let [b',', r @ ..] = rem {
            rem = r;
            while let [b' ' | b'\t', tail @ ..] = rem {
                rem = tail;
            }
        }
        self.remaining = rem;
        Some(etag)
    }
}

/// Returns true if `req` doesn't have an `If-None-Match` header matching `req`.
pub fn none_match(etag: &Option<HeaderValue>, req_hdrs: &HeaderMap) -> Option<bool> {
    let m = match req_hdrs.get(header::IF_NONE_MATCH) {
        None => return None,
        Some(m) => m.as_bytes(),
    };
    if m == b"*" {
        return Some(false);
    }
    let mut none_match = true;
    if let Some(ref some_etag) = *etag {
        let mut items = List::from(m);
        for item in &mut items {
            // RFC 7232 section 3.2: A recipient MUST use the weak comparison function when
            // comparing entity-tags for If-None-Match
            if none_match && weak_eq(item, some_etag.as_bytes()) {
                none_match = false;
            }
        }
        if items.corrupt {
            return None; // ignore the header.
        }
    }
    Some(none_match)
}

/// Returns true if `req` has no `If-Match` header or one which matches `etag`.
pub fn any_match(etag: &Option<HeaderValue>, req_hdrs: &HeaderMap) -> Result<bool, &'static str> {
    let m = match req_hdrs.get(header::IF_MATCH) {
        None => return Ok(true),
        Some(m) => m.as_bytes(),
    };
    if m == b"*" {
        // The absent header and "If-Match: *" cases differ only when there is no entity to serve.
        // We always have an entity to serve, so consider them identical.
        return Ok(true);
    }
    let mut any_match = false;
    if let Some(ref some_etag) = *etag {
        let mut items = List::from(m);
        for item in &mut items {
            if !any_match && strong_eq(item, some_etag.as_bytes()) {
                any_match = true;
            }
        }
        if items.corrupt {
            return Err("Unparseable If-Match header");
        }
    }
    Ok(any_match)
}

#[cfg(test)]
mod tests {
    use super::List;

    #[test]
    fn weak_eq() {
        assert!(super::weak_eq(b"\"foo\"", b"\"foo\""));
        assert!(!super::weak_eq(b"\"foo\"", b"\"bar\""));
        assert!(super::weak_eq(b"W/\"foo\"", b"\"foo\""));
        assert!(super::weak_eq(b"\"foo\"", b"W/\"foo\""));
        assert!(super::weak_eq(b"W/\"foo\"", b"W/\"foo\""));
        assert!(!super::weak_eq(b"W/\"foo\"", b"W/\"bar\""));
    }

    #[test]
    fn strong_eq() {
        assert!(super::strong_eq(b"\"foo\"", b"\"foo\""));
        assert!(!super::strong_eq(b"\"foo\"", b"\"bar\""));
        assert!(!super::strong_eq(b"W/\"foo\"", b"\"foo\""));
        assert!(!super::strong_eq(b"\"foo\"", b"W/\"foo\""));
        assert!(!super::strong_eq(b"W/\"foo\"", b"W/\"foo\""));
        assert!(!super::strong_eq(b"W/\"foo\"", b"W/\"bar\""));
    }

    #[test]
    fn empty_list() {
        let mut l = List::from(b"");
        assert_eq!(l.next(), None);
        assert!(!l.corrupt);
    }

    #[test]
    fn nonempty_list() {
        let mut l = List::from(b"\"foo\", \tW/\"bar\",W/\"baz\"");
        assert_eq!(l.next(), Some(&b"\"foo\""[..]));
        assert_eq!(l.next(), Some(&b"W/\"bar\""[..]));
        assert_eq!(l.next(), Some(&b"W/\"baz\""[..]));
        assert_eq!(l.next(), None);
        assert!(!l.corrupt);
    }

    #[test]
    fn comma_in_etag() {
        let mut l = List::from(b"\"foo, bar\", \"baz\"");
        assert_eq!(l.next(), Some(&b"\"foo, bar\""[..]));
        assert_eq!(l.next(), Some(&b"\"baz\""[..]));
        assert_eq!(l.next(), None);
        assert!(!l.corrupt);
    }

    #[test]
    fn corrupt_list() {
        let mut l = List::from(b"\"foo\", bar");
        assert_eq!(l.next(), Some(&b"\"foo\""[..]));
        assert_eq!(l.next(), None);
        assert!(l.corrupt);
    }
}
