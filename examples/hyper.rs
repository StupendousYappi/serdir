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

#[tokio::main(worker_threads = 1)]
async fn main() -> Result<()> {
    env_logger::init();
    run().await
}

async fn run() -> Result<()> {
    let config = Config::from_env();
    let served_dir = config.into_builder()?.build();
    let listener = common::bind_listener(served_dir.dir()).await?;
    let service = served_dir.into_hyper_service();

    loop {
        let (tcp, _) = listener.accept().await.context("accept failed")?;
        let service = service.clone();
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
