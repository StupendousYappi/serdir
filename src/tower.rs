use crate::served_dir::ServedDir;
use crate::{Body, ServeFilesError};
use futures_core::future::BoxFuture;
use http::{Request, Response, StatusCode};
use std::convert::Infallible;
use std::sync::Arc;
use std::task::{Context, Poll};
use tower::Service;

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
        Box::pin(async move {
            // We ignore the request body as we are serving static files.
            let (parts, _body) = req.into_parts();
            let req = Request::from_parts(parts, ());
            let path = req.uri().path();

            // ServedDir::get expects a relative path, strip the leading slash,
            // if any
            let path = if let Some(p) = path.strip_prefix('/') {
                p
            } else {
                path
            };

            // Handle root path "" -> "." for served_dir if needed?
            // If path was "/", it becomes "".
            // ServedDir::get: `match self.strip_prefix ...`.
            // `validate_path` checks `..`.
            // If path is empty, what happens? `validate_path` is fine.
            // `full_path = self.dirpath.join(path)`. join("") is same dir.
            // `find_file` calls `File::open`. `dirpath` is a directory.
            // `find_file` checks metadata. If dir, returns IsDirectory.
            // If `append_index_html` is true, it appends index.html.
            // So empty path ("") should work if it maps to the directory and index.html logic applies.

            match served_dir.get(path, req.headers()).await {
                Ok(entity) => {
                    let resp = crate::serving::serve(entity, &req, StatusCode::OK);
                    Ok(resp)
                }
                Err(e) => {
                    let status = match e {
                        ServeFilesError::NotFound(Some(entity)) => {
                            let resp = crate::serving::serve(entity, &req, StatusCode::NOT_FOUND);
                            return Ok(resp);
                        }
                        ServeFilesError::NotFound(None) => StatusCode::NOT_FOUND,
                        ServeFilesError::IsDirectory(_) => StatusCode::NOT_FOUND,
                        ServeFilesError::InvalidPath(_) => StatusCode::BAD_REQUEST,
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    let reason = status.canonical_reason().unwrap();

                    Ok(Response::builder()
                        .status(status)
                        .body(Body::from(reason))
                        .unwrap())
                }
            }
        })
    }
}
