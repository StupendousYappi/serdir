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
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    run().await
}

async fn run() -> Result<()> {
    let config = Config::from_env();
    let served_dir = config.into_builder()?.build();
    let served_dir = Arc::new(served_dir);
    let listener = common::bind_listener(served_dir.dir()).await?;

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
