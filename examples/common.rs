// Copyright (c) 2016-2026 Greg Steffensen and the http-serve developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use anyhow::Result;
use argh::FromArgs;
use serdir::ServedDirBuilder;

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

    pub fn compression_strategy(&self) -> Result<serdir::compression::CompressionStrategy> {
        match self.compression {
            CompressionMode::Static => {
                Ok(serdir::compression::CompressionStrategy::static_compression())
            }
            CompressionMode::None => Ok(serdir::compression::CompressionStrategy::none()),
            CompressionMode::Cached => {
                #[cfg(feature = "runtime-compression")]
                {
                    Ok(serdir::compression::CompressionStrategy::cached_compression())
                }
                #[cfg(not(feature = "runtime-compression"))]
                {
                    anyhow::bail!(
                        "compression mode 'cached' requires the 'runtime-compression' feature"
                    );
                }
            }
        }
    }

    pub fn into_builder(self) -> Result<ServedDirBuilder> {
        let mut builder = serdir::ServedDir::builder(&self.directory)
            .map_err(|e| anyhow::anyhow!("failed to create ServedDir builder: {e}"))?
            .append_index_html(true);

        builder = builder.compression(self.compression_strategy()?);

        if let Some(path) = self.not_found_path {
            builder = builder
                .not_found_path(path)
                .map_err(|e| anyhow::anyhow!("failed to set --not-found-path: {e}"))?;
        }

        Ok(builder)
    }
}

#[allow(dead_code)]
fn main() {}
