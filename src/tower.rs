use crate::served_dir::ServedDir;
use crate::ServeFilesError;
use http::{Request, Response, StatusCode};
use http_body::Body;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
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

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
type BoxBody = Box<dyn Body<Data = bytes::Bytes, Error = Box<dyn std::error::Error + Send + Sync>> + Send + Sync + Unpin>;

impl<B> Service<Request<B>> for ServedDirService
where
    B: Send + 'static,
{
    type Response = Response<BoxBody>;
    type Error = Infallible;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

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

            // ServedDir::get expects the path to be relative to the served directory,
            // but req.uri().path() is absolute. However, ServedDir::get handles
            // strip_prefix if configured.
            // Also ServedDir::validate_path checks for absolute path if it starts with /.
            // Wait, ServedDir::get implementation:
            // if path == prefix => ".", prefix removal...
            // validate_path checks if path is absolute: `if path.as_bytes().first() == Some(&b'/')`.
            // Request URI path usually starts with /.
            // So we might need to strip the leading slash?
            // Let's check ServedDir::get again.
            // "This method will return an error with kind `ErrorKind::InvalidInput` if the path is invalid."
            // In `src/served_dir.rs`:
            // fn validate_path(&self, path: &str) ...
            // if path.as_bytes().first() == Some(&b'/') { return Err(... "path is absolute") }
            // So ServedDir::get expects a relative path!
            // But Request URI path is absolute (starts with /).
            // So we MUST strip the leading slash.

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
                    let resp = crate::serving::serve(entity, &req);
                    let (parts, body) = resp.into_parts();

                    // Map the body error to Box<dyn Error + Send + Sync>
                    use http_body_util::BodyExt;
                    let boxed_body: BoxBody = Box::new(body
                        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>));

                    Ok(Response::from_parts(parts, boxed_body))
                },
                Err(e) => {
                    let status = match e {
                        ServeFilesError::NotFound => StatusCode::NOT_FOUND,
                        ServeFilesError::IsDirectory(_) => StatusCode::NOT_FOUND,
                        ServeFilesError::InvalidPath(_) => StatusCode::BAD_REQUEST,
                        ServeFilesError::ConfigError(_) => StatusCode::INTERNAL_SERVER_ERROR,
                        ServeFilesError::CompressionError(_, _) => StatusCode::INTERNAL_SERVER_ERROR,
                        ServeFilesError::IOError(_) => StatusCode::INTERNAL_SERVER_ERROR,
                    };

                    let body_content = e.to_string();
                    use http_body_util::BodyExt;
                    let body = http_body_util::Full::new(bytes::Bytes::from(body_content))
                        .map_err(|_: Infallible| unreachable!());
                    let boxed_body: BoxBody = Box::new(body);

                    Ok(Response::builder().status(status).body(boxed_body).unwrap())
                }
            }
        })
    }
}
