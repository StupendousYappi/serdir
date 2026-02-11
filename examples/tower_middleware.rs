// Copyright (c) 2020 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Serves a directory on `http://127.0.0.1:1337/` using `ServedDir::into_tower_layer`.

mod common;

use anyhow::{bail, Context, Result};
use common::Config;
use http::header::{self, HeaderValue};
use hyper::server::conn;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use std::fmt::Write;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tower::{Layer, Service};

use serdir::Body;

type ResponseFuture =
    Pin<Box<dyn Future<Output = Result<http::Response<Body>, std::convert::Infallible>> + Send>>;

#[derive(Clone)]
struct DirectoryFallbackService {
    root: Arc<PathBuf>,
}

impl DirectoryFallbackService {
    fn new(root: PathBuf) -> Self {
        Self {
            root: Arc::new(root),
        }
    }
}

impl Service<http::Request<hyper::body::Incoming>> for DirectoryFallbackService {
    type Response = http::Response<Body>;
    type Error = std::convert::Infallible;
    type Future = ResponseFuture;

    fn poll_ready(&mut self, _cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<hyper::body::Incoming>) -> Self::Future {
        let root = self.root.clone();
        Box::pin(async move {
            Ok(match path_for_request(root.as_path(), req.uri().path()) {
                Some(path) if path.is_dir() => directory_listing(&req, &path),
                _ => not_found_response(),
            })
        })
    }
}

fn path_for_request(root: &Path, request_path: &str) -> Option<PathBuf> {
    let rel = request_path.trim_start_matches('/');
    let path = Path::new(rel);

    for component in path.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return None;
        }
    }

    Some(root.join(path))
}

fn directory_listing(
    req: &http::Request<hyper::body::Incoming>,
    path: &Path,
) -> http::Response<Body> {
    if !req.uri().path().ends_with("/") {
        let mut loc = ::bytes::BytesMut::with_capacity(req.uri().path().len() + 1);
        write!(loc, "{}/", req.uri().path()).unwrap();
        let loc = HeaderValue::from_maybe_shared(loc.freeze()).unwrap();
        return http::Response::builder()
            .status(http::StatusCode::MOVED_PERMANENTLY)
            .header(http::header::LOCATION, loc)
            .body(serdir::Body::empty())
            .unwrap();
    }

    let mut listing = String::new();
    listing.push_str("<!DOCTYPE html>\n<title>directory listing</title>\n<ul>\n");

    let mut ents =
        match std::fs::read_dir(path).and_then(|iter| iter.collect::<Result<Vec<_>, _>>()) {
            Ok(ents) => ents,
            Err(_) => return not_found_response(),
        };
    ents.sort_unstable_by(|a, b| a.file_name().cmp(&b.file_name()));

    for ent in ents {
        let file_name = ent.file_name();
        let p = match file_name.to_str() {
            None => continue, // skip non-UTF-8
            Some(".") => continue,
            Some(p) => p,
        };
        if p == ".." && req.uri().path() == "/" {
            continue;
        }

        listing.push_str("<li><a href=\"");
        listing.push_str(&htmlescape::encode_minimal(p));
        let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            listing.push('/');
        }
        listing.push_str("\">");
        listing.push_str(&htmlescape::encode_minimal(p));
        if is_dir {
            listing.push('/');
        }
        listing.push_str("</a>\n");
    }

    listing.push_str("</ul>\n");
    let mut resp = http::Response::new(serdir::Body::from(listing));
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
    resp
}

fn not_found_response() -> http::Response<Body> {
    http::Response::builder()
        .status(http::StatusCode::NOT_FOUND)
        .body(serdir::Body::from("Not Found"))
        .unwrap()
}

#[tokio::main]
async fn main() -> Result<()> {
    run().await
}

async fn run() -> Result<()> {
    env_logger::init();
    let config = Config::from_env();
    let served_dir = config.into_builder()?.build();
    let root_path = served_dir.dir().to_path_buf();
    let served_dir_display = root_path.display().to_string();
    let layer = served_dir.into_tower_layer();
    let service = layer.layer(DirectoryFallbackService::new(root_path));

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 1337));
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    println!(
        "Serving {} on http://{}",
        served_dir_display,
        listener
            .local_addr()
            .context("failed to get listener address")?
    );
    let mut workers: JoinSet<Result<()>> = JoinSet::new();
    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (tcp, _) = accept_result.context("accept failed")?;
                let service = service.clone();
                workers.spawn(async move {
                    tcp.set_nodelay(true)
                        .context("failed to set TCP_NODELAY")?;
                    let io = TokioIo::new(tcp);
                    let hyper_service = TowerToHyperService::new(service);
                    conn::http1::Builder::new()
                        .serve_connection(io, hyper_service)
                        .await
                        .context("connection error")?;
                    Ok(())
                });
            }
            Some(task_result) = workers.join_next() => {
                task_result.context("connection task panicked")??;
            }
        }
    }
}
