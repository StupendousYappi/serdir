// Copyright (c) 2020 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Serves a directory on `http://127.0.0.1:1337/` using `ServedDir::into_tower_layer`.

mod common;

use anyhow::{Context, Result};
use common::Config;
use http::header::{self, HeaderValue};
use hyper::server::conn;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use tokio::net::TcpListener;
use tower::{Layer, Service};

use serdir::{Body, ServedDirBuilder};

type ResponseFuture =
    Pin<Box<dyn Future<Output = Result<http::Response<Body>, std::convert::Infallible>> + Send>>;

#[derive(Clone)]
struct DynamicFallbackService;

impl Service<http::Request<hyper::body::Incoming>> for DynamicFallbackService {
    type Response = http::Response<Body>;
    type Error = std::convert::Infallible;
    type Future = ResponseFuture;

    fn poll_ready(&mut self, _cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<hyper::body::Incoming>) -> Self::Future {
        Box::pin(async move { Ok(dynamic_fallback_response(&req)) })
    }
}

fn dynamic_fallback_response(req: &http::Request<hyper::body::Incoming>) -> http::Response<Body> {
    let mut html = String::new();
    html.push_str("<!DOCTYPE html>\n<title>fallback handler</title>\n");
    html.push_str("<h1>Dynamic fallback response</h1>\n");
    html.push_str("<p>URL: <code>");
    html.push_str(&htmlescape::encode_minimal(&req.uri().to_string()));
    html.push_str("</code></p>\n");
    html.push_str("<h2>Headers</h2>\n<ul>\n");
    for (name, value) in req.headers() {
        html.push_str("<li><strong>");
        html.push_str(&htmlescape::encode_minimal(name.as_str()));
        html.push_str("</strong>: <code>");
        html.push_str(&htmlescape::encode_minimal(&String::from_utf8_lossy(
            value.as_bytes(),
        )));
        html.push_str("</code></li>\n");
    }
    html.push_str("</ul>\n");

    http::Response::builder()
        .status(http::StatusCode::OK)
        .header(header::CONTENT_TYPE, HeaderValue::from_static("text/html"))
        .body(Body::from(html))
        .unwrap()
}

#[tokio::main]
async fn main() -> Result<()> {
    run().await
}

async fn run() -> Result<()> {
    env_logger::init();
    let config = Config::from_env();
    let mut builder = ServedDirBuilder::new(config.directory.as_str())
        .context("failed to create ServedDir builder")?
        .append_index_html(true)
        .compression(config.compression_strategy())
        .strip_prefix(config.strip_prefix.unwrap_or_default());
    if let Some(path) = config.not_found_path {
        builder = builder
            .not_found_path(path)
            .context("failed to set --not-found-path")?;
    }
    let served_dir = builder.build();
    let served_dir_display = served_dir.dir().display().to_string();
    let layer = served_dir.into_tower_layer();
    let service = layer.layer(DynamicFallbackService);

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
    loop {
        let (tcp, _) = listener.accept().await.context("accept failed")?;
        let service = service.clone();
        tokio::spawn(async move {
            tcp.set_nodelay(true).context("failed to set TCP_NODELAY")?;
            let io = TokioIo::new(tcp);
            let hyper_service = TowerToHyperService::new(service);
            conn::http1::Builder::new()
                .serve_connection(io, hyper_service)
                .await
                .context("connection error")?;
            Ok::<(), anyhow::Error>(())
        });
    }
}
