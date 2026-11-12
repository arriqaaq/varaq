use crate::{FileStore, PositionalFile, store_failed};
use std::sync::Arc;
use waraq::txn::db::{Db, Stores};
use waraq::{DeviceOp, Error, PageStore};
pub type DeviceStore<F> = FileStore<Arc<F>>;
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("another engine holds this store")]
    StoreBusy,
    #[error("no store exists here")]
    NoSuchStore,
    #[error("a create of this store never finished; destroy it to create it again")]
    IncompleteCreate,
    #[error("a store already exists here")]
    StoreExists,
    #[error("a store needs at least one worker")]
    NoWorkers,
    #[error("a log ring of {ring} bytes is below the {floor} bytes one write can need")]
    RingBelowFloor { ring: usize, floor: u64 },
    #[error(transparent)]
    Engine(#[from] Error),
}
#[derive(Debug)]
pub struct Devices<F> {
    catalog: F,
    log: F,
    data: F,
    versions: F,
}
impl<F> Devices<F> {
    pub const fn new(catalog: F, log: F, data: F, versions: F) -> Self {
        Self { catalog, log, data, versions }
    }
    pub fn map<G>(self, mut wrap: impl FnMut(F) -> G) -> Devices<G> {
        Devices {
            catalog: wrap(self.catalog),
            log: wrap(self.log),
            data: wrap(self.data),
            versions: wrap(self.versions),
        }
    }
}
impl<F: PositionalFile + 'static> Devices<F> {
    fn all_empty(&self) -> Result<bool, Refusal> {
        for file in [&self.catalog, &self.log, &self.data, &self.versions] {
            if file.size().map_err(|_| store_failed(DeviceOp::Read))? != 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }
    fn into_parts(self) -> Result<Parts<F>, Refusal> {
        let files = [self.catalog, self.log, self.data, self.versions].map(Arc::new);
        let [catalog, log, data, versions] = files.clone();
        Ok((
            FileStore::adopt(catalog)?,
            Others {
                data: FileStore::adopt(data)?,
                versions: FileStore::adopt(versions)?,
                log: FileStore::adopt(log)?,
            },
            files,
        ))
    }
    pub fn create(
        self,
        workers: usize,
        pool_pages: usize,
        ring_bytes: usize,
    ) -> Result<Db, Refusal> {
        self.create_with(workers, pool_pages, ring_bytes, as_is)
    }
    pub fn create_with(
        self,
        workers: usize,
        pool_pages: usize,
        ring_bytes: usize,
        wrap: impl FnMut(&'static str, DeviceStore<F>) -> Box<dyn PageStore>,
    ) -> Result<Db, Refusal> {
        if workers == 0 {
            return Err(Refusal::NoWorkers);
        }
        if !self.all_empty()? {
            return Err(Refusal::StoreExists);
        }
        let floor = waraq::wal::worker::ring_floor();
        if (ring_bytes as u64) < floor {
            return Err(Refusal::RingBelowFloor { ring: ring_bytes, floor });
        }
        let (catalog, others, files) = self.into_parts()?;
        let built = waraq::catalog::prepare(&catalog).and_then(|()| {
            Db::with_log_capacity(workers, pool_pages, ring_bytes, others.stores(catalog, wrap))
        });
        match built {
            Ok(db) => Ok(db),
            Err(refused) => {
                for file in &files {
                    let _ = file.set_len(0);
                    let _ = file.sync_data();
                }
                Err(Refusal::Engine(refused))
            }
        }
    }
    pub fn open(self, pool_pages: usize) -> Result<Db, Refusal> {
        self.open_with(pool_pages, as_is)
    }
    pub fn open_with(
        self,
        pool_pages: usize,
        wrap: impl FnMut(&'static str, DeviceStore<F>) -> Box<dyn PageStore>,
    ) -> Result<Db, Refusal> {
        if self.all_empty()? {
            return Err(Refusal::NoSuchStore);
        }
        let (catalog, others, _) = self.into_parts()?;
        if (catalog.len() as u64) < waraq::catalog::SECOND_SLOT
            || waraq::catalog::is_blank(&catalog)?
        {
            return Err(Refusal::IncompleteCreate);
        }
        let workers = waraq::catalog::load(&catalog)?.workers.len();
        Ok(Db::open(workers, pool_pages, others.stores(catalog, wrap))?)
    }
    pub fn destroy(self) -> Result<(), Refusal> {
        for file in [&self.catalog, &self.log, &self.data, &self.versions] {
            file.set_len(0).map_err(|_| store_failed(DeviceOp::Write))?;
            file.sync_data().map_err(|_| store_failed(DeviceOp::Write))?;
        }
        Ok(())
    }
}
type Parts<F> = (DeviceStore<F>, Others<F>, [Arc<F>; 4]);
struct Others<F> {
    data: DeviceStore<F>,
    versions: DeviceStore<F>,
    log: DeviceStore<F>,
}
impl<F: PositionalFile + 'static> Others<F> {
    fn stores(
        self,
        catalog: DeviceStore<F>,
        mut wrap: impl FnMut(&'static str, DeviceStore<F>) -> Box<dyn PageStore>,
    ) -> Stores {
        Stores {
            catalog: Some(wrap("catalog", catalog)),
            log: Some(wrap("log", self.log)),
            data: Some(wrap("data", self.data)),
            versions: Some(wrap("versions", self.versions)),
            reads: None,
        }
    }
}
fn as_is<F: PositionalFile + 'static>(
    _: &'static str,
    store: DeviceStore<F>,
) -> Box<dyn PageStore> {
    Box::new(store)
}
