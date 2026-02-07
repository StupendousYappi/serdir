use std::{
    collections::HashSet,
    env,
    fmt::Debug,
    fs::File,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
    sync::LazyLock,
};

use brotli::enc::backward_references::BrotliEncoderMode;
use brotli::enc::BrotliEncoderParams;
// use bytes::Bytes; removed
use fixed_cache::Cache;
// use log::{debug, error, info, warn}; removed
// use http::HeaderValue; removed
use std::io::Write;
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
    supported_extensions: Option<HashSet<&'static str>>,
    max_file_size: u64,
}

impl BrotliCacheBuilder {
    /// Returns a builder for `BrotliCache`.
    pub fn new() -> Self {
        Self {
            cache_size: 128,
            compression_level: 3,
            supported_extensions: None,
            max_file_size: u64::MAX,
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

    /// Sets the file extensions that are eligible for compression.
    ///
    /// If `None`, all file extensions will be compressed.
    pub fn supported_extensions(mut self, extensions: Option<HashSet<&'static str>>) -> Self {
        self.supported_extensions = extensions;
        self
    }

    /// Sets the maximum file size for compression.
    ///
    /// Files larger than this value will skip compression and be served
    /// in their original form.
    pub fn max_file_size(mut self, size: u64) -> Self {
        self.max_file_size = size;
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
    max_file_size: u64,
}

impl BrotliCache {
    /// Returns a builder for `BrotliCache`.
    pub fn builder() -> BrotliCacheBuilder {
        BrotliCacheBuilder::new()
    }

    fn new(builder: BrotliCacheBuilder) -> Self {
        let tempdir = env::temp_dir();
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
            max_file_size: builder.max_file_size,
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
        let extension: &str = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        // We don't bother compressing file types that are already compressed, and instead
        // just return a FileEntity for the original file.
        if !self.supported_extensions.contains(extension) {
            return Self::wrap_orig(path, extension);
        }

        // If we have a cache entry for the file, return it.
        let file_info = crate::FileInfo::for_path(path)?;

        // Skip compression if the file is too large.
        if file_info.len() > self.max_file_size {
            return Self::wrap_orig(path, extension);
        }

        let matched: Option<MatchedFile> = self.cache.get(&file_info);
        if let Some(f) = matched {
            return Ok(f);
        }

        let mut params = self.params.clone();
        params.mode = self.detect_brotli_mode(extension);

        if let Ok(len) = usize::try_from(file_info.len()) {
            params.size_hint = len;
        }

        let brotli_file = match self.compress(path, params) {
            Ok(v) => v,
            Err(e) if e.kind() == ErrorKind::StorageFull => {
                self.cache.clear_slow();
                return Self::wrap_orig(path, extension);
            }
            Err(e) => {
                let msg = format!("Brotli compression failed for {}", path.display());
                return Err(ServeFilesError::CompressionError(msg, e));
            }
        };

        let brotli_metadata = brotli_file.metadata()?;

        // The dynamic brotli tempfile literally doesn't have a filename, so we can't
        // calculate a path hash for it. Instead, we reverse the bytes of the original
        // file's path hash to create a pseudo path hash that is deterministic in the same
        // ways as the original path hash, and just as unlikely to collide with any other
        // value.
        let pseudo_hash = file_info.get_hash().swap_bytes();

        let brotli_file_info = crate::FileInfo {
            path_hash: pseudo_hash,
            len: brotli_metadata.len(),
            // arguably it would be more accurate and produce a higher browser
            // cache hit rate to use the same modification time as the original
            // file, but given that we use strong ETag values (i.e. different
            // for the original and compressed versions), I think it's more
            // consistent for the modification times to potentially differ too.
            mtime: brotli_metadata.modified()?,
        };

        let matched = MatchedFile {
            file: Arc::new(brotli_file),
            file_info: brotli_file_info,
            content_encoding: ContentEncoding::Brotli,
            extension: extension.to_string(),
        };
        self.cache.insert(file_info, matched.clone());
        Ok(matched)
    }

    /// Returns a FileEntity for the uncompressed file
    ///
    /// Used when skipping compression.
    fn wrap_orig(path: &Path, extension: &str) -> Result<MatchedFile, ServeFilesError> {
        let file = File::open(path)?;
        let file_info = crate::FileInfo::open_file(path, &file)?;
        let extension = extension.to_string();
        Ok(MatchedFile {
            file: Arc::new(file),
            file_info,
            content_encoding: ContentEncoding::Identity,
            extension,
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
    use std::{collections::HashSet, io::Read};

    static CAT_PHOTO_BYTES: &[u8] = include_bytes!("test-resources/cat.jpg");
    static BOOK_BYTES: &[u8] = include_bytes!("test-resources/wonderland.txt");

    /// Reads the contents of a file, optionally decompressing it if it's a brotli file.
    fn read_bytes(f: &File, decompress: bool) -> Vec<u8> {
        use std::io::{Seek, SeekFrom};
        let mut f = f;
        f.seek(SeekFrom::Start(0))
            .expect("Failed to seek to beginning");
        let mut raw_bytes = Vec::new();
        if decompress {
            let mut input = brotli::Decompressor::new(f, 4096);
            let _ = input
                .read_to_end(&mut raw_bytes)
                .expect("Failed to decompress file");
        } else {
            let mut f = f;
            f.read_to_end(&mut raw_bytes).expect("Failed to read file");
        }
        raw_bytes
    }

    #[test]
    fn test_brotli_cache_builder_defaults() {
        let builder = BrotliCacheBuilder::new();
        assert_eq!(builder.cache_size, 128);
        assert_eq!(builder.compression_level, 3);
        assert!(builder.supported_extensions.is_none());
    }

    #[test]
    fn test_brotli_cache_builder_custom() {
        let mut extensions = HashSet::new();
        extensions.insert("html");

        let builder = BrotliCacheBuilder::new()
            .cache_size(64)
            .compression_level(5)
            .supported_extensions(Some(extensions.clone()));

        assert_eq!(builder.cache_size, 64);
        assert_eq!(builder.compression_level, 5);
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

    #[test]
    fn test_simple_compression() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.html");
        let content = "<html><body><h1>Hello World</h1></body></html>";
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(content.as_bytes()).unwrap();
        }

        let cache = BrotliCache::builder().compression_level(1).build();

        let matched = cache.get(&path).expect("Failed to get file from cache");

        assert!(matches!(matched.content_encoding, ContentEncoding::Brotli));

        // Verify FileInfo
        let orig_info = crate::FileInfo::for_path(&path).unwrap();
        assert_ne!(matched.file_info.get_hash(), orig_info.get_hash());
        let delta_nanos = matched
            .file_info
            .mtime()
            .duration_since(orig_info.mtime())
            .unwrap()
            .as_nanos();
        // between 0 and 10ms
        assert!(delta_nanos < 10_000_000);

        // Verify decompressed content
        let decompressed = read_bytes(&matched.file, true);
        assert_eq!(decompressed, content.as_bytes());

        // Verify second call returns cached version
        let matched2 = cache
            .get(&path)
            .expect("Failed to get file from cache second time");
        assert_eq!(matched2.file_info.mtime(), matched.file_info.mtime());
    }

    #[test]
    fn test_compression_levels() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wonderland.txt");
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(BOOK_BYTES).unwrap();
        }

        let cache0 = BrotliCache::builder().compression_level(0).build();
        let cache5 = BrotliCache::builder().compression_level(5).build();

        let matched0 = cache0.get(&path).expect("Failed to get file from cache0");
        let matched5 = cache5.get(&path).expect("Failed to get file from cache5");

        assert!(matches!(matched0.content_encoding, ContentEncoding::Brotli));
        assert!(matches!(matched5.content_encoding, ContentEncoding::Brotli));

        // Verify decompressed content for both
        let decompressed0 = read_bytes(&matched0.file, true);
        assert_eq!(decompressed0, BOOK_BYTES);

        let decompressed5 = read_bytes(&matched5.file, true);
        assert_eq!(decompressed5, BOOK_BYTES);

        // Verify that level 5 is smaller than level 0
        let size0 = matched0.file.metadata().unwrap().len();
        let size5 = matched5.file.metadata().unwrap().len();
        // the first output should be larger than the second
        assert!(size0 > size5);
        assert_eq!(83898, size0);
        assert_eq!(59317, size5);
    }

    #[test]
    fn test_skip_compression() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jpg");
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(CAT_PHOTO_BYTES).unwrap();
        }

        // Initialize with default text types, which should NOT include jpg
        let cache = BrotliCache::builder().build();

        let matched = cache.get(&path).expect("Failed to get file from cache");

        // JPG should skip compression
        assert!(matches!(
            matched.content_encoding,
            ContentEncoding::Identity
        ));

        // Verify FileInfo
        let orig_info = crate::FileInfo::for_path(&path).unwrap();
        assert_eq!(matched.file_info.len(), orig_info.len());
        assert_eq!(matched.file_info.mtime(), orig_info.mtime());

        // Verify content
        let bytes = read_bytes(&matched.file, false);
        assert_eq!(bytes, CAT_PHOTO_BYTES);
    }

    #[test]
    fn test_max_file_size() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("too_big.txt");
        let content = "This is a test file that is larger than 10 bytes.";
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(content.as_bytes()).unwrap();
        }

        // Cache with max_file_size of 10 bytes
        let cache = BrotliCache::builder().max_file_size(10).build();

        let matched = cache.get(&path).expect("Failed to get file from cache");

        // Should skip compression because size (49) > max_file_size (10)
        assert!(matches!(
            matched.content_encoding,
            ContentEncoding::Identity
        ));

        // Now try a smaller file
        let path_small = dir.path().join("small.txt");
        let content_small = "small"; // 5 bytes
        {
            let mut f = File::create(&path_small).unwrap();
            f.write_all(content_small.as_bytes()).unwrap();
        }

        let matched_small = cache
            .get(&path_small)
            .expect("Failed to get small file from cache");

        // Should NOT skip compression
        assert!(matches!(
            matched_small.content_encoding,
            ContentEncoding::Brotli
        ));
    }
}
