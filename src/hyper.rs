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
        let serving_req = request_head(&req);

        Box::pin(async move {
            match served_dir_response(&served_dir, &serving_req).await {
                Ok(resp) => Ok(resp),
                Err(ServeFilesError::NotFound(None)) | Err(ServeFilesError::IsDirectory(_)) => {
                    Ok(status_response(StatusCode::NOT_FOUND))
                }
                Err(ServeFilesError::InvalidPath(_)) => {
                    Ok(status_response(StatusCode::BAD_REQUEST))
                }
                Err(_) => Ok(status_response(StatusCode::INTERNAL_SERVER_ERROR)),
            }
        })
    }
}

async fn served_dir_response<B>(
    served_dir: &ServedDir,
    req: &Request<B>,
) -> Result<Response<Body>, ServeFilesError> {
    let path = normalize_path(req.uri().path());
    match served_dir.get(path, req.headers()).await {
        Ok(entity) => Ok(crate::serving::serve(entity, req, StatusCode::OK)),
        Err(ServeFilesError::NotFound(Some(entity))) => {
            Ok(crate::serving::serve(entity, req, StatusCode::NOT_FOUND))
        }
        Err(err) => Err(err),
    }
}

fn normalize_path(path: &str) -> &str {
    path.strip_prefix('/').unwrap_or(path)
}

fn request_head<B>(req: &Request<B>) -> Request<()> {
    let mut request = Request::builder()
        .method(req.method().clone())
        .uri(req.uri().clone())
        .version(req.version())
        .body(())
        .expect("request head should be valid");
    *request.headers_mut() = req.headers().clone();
    request
}

fn status_response(status: StatusCode) -> Response<Body> {
    let reason = status.canonical_reason().unwrap_or("Unknown");
    Response::builder()
        .status(status)
        .body(Body::from(reason))
        .expect("status response should be valid")
}
