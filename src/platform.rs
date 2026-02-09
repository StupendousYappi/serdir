// Copyright (c) 2016-2021 The http-serve developers
// Copyright (c) 2026 Greg Steffensen

// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE.txt or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT.txt or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::io;
use std::path::Path;

#[cfg(unix)]
pub(crate) fn open_file(path: &Path) -> io::Result<std::fs::File> {
    std::fs::File::open(path)
}

#[cfg(windows)]
pub(crate) fn open_file(path: &Path) -> io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;

    OpenOptions::new()
        .read(true)
        // Allow opening directory handles so callers can classify directories
        // via metadata in a platform-consistent way.
        .custom_flags(winapi::um::winbase::FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

pub trait FileExt {
    /// Reads at least 1, at most `chunk_size` bytes beginning at `offset`, or fails.
    ///
    /// If there are no bytes at `offset`, returns an `UnexpectedEof` error.
    ///
    /// The file cursor changes on Windows (like `std::os::windows::fs::seek_read`) but not Unix
    /// (like `std::os::unix::fs::FileExt::read_at`). The caller never uses the cursor, so this
    /// doesn't matter.
    ///
    /// The windows implementation goes directly to `winapi` to allow soundly reading into an
    /// uninitialized buffer. This can be changed and the implementations unified when
    /// [`read_buf`](https://github.com/rust-lang/rust/issues/78485) is stabilized, including buf
    /// equivalents of `read_at`/`seek_read`.
    #[allow(dead_code)]
    fn read_range(&self, chunk_size: usize, offset: u64) -> io::Result<Vec<u8>>;

    /// Reads at least 1, at most `buf.len()` bytes beginning at `offset` into the provided buffer,
    /// or fails.
    ///
    /// If there are no bytes at `offset`, returns an `UnexpectedEof` error.
    fn read_range_into(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
}

impl FileExt for std::fs::File {
    #[cfg(unix)]
    fn read_range(&self, chunk_size: usize, offset: u64) -> io::Result<Vec<u8>> {
        let mut chunk = Vec::with_capacity(chunk_size);
        // Get a mutable slice to the uninitialized spare capacity
        let spare = chunk.spare_capacity_mut();
        debug_assert!(spare.len() == chunk_size);

        // SAFETY: read_at on Unix takes a raw buffer. We cast our MaybeUninit
        // slice to a raw byte slice. This is safe because we will only
        // "initialize" the bytes that are actually read.
        let bytes_read = unsafe {
            let slice = std::slice::from_raw_parts_mut(spare.as_mut_ptr() as *mut u8, chunk_size);
            self.read_range_into(slice, offset)?
        };

        // SAFETY: We just confirmed that 'bytes_read' were initialized by the OS.
        unsafe {
            chunk.set_len(bytes_read);
        }

        Ok(chunk)
    }

    #[cfg(unix)]
    fn read_range_into(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        use std::os::unix::fs::FileExt;
        let bytes_read = self.read_at(buf, offset)?;
        if bytes_read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(bytes_read)
    }

    #[cfg(windows)]
    fn read_range(&self, chunk_size: usize, offset: u64) -> io::Result<Vec<u8>> {
        let mut chunk = Vec::with_capacity(chunk_size);
        // Get a mutable slice to the uninitialized spare capacity
        let spare = chunk.spare_capacity_mut();
        debug_assert!(spare.len() == chunk_size);

        // SAFETY: read_at on Windows takes a raw buffer. We cast our MaybeUninit
        // slice to a raw byte slice. This is safe because we will only
        // "initialize" the bytes that are actually read.
        let bytes_read = unsafe {
            let slice = std::slice::from_raw_parts_mut(spare.as_mut_ptr() as *mut u8, chunk_size);
            self.read_range_into(slice, offset)?
        };

        // SAFETY: We just confirmed that 'bytes_read' were initialized by the OS.
        unsafe {
            chunk.set_len(bytes_read);
        }

        Ok(chunk)
    }

    #[cfg(windows)]
    fn read_range_into(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        // References:
        // https://github.com/rust-lang/rust/blob/5ffebc2cb3a089c27a4c7da13d09fd2365c288aa/library/std/src/sys/windows/handle.rs#L230
        // https://docs.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-readfile
        use std::os::windows::io::AsRawHandle;
        use winapi::shared::minwindef::DWORD;
        let handle = self.as_raw_handle();
        let mut read = 0;

        unsafe {
            // SAFETY: a zero `OVERLAPPED` is valid.
            let mut overlapped: winapi::um::minwinbase::OVERLAPPED = std::mem::zeroed();
            overlapped.u.s_mut().Offset = offset as u32;
            overlapped.u.s_mut().OffsetHigh = (offset >> 32) as u32;

            // SAFETY: Caller guarantees the pointer range is valid.
            if winapi::um::fileapi::ReadFile(
                handle,
                buf.as_mut_ptr() as *mut winapi::ctypes::c_void,
                DWORD::try_from(buf.len()).unwrap_or(DWORD::MAX), // saturating conversion
                &mut read,
                &mut overlapped,
            ) == 0
            {
                match winapi::um::errhandlingapi::GetLastError() {
                    #[allow(clippy::print_stderr)]
                    winapi::shared::winerror::ERROR_IO_PENDING => {
                        // Match std's <https://github.com/rust-lang/rust/issues/81357> fix:
                        // abort the process before `overlapped` is dropped.
                        eprintln!("I/O error: operation failed to complete synchronously");
                        std::process::abort();
                    }
                    winapi::shared::winerror::ERROR_HANDLE_EOF => {
                        // std::io::Error::from_raw_os_error converts this to ErrorKind::Other.
                        // Override that.
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            format!("no bytes beyond position {}", offset),
                        ));
                    }
                    o => return Err(std::io::Error::from_raw_os_error(o as i32)),
                }
            }
        }
        let read = usize::try_from(read).expect("u32 should fit in usize");
        if read == 0 {
             return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("no bytes beyond position {}", offset),
            ));
        }
        Ok(read)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::io::Write;
    use tempfile::tempfile;

    #[test]
    fn test_read_at_middle() {
        let mut f = tempfile().unwrap();
        f.write_all(b"0123456789").unwrap();
        let chunk = f.read_range(3, 4).unwrap();
        assert_eq!(chunk, b"456");
    }

    #[test]
    fn test_read_at_beyond_eof() {
        let mut f = tempfile().unwrap();
        f.write_all(b"0123456789").unwrap();
        let chunk = f.read_range(10, 8).unwrap();
        assert_eq!(chunk, b"89");
    }

    #[test]
    fn test_read_at_entirely_beyond_eof() {
        let mut f = tempfile().unwrap();
        f.write_all(b"0123456789").unwrap();
        let err = f.read_range(3, 10).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn test_read_at_io_error() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("write_only");
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .open(path)
            .unwrap();
        // This should fail because it's not open for reading.
        let err = f.read_range(10, 0).unwrap_err();
        #[cfg(unix)]
        assert_eq!(err.raw_os_error(), Some(9)); // EBADF
        #[cfg(windows)]
        assert_eq!(err.raw_os_error(), Some(5)); // ERROR_ACCESS_DENIED
    }

    #[test]
    fn test_read_whole() {
        use rand::RngCore;
        let mut f = tempfile().unwrap();
        let mut data = vec![0u8; 20480]; // 20KB
        rand::thread_rng().fill_bytes(&mut data);
        f.write_all(&data).unwrap();

        let chunk = f.read_range(data.len(), 0).unwrap();
        assert_eq!(chunk, data);
    }
}
