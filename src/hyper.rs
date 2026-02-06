use crate::served_dir::ServedDir;
use crate::{Body, ServeFilesError};
use futures_core::future::BoxFuture;
use http::{Request, Response, StatusCode};
use hyper::service::Service;
use std::convert::Infallible;
use std::sync::Arc;

/// A Hyper service that serves files from a [`ServedDir`].
#[derive(Clone)]
pub struct ServedDirHyperService(Arc<ServedDir>);

impl ServedDirHyperService {
    pub(crate) fn new(served_dir: ServedDir) -> Self {
        Self(Arc::new(served_dir))
    }
}

impl<B> Service<Request<B>> for ServedDirHyperService
where
    B: Send + 'static,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn call(&self, req: Request<B>) -> Self::Future {
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
