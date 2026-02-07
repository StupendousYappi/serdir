use crate::served_dir::ServedDir;
use crate::{Body, ServeFilesError};
use futures_core::future::BoxFuture;
use http::{Request, Response, StatusCode};
use std::convert::Infallible;
use std::sync::Arc;

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
#[cfg(feature = "hyper")]
#[derive(Clone)]
pub struct ServedDirHyperService(Arc<ServedDir>);

#[cfg(feature = "hyper")]
impl ServedDirHyperService {
    pub(crate) fn new(served_dir: ServedDir) -> Self {
        Self(Arc::new(served_dir))
    }
}

#[cfg(feature = "hyper")]
impl<B> hyper::service::Service<Request<B>> for ServedDirHyperService
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
#[cfg(feature = "tower")]
#[derive(Clone)]
pub struct ServedDirLayer(Arc<ServedDir>);

#[cfg(feature = "tower")]
impl ServedDirLayer {
    pub(crate) fn new(served_dir: ServedDir) -> Self {
        Self(Arc::new(served_dir))
    }
}

#[cfg(feature = "tower")]
impl<S> tower::Layer<S> for ServedDirLayer {
    type Service = ServedDirMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ServedDirMiddleware {
            served_dir: self.0.clone(),
            inner,
        }
    }
}

/// Tower middleware produced by [`ServedDirLayer`].
#[cfg(feature = "tower")]
#[derive(Clone)]
pub struct ServedDirMiddleware<S> {
    served_dir: Arc<ServedDir>,
    inner: S,
}

/// A Tower service that serves files from a [`ServedDir`].
#[cfg(feature = "tower")]
#[derive(Clone)]
pub struct ServedDirService(Arc<ServedDir>);

#[cfg(feature = "tower")]
impl ServedDirService {
    pub(crate) fn new(served_dir: ServedDir) -> Self {
        Self(Arc::new(served_dir))
    }
}

#[cfg(feature = "tower")]
impl<B> tower::Service<Request<B>> for ServedDirService
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
    ResBody::Error: Into<crate::BoxError> + 'static,
{
    type Response =
        Response<http_body_util::combinators::UnsyncBoxBody<bytes::Bytes, crate::BoxError>>;
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
                Ok(entity) => Ok(box_response(crate::serving::serve(
                    entity,
                    &serving_req,
                    StatusCode::OK,
                ))),
                Err(ServeFilesError::NotFound(Some(entity))) => Ok(box_response(
                    crate::serving::serve(entity, &serving_req, StatusCode::NOT_FOUND),
                )),
                Err(ServeFilesError::NotFound(None))
                | Err(ServeFilesError::IsDirectory(_))
                | Err(ServeFilesError::InvalidPath(_)) => {
                    let response = inner.call(req).await?;
                    Ok(response.map(|body| body.map_err(Into::into).boxed_unsync()))
                }
                Err(_) => Ok(box_response(ServedDir::make_status_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                ))),
            }
        })
    }
}

#[cfg(feature = "tower")]
fn box_response(
    response: Response<Body>,
) -> Response<http_body_util::combinators::UnsyncBoxBody<bytes::Bytes, crate::BoxError>> {
    use http_body_util::BodyExt;

    response.map(|body| {
        body.map_err(|err| -> crate::BoxError { Box::new(err) })
            .boxed_unsync()
    })
}
