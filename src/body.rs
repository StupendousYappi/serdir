// Copyright (c) 2023-2026 Greg Steffensen and the http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::{pin::Pin, task::Poll};

use bytes::Buf;
use futures_core::Stream;
use sync_wrapper::SyncWrapper;

type OnComplete = Box<dyn FnOnce() + Send + Sync + 'static>;

pin_project_lite::pin_project! {
    /// A streaming [`http_body::Body`] implementation used by [`Resource`](crate::Resource)
    pub struct Body {
        #[pin]
        pub(crate) stream: BodyStream,

        // A function called once the body stream has been consumed.
        on_complete: Option<OnComplete>,
    }

    impl PinnedDrop for Body {
        fn drop(this: Pin<&mut Self>) {
            if let Some(f) = this.project().on_complete.take() {
                f();
            }
        }
    }
}

impl http_body::Body for Body {
    type Data = bytes::Bytes;
    type Error = crate::IOError;

    #[inline]
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let result = self
            .as_mut()
            .project()
            .stream
            .poll_next(cx)
            .map(|p| p.map(|o| o.map(http_body::Frame::data)));
        let is_done = matches!(result, Poll::Ready(None) | Poll::Ready(Some(Err(_))));
        if is_done {
            if let Some(f) = self.on_complete.take() {
                f();
            }
        }
        result
    }

    fn size_hint(&self) -> http_body::SizeHint {
        match &self.stream {
            BodyStream::Once { chunk: Some(Ok(d)) } => http_body::SizeHint::with_exact(
                u64::try_from(d.remaining()).expect("usize should fit in u64"),
            ),
            BodyStream::Once { .. } => http_body::SizeHint::with_exact(0),
            BodyStream::ExactLen { s } => http_body::SizeHint::with_exact(s.remaining),
            BodyStream::Multipart { s } => http_body::SizeHint::with_exact(s.remaining()),
        }
    }

    fn is_end_stream(&self) -> bool {
        match &self.stream {
            BodyStream::Once { chunk } => chunk.is_none(),
            BodyStream::ExactLen { s } => s.remaining == 0,
            BodyStream::Multipart { s } => s.remaining() == 0,
        }
    }
}

impl Body {
    /// Returns a 0-byte body.
    #[inline]
    pub fn empty() -> Self {
        Self::new_once(None)
    }

    #[inline]
    pub(crate) fn new_once(chunk: Option<Result<bytes::Bytes, crate::IOError>>) -> Self {
        Self {
            stream: BodyStream::Once { chunk },
            on_complete: None,
        }
    }

    #[inline]
    pub(crate) fn new_exact_len(
        len: u64,
        stream: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, crate::IOError>> + Send>>,
    ) -> Self {
        Self {
            stream: BodyStream::ExactLen {
                s: ExactLenStream::new(len, stream),
            },
            on_complete: None,
        }
    }

    #[inline]
    pub(crate) fn new_multipart(
        entity: crate::Resource,
        part_headers: Vec<Vec<u8>>,
        ranges: Vec<std::ops::Range<u64>>,
        len: u64,
    ) -> Self {
        Self {
            stream: BodyStream::Multipart {
                s: MultipartStream::new(entity, part_headers, ranges, len),
            },
            on_complete: None,
        }
    }

    #[allow(dead_code)]
    #[inline]
    pub(crate) fn on_complete(
        mut self,
        on_complete: impl FnOnce() + Send + Sync + 'static,
    ) -> Self {
        self.on_complete = Some(Box::new(on_complete));
        self
    }
}

impl From<&'static [u8]> for Body {
    #[inline]
    fn from(value: &'static [u8]) -> Self {
        Self::new_once(Some(Ok(value.into())))
    }
}

impl From<&'static str> for Body {
    #[inline]
    fn from(value: &'static str) -> Self {
        Self::new_once(Some(Ok(value.as_bytes().into())))
    }
}

impl From<Vec<u8>> for Body {
    #[inline]
    fn from(value: Vec<u8>) -> Self {
        Self::new_once(Some(Ok(value.into())))
    }
}

impl From<String> for Body {
    #[inline]
    fn from(value: String) -> Self {
        Self::new_once(Some(Ok(value.into_bytes().into())))
    }
}

