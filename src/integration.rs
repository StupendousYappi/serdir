use crate::served_dir::ServedDir;
use crate::Body;
#[cfg(feature = "tower")]
use crate::SerdirError;
use futures_core::future::BoxFuture;
#[cfg(feature = "tower")]
use http::StatusCode;
use http::{Request, Response};
use std::convert::Infallible;
use std::sync::Arc;

#[cfg(feature = "tower")]
use tower::BoxError;

/// Returns a Request based on the input request, but with an empty body.
pub(crate) fn request_head<B>(req: &Request<B>) -> Request<()> {
    let mut request = Request::builder()
        .method(req.method().clone())
        .uri(req.uri().clone())
        .version(req.version())
        .body(())
        .expect("request head should be valid");
    *request.headers_mut() = req.headers().clone();
    request
}

/// A Hyper service that serves files from a [`ServedDir`].
///
/// Requires the `hyper` feature.
#[cfg(feature = "hyper")]
#[derive(Clone)]
pub struct HyperService(Arc<ServedDir>);

#[cfg(feature = "hyper")]
impl HyperService {
    pub(crate) fn new(served_dir: ServedDir) -> Self {
        Self(Arc::new(served_dir))
    }
}

#[cfg(feature = "hyper")]
impl<B> hyper::service::Service<Request<B>> for HyperService
where
    B: Send + 'static,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn call(&self, req: Request<B>) -> Self::Future {
        let served_dir = self.0.clone();
        let serving_req = request_head(&req);

        Box::pin(async move { served_dir.get_response(&serving_req).await })
    }
}

/// A Tower layer that serves files from a [`ServedDir`] and otherwise
/// passes requests to the wrapped service.
///
/// Requires the `tower` feature.
#[cfg(feature = "tower")]
#[derive(Clone)]
pub struct TowerLayer(Arc<ServedDir>);

#[cfg(feature = "tower")]
impl TowerLayer {
    pub(crate) fn new(served_dir: ServedDir) -> Self {
        Self(Arc::new(served_dir))
    }
}

#[cfg(feature = "tower")]
impl<S> tower::Layer<S> for TowerLayer {
    type Service = ServedDirMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ServedDirMiddleware {
            served_dir: self.0.clone(),
            inner,
        }
    }
}

/// Tower middleware produced by [`TowerLayer`].
///
/// Requires the `tower` feature.
#[cfg(feature = "tower")]
#[derive(Clone)]
pub struct ServedDirMiddleware<S> {
    served_dir: Arc<ServedDir>,
    inner: S,
}

/// A Tower service that serves files from a [`ServedDir`].
///
/// Requires the `tower` feature.
#[cfg(feature = "tower")]
#[derive(Clone)]
pub struct TowerService(Arc<ServedDir>);

#[cfg(feature = "tower")]
impl TowerService {
    pub(crate) fn new(served_dir: ServedDir) -> Self {
        Self(Arc::new(served_dir))
    }
}

#[cfg(feature = "tower")]
impl<B> tower::Service<Request<B>> for TowerService
where
    B: Send + 'static,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let served_dir = self.0.clone();
        let serving_req = request_head(&req);
        Box::pin(async move { served_dir.get_response(&serving_req).await })
    }
}

#[cfg(feature = "tower")]
impl<S, ReqBody, ResBody> tower::Service<Request<ReqBody>> for ServedDirMiddleware<S>
where
    S: tower::Service<Request<ReqBody>, Response = Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    ReqBody: Send + 'static,
    ResBody: http_body::Body<Data = bytes::Bytes> + Send + 'static,
    ResBody::Error: Into<BoxError> + 'static,
{
    type Response = Response<http_body_util::combinators::UnsyncBoxBody<bytes::Bytes, BoxError>>;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        use http_body_util::BodyExt;

        let served_dir = self.served_dir.clone();
        let serving_req = request_head(&req);
        // Drive the request with a clone while keeping `self.inner` available for readiness checks.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        Box::pin(async move {
            match served_dir
                .get(serving_req.uri().path(), serving_req.headers())
                .await
            {
                Ok(entity) => Ok(box_response(
                    entity.serve_request(&serving_req, StatusCode::OK),
                )),
                Err(SerdirError::NotFound(Some(entity))) => Ok(box_response(
                    entity.serve_request(&serving_req, StatusCode::NOT_FOUND),
                )),
                Err(SerdirError::NotFound(None))
                | Err(SerdirError::IsDirectory(_))
                | Err(SerdirError::InvalidPath(_)) => {
                    let response = inner.call(req).await?;
                    Ok(response.map(|body| body.map_err(Into::into).boxed_unsync()))
                }
                Err(_) => {
                    let status = StatusCode::INTERNAL_SERVER_ERROR;
                    let reason = status.canonical_reason().unwrap();
                    let resp = Response::builder()
                        .status(status)
                        .body(Body::from(reason))
                        .expect("internal server error response should be valid");
                    Ok(resp.map(|body| body.map_err(Into::into).boxed_unsync()))
                }
            }
        })
    }
}

#[cfg(feature = "tower")]
fn box_response(
    response: Response<Body>,
) -> Response<http_body_util::combinators::UnsyncBoxBody<bytes::Bytes, BoxError>> {
    use http_body_util::BodyExt;

    response.map(|body| {
        body.map_err(|err| -> BoxError { Box::new(err) })
            .boxed_unsync()
    })
}
