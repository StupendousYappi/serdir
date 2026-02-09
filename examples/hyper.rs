// Copyright (c) 2020 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Serves a directory on `http://127.0.0.1:1337/` using `ServedDir::into_hyper_service`.

use argh::FromArgs;
use hyper_util::rt::TokioIo;
use serve_files::ServedDir;
use std::net::{Ipv4Addr, SocketAddr};
use tokio::net::TcpListener;

impl std::str::FromStr for CompressionMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "cached" => Ok(Self::Cached),
            "static" => Ok(Self::Static),
            "none" => Ok(Self::None),
            _ => Err(format!(
                "invalid value '{value}', expected one of: cached, static, none"
            )),
        }
    }
}

#[derive(FromArgs, Debug)]
/// Serves a directory over HTTP using ServedDir::into_hyper_service.
struct Config {
    /// path to the directory to serve
    #[argh(positional)]
    directory: String,

    /// path (relative to served directory) to use as 404 body
    #[argh(option)]
    not_found_path: Option<String>,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let config: Config = argh::from_env();

    let mut builder = ServedDir::builder(&config.directory)
        .map_err(|e| format!("failed to create ServedDir builder: {e}"))?
        .append_index_html(true);

    if let Some(path) = config.not_found_path {
        builder = builder
            .not_found_path(path)
            .map_err(|e| format!("failed to set --not-found-path: {e}"))?;
    }

    let service = builder.build().into_hyper_service();
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 1337));
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| format!("failed to bind {addr}: {e}"))?;

    println!(
        "Serving {} on http://{}",
        config.directory,
        listener
            .local_addr()
            .map_err(|e| format!("failed to get listener address: {e}"))?
    );

    loop {
        let (tcp, _) = listener
            .accept()
            .await
            .map_err(|e| format!("accept failed: {e}"))?;
        let service = service.clone();
        tokio::spawn(async move {
            if let Err(e) = tcp.set_nodelay(true) {
                eprintln!("failed to set TCP_NODELAY: {e}");
                return;
            }
            let io = TokioIo::new(tcp);
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                eprintln!("connection error: {e}");
            }
        });
    }
}
