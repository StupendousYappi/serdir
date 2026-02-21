// Copyright (c) 2016-2026 Greg Steffensen and the http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Serves a directory on `http://127.0.0.1:1337/` using the native `ServedDir` API.

mod common;

use anyhow::{Context, Result};
use common::Config;
use hyper::server::conn;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serdir::ServedDir;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    run().await
}

async fn run() -> Result<()> {
    let config = Config::from_env();
    let mut builder = ServedDir::builder(config.directory.as_str())
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
    let served_dir = Arc::new(served_dir);

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 1337));
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    println!(
        "Serving {} on http://{}",
        served_dir.dir().display(),
        listener
            .local_addr()
            .context("failed to get listener address")?
    );

    loop {
        let (tcp, _) = listener.accept().await.context("accept failed")?;
        let served_dir = Arc::clone(&served_dir);
        let service = service_fn(move |req| {
            let served_dir = Arc::clone(&served_dir);
            async move { served_dir.get_response(&req).await }
        });

        tokio::spawn(async move {
            tcp.set_nodelay(true).context("failed to set TCP_NODELAY")?;
            let io = TokioIo::new(tcp);
            conn::http1::Builder::new()
                .serve_connection(io, service)
                .await
                .context("connection error")?;
            Ok::<(), anyhow::Error>(())
        });
    }
}