pin_project_lite::pin_project! {
    #[project = BodyStreamProj]
    pub(crate) enum BodyStream {
        Once {
            chunk: Option<Result<bytes::Bytes, crate::IOError>>,
        },
        ExactLen {
            #[pin]
            s: ExactLenStream,
        },
        Multipart {
            #[pin]
            s: MultipartStream,
        },
    }
}

impl Stream for BodyStream {
    type Item = Result<bytes::Bytes, crate::IOError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<bytes::Bytes, crate::IOError>>> {
        match self.project() {
            BodyStreamProj::Once { chunk } => Poll::Ready(chunk.take()),
            BodyStreamProj::ExactLen { s } => s.poll_next(cx),
            BodyStreamProj::Multipart { s } => s.poll_next(cx),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct StreamTooShortError {
    remaining: u64,
}

impl std::fmt::Display for StreamTooShortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "stream ended with {} bytes still expected",
            self.remaining
        )
    }
}

impl std::error::Error for StreamTooShortError {}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct StreamTooLongError {
    extra: u64,
}

impl std::fmt::Display for StreamTooLongError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "stream returned (at least) {} bytes more than expected",
            self.extra
        )
    }
}

impl std::error::Error for StreamTooLongError {}

pub(crate) struct ExactLenStream {
    #[allow(clippy::type_complexity)]
    stream: SyncWrapper<Pin<Box<dyn Stream<Item = Result<bytes::Bytes, crate::IOError>> + Send>>>,
    remaining: u64,
}

impl ExactLenStream {
    pub(crate) fn new(
        len: u64,
        stream: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, crate::IOError>> + Send>>,
    ) -> Self {
        Self {
            stream: SyncWrapper::new(stream),
            remaining: len,
        }
    }
}

