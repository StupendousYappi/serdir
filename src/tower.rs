use crate::served_dir::ServedDir;
use crate::{BoxError, ServeFilesError};
use bytes::Bytes;
use http::HeaderValue;
use std::sync::Arc;
use std::task::{Context, Poll};
use tower::Service;
use std::future::Future;
use std::pin::Pin;

/// A tower service that serves files from a directory.
#[derive(Clone)]
pub struct ServedDirService {
    inner: Arc<ServedDir>,
}

impl ServedDirService {
    pub(crate) fn new(served_dir: ServedDir) -> Self {
        Self {
            inner: Arc::new(served_dir),
        }
    }
}

impl<B> Service<http::Request<B>> for ServedDirService
where
    B: Send + Sync + 'static,
{
    type Response = http::Response<
        Box<dyn http_body::Body<Data = Bytes, Error = BoxError> + Send + Sync + Unpin>,
    >;
    type Error = std::convert::Infallible;
    type Future = Pin<
        Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + Sync>,
    >;

    fn poll_ready(
        &mut self,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let inner = self.inner.clone();
        Box::pin(async move {
            let (parts, _) = req.into_parts();
            let req = http::Request::from_parts(parts, ());
            let path = req.uri().path();
            let path = path.strip_prefix('/').unwrap_or(path);

            match inner.get(path, req.headers()).await {
                Ok(entity) => {
                    let resp = crate::serve(entity, &req);
                    let (parts, body) = resp.into_parts();
                    let boxed_body: Box<
                        dyn http_body::Body<Data = Bytes, Error = BoxError> + Send + Sync + Unpin,
                    > = Box::new(body);
                    Ok(http::Response::from_parts(parts, boxed_body))
                }
                Err(e) => {
                    let status = match e {
                        ServeFilesError::NotFound => http::StatusCode::NOT_FOUND,
                        ServeFilesError::InvalidPath(_) => http::StatusCode::BAD_REQUEST,
                        ServeFilesError::NotAFile(_) => http::StatusCode::NOT_FOUND,
                        ServeFilesError::NotADirectory(_) => http::StatusCode::INTERNAL_SERVER_ERROR,
                        ServeFilesError::IOError(_) => http::StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    let body_str = e.to_string();
                    let body = crate::Body::from(body_str);
                    let boxed_body: Box<
                        dyn http_body::Body<Data = Bytes, Error = BoxError> + Send + Sync + Unpin,
                    > = Box::new(body);

                    let mut res = http::Response::new(boxed_body);
                    *res.status_mut() = status;
                    res.headers_mut().insert(
                        http::header::CONTENT_TYPE,
                        HeaderValue::from_static("text/plain"),
                    );
                    Ok(res)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::served_dir::ServedDir;
    use http_body_util::BodyExt;
    use tempfile::TempDir;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_tower_service() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::fs::write(path.join("index.html"), "Hello, World!").unwrap();

        let served_dir = ServedDir::builder(path).unwrap().build();
        let mut service = ServedDirService::new(served_dir);

        let req = http::Request::builder()
            .uri("http://localhost/index.html")
            .body(())
            .unwrap();

        let resp = service.call(req).await.unwrap();

        assert_eq!(resp.status(), http::StatusCode::OK);
        assert_eq!(
            resp.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "text/html"
        );

        let body = resp.into_body();
        let collected = BodyExt::collect(body).await.unwrap();
        let bytes = collected.to_bytes();
        assert_eq!(bytes, "Hello, World!");
    }
}
