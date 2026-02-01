use std::{
    collections::HashSet,
    env,
    fmt::Debug,
    fs::File,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use brotli::enc::backward_references::BrotliEncoderMode;
use brotli::enc::BrotliEncoderParams;
// use bytes::Bytes; removed
use fixed_cache::Cache;
// use http::HeaderValue; removed
use std::io::Write;
use std::sync::Arc;
use tempfile;

use crate::compression::{ContentEncoding, MatchedFile};
use crate::ServeFilesError;

type CacheKey = crate::FileInfo;

const BUF_SIZE: usize = 8192;

static DEFAULT_TEXT_TYPES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "html", "htm", "xhtml", "css", "js", "json", "xml", "svg", "txt", "csv", "tsv", "md",
    ])
});

/// Builder for `BrotliCache`.
pub struct BrotliCacheBuilder {
    cache_size: u16,
    compression_level: u8,
    tempdir: Option<PathBuf>,
    supported_extensions: Option<HashSet<&'static str>>,
}

impl BrotliCacheBuilder {
    /// Returns a builder for `BrotliCache`.
    pub fn new() -> Self {
        Self {
            cache_size: 128,
            compression_level: 3,
            tempdir: None,
            supported_extensions: None,
        }
    }

    /// Sets the maximum number of items in the cache.
    ///
    /// Must be at least 4 and a power of 2.
    ///
    /// # Panics
    ///
    /// Panics if the value is invalid.
    pub fn cache_size(mut self, size: u16) -> Self {
        assert!(size >= 4, "cache_size must be at least 4");
        assert!(size.is_power_of_two(), "cache_size must be a power of two");
        self.cache_size = size;
        self
    }

    /// Sets the Brotli compression level (0-11).
    ///
    /// # Panics
    ///
    /// Panics if the value is invalid.
    pub fn compression_level(mut self, level: u8) -> Self {
        assert!(level <= 11, "compression_level must be between 0 and 11");
        self.compression_level = level;
        self
    }

    /// Sets the directory for temporary compressed files.
    pub fn tempdir(mut self, tempdir: PathBuf) -> Self {
        self.tempdir = Some(tempdir);
        self
    }

    /// Sets the file extensions that are eligible for compression.
    ///
    /// If `None`, all file extensions will be compressed.
    pub fn supported_extensions(mut self, extensions: Option<HashSet<&'static str>>) -> Self {
        self.supported_extensions = extensions;
        self
    }

    /// Builds the `BrotliCache`.
    pub fn build(self) -> BrotliCache {
        BrotliCache::new(self)
    }
}

impl Default for BrotliCacheBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// A service that creates and reuses Brotli-compressed versions of files, returning
/// a FileEntity for the compressed file if it's supported, or a FileEntity for the
/// original file if it's not.
pub struct BrotliCache {
    tempdir: PathBuf,
    cache: Cache<CacheKey, MatchedFile>,
    params: BrotliEncoderParams,
    supported_extensions: HashSet<&'static str>,
}

impl BrotliCache {
    /// Returns a builder for `BrotliCache`.
    pub fn builder() -> BrotliCacheBuilder {
        BrotliCacheBuilder::new()
    }

    fn new(builder: BrotliCacheBuilder) -> Self {
        let tempdir = builder.tempdir.unwrap_or_else(env::temp_dir);
        let cache = Cache::new(builder.cache_size as usize, Default::default());
        let mut params = BrotliEncoderParams::default();
        params.quality = i32::from(builder.compression_level);
        let supported_extensions = builder
            .supported_extensions
            .unwrap_or_else(|| DEFAULT_TEXT_TYPES.clone());

        BrotliCache {
            tempdir,
            cache,
            params,
            supported_extensions,
        }
    }

    fn detect_brotli_mode(&self, extension: &str) -> BrotliEncoderMode {
        match self.supported_extensions.contains(extension) {
            true => BrotliEncoderMode::BROTLI_MODE_TEXT,
            false => BrotliEncoderMode::BROTLI_MODE_GENERIC,
        }
    }

