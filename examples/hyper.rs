// Copyright (c) 2020 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Serves a directory on `http://127.0.0.1:1337/` using `ServedDir::into_hyper_service`.

mod common;

use anyhow::{Context, Result};
use common::Config;
use hyper::server::conn;
use hyper_util::rt::TokioIo;
use std::net::{Ipv4Addr, SocketAddr};
use tokio::net::TcpListener;
use tokio::task::JoinSet;

#[tokio::main]
async fn main() -> Result<()> {
    run().await
}

async fn run() -> Result<()> {
    let config = Config::from_env();
    let served_dir = config.into_builder()?.build();
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
    let service = served_dir.into_hyper_service();
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
                    conn::http1::Builder::new()
                        .serve_connection(io, service)
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
