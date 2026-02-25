// Copyright (c) 2016-2026 Greg Steffensen and the http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Serves a directory on the unix domain socket `/tmp/serdir-uds.sock` using `ServedDir::into_hyper_service`.

mod common;

use anyhow::{Context, Result};
use common::Config;
use hyper::server::conn;
use hyper_util::rt::TokioIo;
use std::fs;
use std::path::Path;
use tokio::net::UnixListener;

const SOCKET_PATH: &str = "/tmp/serdir-uds.sock";

#[tokio::main(worker_threads = 1)]
async fn main() -> Result<()> {
    env_logger::init();
    run().await
}

async fn run() -> Result<()> {
    let config = Config::from_env();
    let served_dir = config.into_builder()?.build();
    let dir = served_dir.dir().to_path_buf();
    let service = served_dir.into_hyper_service();

    let socket_path = Path::new(SOCKET_PATH);
    if socket_path.exists() {
        fs::remove_file(socket_path).context("failed to remove existing socket file")?;
    }

    let listener =
        UnixListener::bind(socket_path).context(format!("failed to bind to {}", SOCKET_PATH))?;

    log::info!("Serving {} on unix://{}", dir.display(), SOCKET_PATH);

    loop {
        let (stream, _) = listener.accept().await.context("accept failed")?;
        let service = service.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            conn::http1::Builder::new()
                .serve_connection(io, service)
                .await
                .context("connection error")?;
            Ok::<(), anyhow::Error>(())
        });
    }
}
