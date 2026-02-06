use crate::served_dir::ServedDir;
use crate::{Body, ServeFilesError};
use bytes::Bytes;
use futures_core::future::BoxFuture;
use http::{Request, Response, StatusCode};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::BodyExt;
use std::convert::Infallible;
use std::sync::Arc;
use std::task::{Context, Poll};
use tower::Layer;
use tower::Service;

type TowerBody = UnsyncBoxBody<Bytes, crate::BoxError>;

/// A Tower layer that serves files from a [`ServedDir`] and otherwise
/// passes requests to the wrapped service.
#[derive(Clone)]
pub struct ServedDirLayer(Arc<ServedDir>);

impl ServedDirLayer {
    pub(crate) fn new(served_dir: ServedDir) -> Self {
        Self(Arc::new(served_dir))
    }
}

impl<S> Layer<S> for ServedDirLayer {
    type Service = ServedDirMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ServedDirMiddleware {
            served_dir: self.0.clone(),
            inner,
        }
    }
}

/// Tower middleware produced by [`ServedDirLayer`].
#[derive(Clone)]
pub struct ServedDirMiddleware<S> {
    served_dir: Arc<ServedDir>,
    inner: S,
}

/// A Tower service that serves files from a [`ServedDir`].
#[derive(Clone)]
pub struct ServedDirService(Arc<ServedDir>);

impl ServedDirService {
    pub(crate) fn new(served_dir: ServedDir) -> Self {
        Self(Arc::new(served_dir))
    }
}

impl<B> Service<Request<B>> for ServedDirService
where
    B: Send + 'static,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let served_dir = self.0.clone();
        let serving_req = crate::request_head(&req);
        Box::pin(async move {
            match served_dir.get_response(&serving_req).await {
                Ok(resp) => Ok(resp),
                Err(ServeFilesError::NotFound(None)) | Err(ServeFilesError::IsDirectory(_)) => {
                    Ok(crate::status_response(StatusCode::NOT_FOUND))
                }
                Err(ServeFilesError::InvalidPath(_)) => {
                    Ok(crate::status_response(StatusCode::BAD_REQUEST))
                }
                Err(_) => Ok(crate::status_response(StatusCode::INTERNAL_SERVER_ERROR)),
            }
        })
    }
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for ServedDirMiddleware<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    ReqBody: Send + 'static,
    ResBody: http_body::Body<Data = Bytes> + Send + 'static,
    ResBody::Error: Into<crate::BoxError> + 'static,
{
    type Response = Response<TowerBody>;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        let served_dir = self.served_dir.clone();
        let serving_req = crate::request_head(&req);
        // Drive the request with a clone while keeping `self.inner` available for readiness checks.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        Box::pin(async move {
            match served_dir.get_response(&serving_req).await {
                Ok(resp) => Ok(box_response(resp)),
                Err(ServeFilesError::NotFound(None))
                | Err(ServeFilesError::IsDirectory(_))
                | Err(ServeFilesError::InvalidPath(_)) => {
                    let response = inner.call(req).await?;
                    Ok(response.map(|body| body.map_err(Into::into).boxed_unsync()))
                }
                Err(_) => Ok(box_response(crate::status_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                ))),
            }
        })
    }
}

fn box_response(response: Response<Body>) -> Response<TowerBody> {
    response.map(|body| {
        body.map_err(|err| -> crate::BoxError { Box::new(err) })
            .boxed_unsync()
    })
}
