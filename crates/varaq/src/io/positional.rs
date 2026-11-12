pub trait PositionalFile: Send + Sync {
    fn read_at(&self, buf: &mut [u8], at: u64) -> std::io::Result<usize>;
    fn write_at(&self, buf: &[u8], at: u64) -> std::io::Result<usize>;
    fn size(&self) -> std::io::Result<u64>;
    fn set_len(&self, len: u64) -> std::io::Result<()>;
    fn sync_data(&self) -> std::io::Result<()>;
    fn reserve(&self, len: u64) -> std::io::Result<()> {
        self.set_len(len)
    }
}
impl<F: PositionalFile + ?Sized> PositionalFile for std::sync::Arc<F> {
    #[inline]
    fn read_at(&self, buf: &mut [u8], at: u64) -> std::io::Result<usize> {
        (**self).read_at(buf, at)
    }
    #[inline]
    fn write_at(&self, buf: &[u8], at: u64) -> std::io::Result<usize> {
        (**self).write_at(buf, at)
    }
    fn size(&self) -> std::io::Result<u64> {
        (**self).size()
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        (**self).set_len(len)
    }
    fn sync_data(&self) -> std::io::Result<()> {
        (**self).sync_data()
    }
    fn reserve(&self, len: u64) -> std::io::Result<()> {
        (**self).reserve(len)
    }
}
#[inline]
fn whole(
    len: usize,
    at: u64,
    mut step: impl FnMut(usize, u64) -> std::io::Result<usize>,
) -> Result<(), u64> {
    let mut done = 0;
    while done < len {
        let here = at.checked_add(done as u64).ok_or(at)?;
        match step(done, here) {
            Ok(n) if n == 0 || n > len - done => return Err(here),
            Ok(n) => done += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(here),
        }
    }
    Ok(())
}
#[inline]
pub(crate) fn read_whole<F: PositionalFile + ?Sized>(
    file: &F,
    buf: &mut [u8],
    at: u64,
) -> Result<(), u64> {
    whole(buf.len(), at, |done, here| file.read_at(&mut buf[done..], here))
}
#[inline]
pub(crate) fn write_whole<F: PositionalFile + ?Sized>(
    file: &F,
    buf: &[u8],
    at: u64,
) -> Result<(), u64> {
    whole(buf.len(), at, |done, here| file.write_at(&buf[done..], here))
}
#[cfg(unix)]
impl PositionalFile for std::fs::File {
    #[inline]
    fn read_at(&self, buf: &mut [u8], at: u64) -> std::io::Result<usize> {
        std::os::unix::fs::FileExt::read_at(self, buf, at)
    }
    #[inline]
    fn write_at(&self, buf: &[u8], at: u64) -> std::io::Result<usize> {
        std::os::unix::fs::FileExt::write_at(self, buf, at)
    }
    fn size(&self) -> std::io::Result<u64> {
        Ok(self.metadata()?.len())
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        std::fs::File::set_len(self, len)
    }
    fn sync_data(&self) -> std::io::Result<()> {
        std::fs::File::sync_data(self)
    }
    fn reserve(&self, len: u64) -> std::io::Result<()> {
        crate::io::platform::reserve(self, len)
    }
}
#[cfg(unix)]
#[repr(C, align(4096))]
pub(crate) struct AlignedPage(pub(crate) [u8; crate::PAGE_SIZE]);
#[cfg(unix)]
std::thread_local! {
    static BOUNCE : std::cell::RefCell < Box < AlignedPage >> =
    std::cell::RefCell::new(Box::new(AlignedPage([0; crate::PAGE_SIZE])));
}
#[cfg(unix)]
#[derive(Debug)]
pub struct DirectFile(pub(crate) std::fs::File);
#[cfg(unix)]
impl PositionalFile for DirectFile {
    fn read_at(&self, buf: &mut [u8], at: u64) -> std::io::Result<usize> {
        let n = buf.len().min(crate::PAGE_SIZE);
        BOUNCE.with(|b| {
            let mut b = b.borrow_mut();
            let got = std::os::unix::fs::FileExt::read_at(&self.0, &mut b.0, at)?;
            if got < n {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            buf[..n].copy_from_slice(&b.0[..n]);
            Ok(n)
        })
    }
    fn write_at(&self, buf: &[u8], at: u64) -> std::io::Result<usize> {
        if buf.len() < crate::PAGE_SIZE {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        BOUNCE.with(|b| {
            let mut b = b.borrow_mut();
            b.0.copy_from_slice(&buf[..crate::PAGE_SIZE]);
            let put = std::os::unix::fs::FileExt::write_at(&self.0, &b.0, at)?;
            if put < crate::PAGE_SIZE {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            Ok(crate::PAGE_SIZE)
        })
    }
    fn size(&self) -> std::io::Result<u64> {
        self.0.size()
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.0.set_len(len)
    }
    fn sync_data(&self) -> std::io::Result<()> {
        self.0.sync_data()
    }
    fn reserve(&self, len: u64) -> std::io::Result<()> {
        PositionalFile::reserve(&self.0, len)
    }
}
