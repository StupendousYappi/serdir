// Copyright (c) 2016-2018 The http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Test program which serves a local file on `http://127.0.0.1:1337/`.
//!
//! Performs file IO on a separate thread pool from the reactor so that it doesn't block on
//! local disk. Supports HEAD, conditional GET, and byte range requests. Some commands to try:
//!
//! ```text
//! $ curl --head http://127.0.0.1:1337/FILENAME
//! $ curl http://127.0.0.1:1337/FILENAME > /dev/null
//! $ curl -v -H 'Range: bytes=1-10' http://127.0.0.1:1337/FILENAME
//! $ curl -v -H 'Range: bytes=1-10,30-40' http://127.0.0.1:1337/FILENAME
//! ```

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use serve_files::ServedDir;
use tokio::net::TcpListener;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let mut args = std::env::args();
    if args.len() != 2 {
        eprintln!("Expected serve [FILENAME]");
        std::process::exit(1);
    }
    let path = PathBuf::from(args.nth(1).unwrap());
    let filename = path
        .file_name()
        .ok_or_else(|| "expected a filename path, but received a directory root")?
        .to_string_lossy()
        .into_owned();
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let service = ServedDir::builder(parent)?.build().into_hyper_service();

    let addr: SocketAddr = (Ipv4Addr::LOCALHOST, 1337).into();
    let listener = TcpListener::bind(addr).await?;
    println!("Serving {} on http://{}/{}", path.display(), addr, filename);
    loop {
        let (tcp, _) = listener.accept().await?;
        let service = service.clone();
        let io = TokioIo::new(tcp);
        tokio::task::spawn(async move {
            if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                eprintln!("Error serving connection: {}", e);
            }
        });
    }
}
