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

pin_project_lite::pin_project! {
    /// A streaming [`http_body::Body`] implementation used by [`Resource`](crate::Resource)
    pub struct Body {
        #[pin]
        pub(crate) stream: BodyStream,
        pub(crate) on_complete: Option<OnComplete>,
    }
}

pub(crate) type OnComplete = Box<dyn FnOnce() + Send + Sync + 'static>;

impl http_body::Body for Body {
    type Data = bytes::Bytes;
    type Error = crate::IOError;

    #[inline]
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let poll = self
            .as_mut()
            .project()
            .stream
            .poll_next(cx)
            .map(|p| p.map(|o| o.map(http_body::Frame::data)));
        if matches!(&poll, Poll::Ready(None) | Poll::Ready(Some(Err(_)))) {
            self.complete();
        }
        poll
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
        Self {
            stream: BodyStream::Once { chunk: None },
            on_complete: None,
        }
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn with_on_complete(mut self, f: OnComplete) -> Self {
        self.on_complete = Some(f);
        self
    }

    #[inline]
    pub(crate) fn complete(self: Pin<&mut Self>) {
        if let Some(f) = Pin::into_inner(self).on_complete.take() {
            f();
        }
    }
}

impl From<&'static [u8]> for Body {
    #[inline]
    fn from(value: &'static [u8]) -> Self {
        Self {
            stream: BodyStream::Once {
                chunk: Some(Ok(value.into())),
            },

            on_complete: None,
        }
    }
}

impl From<&'static str> for Body {
    #[inline]
    fn from(value: &'static str) -> Self {
        Self {
            stream: BodyStream::Once {
                chunk: Some(Ok(value.as_bytes().into())),
            },

            on_complete: None,
        }
    }
}

impl From<Vec<u8>> for Body {
    #[inline]
    fn from(value: Vec<u8>) -> Self {
        Self {
            stream: BodyStream::Once {
                chunk: Some(Ok(value.into())),
            },

            on_complete: None,
        }
    }
}

impl From<String> for Body {
    #[inline]
    fn from(value: String) -> Self {
        Self {
            stream: BodyStream::Once {
                chunk: Some(Ok(value.into_bytes().into())),
            },

            on_complete: None,
        }
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
            s: crate::serving::MultipartStream,
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

const _: () = {
    fn _assert() {
        fn assert_bounds<T: Sync + Send>() {}
        assert_bounds::<Body>();
    }
};

#[cfg(test)]
mod tests {
    use futures_util::StreamExt as _;
    use http_body::Body as _;

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
    async fn on_complete_called_when_body_finishes() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let called = Arc::new(AtomicBool::new(false));
        let called2 = called.clone();
        let mut body = std::pin::pin!(Body::from("done").with_on_complete(Box::new(move || {
            called2.store(true, Ordering::SeqCst);
        })));

        while let Some(frame) =
            futures_util::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await
        {
            frame.unwrap();
        }

        assert!(called.load(Ordering::SeqCst));
    }
}