    /// Returns a FileEntity for the compressed file if it's supported, or a FileEntity for the
    /// original file if it's not.
    ///
    /// If a cached FileEntity already exists for the given file, it will be reused. Otherwise,
    /// the file will be compressed and a new FileEntity will be created and cached.
    ///
    /// The underlying tempfile for the compressed file is unlinked on the
    /// filesystem and inaccessible externally. It will be deleted when the last
    /// `FileEntity` referencing it is dropped (such as when the BrotliCache is
    /// dropped), or when the application is terminated (deletion is handled by
    /// the operating system and should be performed even after hard crashes of
    /// the application).
    pub(crate) fn get(&self, path: &Path) -> Result<MatchedFile, ServeFilesError> {
        let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        // We don't bother compressing file types that are already compressed, and instead
        // just return a FileEntity for the original file.
        if !self.supported_extensions.contains(extension) {
            return Self::wrap_orig(path);
        }

        let mut params = self.params.clone();
        params.mode = self.detect_brotli_mode(extension);

        // If we have a cache entry for the file, return it.
        let file_info = crate::FileInfo::for_path(path)?;
        let entity = self.cache.get(&file_info);
        if entity.is_some() {
            return Ok(entity.unwrap());
        }

        if let Ok(len) = usize::try_from(file_info.len()) {
            params.size_hint = len;
        }

        let brotli_file = match self.compress(path, params) {
            Ok(v) => v,
            Err(e) if e.kind() == ErrorKind::StorageFull => {
                self.cache.clear_slow();
                return Self::wrap_orig(path);
            }
            Err(e) => {
                let msg = format!("Brotli compression failed for {}", path.display());
                return Err(ServeFilesError::CompressionError(msg, e));
            }
        };

        let matched = MatchedFile {
            file: Arc::new(brotli_file),
            file_info,
            content_encoding: ContentEncoding::Brotli,
        };
        self.cache.insert(file_info, matched.clone());
        Ok(matched)
    }

    /// Returns a FileEntity for the uncompressed file
    ///
    /// Used when skipping compression.
    fn wrap_orig(path: &Path) -> Result<MatchedFile, ServeFilesError> {
        let file = File::open(path)?;
        let file_info = crate::FileInfo::new(path, &file)?;
        Ok(MatchedFile {
            file: Arc::new(file),
            file_info,
            content_encoding: ContentEncoding::Identity,
        })
    }

    fn compress(&self, path: &Path, params: BrotliEncoderParams) -> Result<File, crate::IOError> {
        let brotli_file: File = tempfile::tempfile_in(&self.tempdir)?;
        let mut compressor = brotli::CompressorWriter::with_params(brotli_file, BUF_SIZE, &params);

        let mut file = File::open(path)?;
        std::io::copy(&mut file, &mut compressor)?;
        compressor.flush()?;
        let brotli_file = compressor.into_inner();

        Ok(brotli_file)
    }
}

impl Debug for BrotliCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrotliCache")
            .field("cache_size", &self.cache.capacity())
            .field("compression_level", &self.params.quality)
            .field("tempdir", &self.tempdir)
            .field("supported_extensions", &self.supported_extensions)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_brotli_cache_builder_defaults() {
        let builder = BrotliCacheBuilder::new();
        assert_eq!(builder.cache_size, 128);
        assert_eq!(builder.compression_level, 3);
        assert!(builder.tempdir.is_none());
        assert!(builder.supported_extensions.is_none());
    }

    #[test]
    fn test_brotli_cache_builder_custom() {
        let mut extensions = HashSet::new();
        extensions.insert("html");

        let temp = env::temp_dir().join("test_brotli_cache");

        let builder = BrotliCacheBuilder::new()
            .cache_size(64)
            .compression_level(5)
            .tempdir(temp.clone())
            .supported_extensions(Some(extensions.clone()));

        assert_eq!(builder.cache_size, 64);
        assert_eq!(builder.compression_level, 5);
        assert_eq!(builder.tempdir.unwrap(), temp);
        assert_eq!(builder.supported_extensions.unwrap(), extensions);
    }

    #[test]
    #[should_panic(expected = "cache_size must be at least 4")]
    fn test_brotli_cache_builder_cache_size_too_small() {
        BrotliCacheBuilder::new().cache_size(2);
    }

    #[test]
    #[should_panic(expected = "cache_size must be a power of two")]
    fn test_brotli_cache_builder_cache_size_not_power_of_two() {
        BrotliCacheBuilder::new().cache_size(100);
    }

    #[test]
    #[should_panic(expected = "compression_level must be between 0 and 11")]
    fn test_brotli_cache_builder_compression_level_too_high() {
        BrotliCacheBuilder::new().compression_level(12);
    }

    #[test]
    fn test_brotli_cache_build() {
        let cache = BrotliCacheBuilder::new()
            .cache_size(16)
            .compression_level(1)
            .build();

        assert_eq!(cache.params.quality, 1);
    }
}