impl futures_core::Stream for ExactLenStream {
    type Item = Result<bytes::Bytes, crate::IOError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<bytes::Bytes, crate::IOError>>> {
        let this = Pin::into_inner(self);
        match this.stream.get_mut().as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(d))) => {
                let d_len = crate::as_u64(d.remaining());
                let new_rem = this.remaining.checked_sub(d_len);
                if let Some(new_rem) = new_rem {
                    this.remaining = new_rem;
                    Poll::Ready(Some(Ok(d)))
                } else {
                    let remaining = std::mem::take(&mut this.remaining); // fuse.
                    Poll::Ready(Some(Err(crate::IOError::other(StreamTooLongError {
                        extra: d_len - remaining,
                    }))))
                }
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => {
                if this.remaining != 0 {
                    let remaining = std::mem::take(&mut this.remaining); // fuse.
                    return Poll::Ready(Some(Err(crate::IOError::other(StreamTooShortError {
                        remaining,
                    }))));
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

pub(crate) struct MultipartStream {
    /// The current part's body stream, set in "send body" state.
    cur: Option<ExactLenStream>,

    /// Current state:
    ///
    /// * `i << 1` for `i` in `[0, ranges.len())`: send part headers.
    /// * `i << 1 | 1` for `i` in `[0, ranges.len())`: send body.
    /// * `ranges.len() << 1`: send trailer
    /// * `ranges.len() << 1 | 1`: end
    state: usize,
    part_headers: Vec<Vec<u8>>,
    ranges: Vec<std::ops::Range<u64>>,
    entity: crate::Resource,
    remaining: u64,
}

impl MultipartStream {
    pub(crate) fn new(
        entity: crate::Resource,
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
pub(crate) const PART_TRAILER: &[u8] = b"\r\n--B--\r\n";

impl futures_core::Stream for MultipartStream {
    type Item = Result<bytes::Bytes, crate::IOError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);
        loop {
            if let Some(ref mut cur) = this.cur {
                match Pin::new(cur).poll_next(cx) {
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
                this.cur = Some(ExactLenStream::new(
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

const _: () = {
    fn _assert() {
        fn assert_bounds<T: Sync + Send>() {}
        assert_bounds::<Body>();
    }
};

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::SystemTime;

    use futures_util::StreamExt as _;
    use http_body_util::BodyExt as _;

    use super::*;

    #[tokio::test]
    async fn correct_exact_len_stream() {
        let inner = futures_util::stream::iter(vec![Ok("h".into()), Ok("ello".into())]);
        let mut exact_len = std::pin::pin!(ExactLenStream::new(5, Box::pin(inner)));
        assert_eq!(exact_len.remaining, 5);
        let frame = exact_len.next().await.unwrap().unwrap();
        assert_eq!(frame.remaining(), 1);
        assert_eq!(exact_len.remaining, 4);
        let frame = exact_len.next().await.unwrap().unwrap();
        assert_eq!(frame.remaining(), 4);
        assert_eq!(exact_len.remaining, 0);
        assert!(exact_len.next().await.is_none()); // end of stream.
        assert!(exact_len.next().await.is_none()); // fused.
    }

    #[tokio::test]
    async fn short_exact_len_stream() {
        let inner = futures_util::stream::iter(vec![Ok("hello".into())]);
        let mut exact_len = std::pin::pin!(ExactLenStream::new(10, Box::pin(inner)));
        assert_eq!(exact_len.remaining, 10);
        let frame = exact_len.next().await.unwrap().unwrap();
        assert_eq!(frame.remaining(), 5);
        assert_eq!(exact_len.remaining, 5);
        let err: crate::IOError = exact_len.next().await.unwrap().unwrap_err();
        let err = err.downcast::<StreamTooShortError>().unwrap();
        assert_eq!(err, StreamTooShortError { remaining: 5 });
        assert!(exact_len.next().await.is_none()); // fused.
    }

    #[tokio::test]
    async fn long_exact_len_stream() {
        let inner = futures_util::stream::iter(vec![Ok("h".into()), Ok("ello".into())]);
        let mut exact_len = std::pin::pin!(ExactLenStream::new(3, Box::pin(inner)));
        assert_eq!(exact_len.remaining, 3);
        let frame = exact_len.next().await.unwrap().unwrap();
        assert_eq!(frame.remaining(), 1);
        assert_eq!(exact_len.remaining, 2);
        let err = exact_len.next().await.unwrap().unwrap_err();
        let err = err.downcast::<StreamTooLongError>().unwrap();
        assert_eq!(err, StreamTooLongError { extra: 2 });
        assert!(exact_len.next().await.is_none()); // fused.
    }

    #[tokio::test]
    async fn on_complete_called_once_for_exact_len_body() {
        let calls = Arc::new(AtomicUsize::new(0));
        let on_complete_calls = Arc::clone(&calls);

        let inner = futures_util::stream::iter(vec![Ok("he".into()), Ok("llo".into())]);
        let mut body = Body::new_exact_len(5, Box::pin(inner)).on_complete(move || {
            on_complete_calls.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(body.frame().await.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(body.frame().await.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(body.frame().await.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(body.frame().await.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn on_complete_called_once_for_multipart_body() {
        let calls = Arc::new(AtomicUsize::new(0));
        let on_complete_calls = Arc::clone(&calls);

        let entity = crate::ResourceBuilder::for_bytes("abcdef", SystemTime::UNIX_EPOCH).build();
        let part_headers = vec![b"part-one\r\n".to_vec(), b"part-two\r\n".to_vec()];
        let ranges = vec![0..2, 3..6];
        let len = 10 + 10 + 2 + 3 + 9;
        let mut body =
            Body::new_multipart(entity, part_headers, ranges, len).on_complete(move || {
                on_complete_calls.fetch_add(1, Ordering::SeqCst);
            });

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(body.frame().await.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        while body.frame().await.is_some() {}
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(body.frame().await.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn on_complete_called_once_when_exact_len_body_dropped_before_consumed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let on_complete_calls = Arc::clone(&calls);

        let inner = futures_util::stream::iter(vec![Ok("hello".into())]);
        let body = Body::new_exact_len(5, Box::pin(inner)).on_complete(move || {
            on_complete_calls.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        drop(body);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn on_complete_called_once_when_multipart_body_dropped_before_consumed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let on_complete_calls = Arc::clone(&calls);

        let entity = crate::ResourceBuilder::for_bytes("abcdef", SystemTime::UNIX_EPOCH).build();
        let part_headers = vec![b"part-one\r\n".to_vec(), b"part-two\r\n".to_vec()];
        let ranges = vec![0..2, 3..6];
        let len = 10 + 10 + 2 + 3 + 9;
        let body = Body::new_multipart(entity, part_headers, ranges, len).on_complete(move || {
            on_complete_calls.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        drop(body);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
