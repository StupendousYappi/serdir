// Copyright (c) 2016-2026 Greg Steffensen and the http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use anyhow::Result;
use argh::FromArgs;

#[derive(Clone, Copy, Debug)]
pub enum CompressionMode {
    Cached,
    Static,
    None,
}

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
/// Shared configuration for serdir examples.
pub struct Config {
    /// path to the directory to serve
    #[argh(positional)]
    pub directory: String,

    /// URL path prefix to strip before serving files
    #[argh(option)]
    pub strip_prefix: Option<String>,

    /// compression strategy: cached, static, or none
    #[argh(option, default = "CompressionMode::None")]
    pub compression: CompressionMode,

    /// path (relative to served directory) to use as 404 body
    #[argh(option)]
    pub not_found_path: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        argh::from_env()
    }

    pub fn compression_strategy(&self) -> serdir::compression::CompressionStrategy {
        match self.compression {
            CompressionMode::Static => {
                serdir::compression::CompressionStrategy::static_compression()
            }
            CompressionMode::None => serdir::compression::CompressionStrategy::none(),
            CompressionMode::Cached => {
                #[cfg(feature = "runtime-compression")]
                {
                    serdir::compression::CompressionStrategy::cached_compression()
                }
                #[cfg(not(feature = "runtime-compression"))]
                {
                    panic!("compression mode 'cached' requires the 'runtime-compression' feature");
                }
            }
        }
    }
}

#[allow(dead_code)]
fn main() {}
