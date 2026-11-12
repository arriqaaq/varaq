#![allow(unsafe_code)]
use std::fs::File;
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn advise_no_cache(file: &File) -> Result<bool, ()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1) };
        if rc == -1 { Err(()) } else { Ok(true) }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = file;
        Ok(false)
    }
}
pub(crate) fn reserve(file: &File, len: u64) -> std::io::Result<()> {
    let size = file.metadata()?.len();
    if len <= size {
        return Ok(());
    }
    let want = i64::try_from(len - size).map_err(|_| std::io::ErrorKind::InvalidInput)?;
    let unsupported = |e: &std::io::Error| {
        e.raw_os_error()
            .is_some_and(|c| c == libc::EOPNOTSUPP || c == libc::EINVAL || c == libc::ENOTSUP)
    };
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        let mut request = libc::fstore_t {
            fst_flags: libc::F_ALLOCATEALL,
            fst_posmode: libc::F_PEOFPOSMODE,
            fst_offset: 0,
            fst_length: want,
            fst_bytesalloc: 0,
        };
        let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, &raw mut request) };
        if rc == -1 {
            let e = std::io::Error::last_os_error();
            if !unsupported(&e) {
                return Err(e);
            }
        }
        file.set_len(len)
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let at = i64::try_from(size).map_err(|_| std::io::ErrorKind::InvalidInput)?;
        let rc = unsafe { libc::fallocate64(file.as_raw_fd(), 0, at, want) };
        if rc == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if !unsupported(&e) {
            return Err(e);
        }
        file.set_len(len)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (want, unsupported);
        file.set_len(len)
    }
}
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn open_direct(path: &std::path::Path) -> Result<Option<File>, std::io::Error> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_DIRECT)
            .open(path)?;
        Ok(Some(file))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Ok(None)
    }
}
pub(crate) const fn direct_alignment() -> usize {
    512
}
