use io::devices::Refusal;
use io::opfs;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use waraq::WorkerId;
use waraq::tree::scan::Step;
use waraq::txn::db::{Db, Transaction};
use waraq::wal::worker::bind;
use wasm_bindgen::prelude::*;
fn refused(name: &str, message: &str) -> JsValue {
    let error = js_sys::Error::new(message);
    error.set_name(name);
    error.into()
}
fn engine(e: &waraq::Error) -> JsValue {
    refused(e.class().name(), &e.to_string())
}
fn device_set(r: &Refusal) -> JsValue {
    match r {
        Refusal::StoreBusy => refused("StoreBusy", &r.to_string()),
        Refusal::NoSuchStore => refused("NoSuchStore", &r.to_string()),
        Refusal::IncompleteCreate => refused("IncompleteCreate", &r.to_string()),
        Refusal::StoreExists => refused("StoreExists", &r.to_string()),
        Refusal::RingBelowFloor { .. } | Refusal::NoWorkers => refused("Config", &r.to_string()),
        Refusal::Engine(e) => engine(e),
    }
}
fn unknown(id: f64) -> JsValue {
    refused("UnknownTransaction", &format!("no open transaction {id}"))
}
struct Open {
    worker: usize,
    txn: Transaction<'static>,
}
#[wasm_bindgen]
pub struct Store {
    db: &'static Db,
    busy: RefCell<Vec<bool>>,
    open: RefCell<HashMap<u64, Open>>,
    next: Cell<u64>,
}
impl Store {
    fn over(db: Db) -> Self {
        let workers = db.workers();
        Self {
            db: Box::leak(Box::new(db)),
            busy: RefCell::new(vec![false; workers]),
            open: RefCell::new(HashMap::new()),
            next: Cell::new(1),
        }
    }
    fn free_worker(&self) -> Result<usize, JsValue> {
        self.busy
            .borrow()
            .iter()
            .position(|b| !b)
            .ok_or_else(|| refused("WorkerBusy", "every worker holds an open transaction"))
    }
    fn with_open<R>(
        &self,
        id: f64,
        f: impl FnOnce(&mut Transaction<'static>) -> Result<R, waraq::Error>,
    ) -> Result<R, JsValue> {
        let mut open = self.open.borrow_mut();
        let entry = open.get_mut(&key(id)?).ok_or_else(|| unknown(id))?;
        let _bound = bind(worker_id(entry.worker)?);
        f(&mut entry.txn).map_err(|e| engine(&e))
    }
    fn finish(&self, id: f64) -> Result<Open, JsValue> {
        let finished = self.open.borrow_mut().remove(&key(id)?).ok_or_else(|| unknown(id))?;
        self.busy.borrow_mut()[finished.worker] = false;
        Ok(finished)
    }
    fn autocommit<R>(
        &self,
        f: impl FnOnce(&mut Transaction<'static>) -> Result<R, waraq::Error>,
    ) -> Result<R, JsValue> {
        let worker = self.free_worker()?;
        let _bound = bind(worker_id(worker)?);
        let mut txn = self.db.begin(WorkerId(worker)).map_err(|e| engine(&e))?;
        let answer = f(&mut txn).map_err(|e| engine(&e))?;
        txn.commit().map_err(|e| engine(&e))?;
        Ok(answer)
    }
}
fn key(id: f64) -> Result<u64, JsValue> {
    opfs::whole(id).filter(|&id| id >= 1).ok_or_else(|| unknown(id))
}
fn worker_id(worker: usize) -> Result<u16, JsValue> {
    u16::try_from(worker).map_err(|_| refused("Config", "more workers than a log binding names"))
}
fn rows(
    scan: impl FnOnce(&mut dyn FnMut(&[u8], &[u8]) -> Step) -> Result<(), waraq::Error>,
) -> Result<js_sys::Array, waraq::Error> {
    let out = js_sys::Array::new();
    scan(&mut |k, v| {
        let pair = js_sys::Array::of2(&js_sys::Uint8Array::from(k), &js_sys::Uint8Array::from(v));
        out.push(&pair);
        Step::Continue
    })?;
    Ok(out)
}
#[wasm_bindgen]
impl Store {
    pub async fn create(
        name: String,
        workers: u32,
        pool_pages: u32,
        ring_bytes: u32,
    ) -> Result<Store, JsValue> {
        let root = opfs::root().await.map_err(|r| device_set(&r))?;
        let devices = opfs::acquire(&root, &name, true).await.map_err(|r| device_set(&r))?;
        let db = devices
            .create(workers as usize, pool_pages as usize, ring_bytes as usize)
            .map_err(|r| device_set(&r))?;
        Ok(Self::over(db))
    }
    pub async fn open(name: String, pool_pages: u32) -> Result<Store, JsValue> {
        let root = opfs::root().await.map_err(|r| device_set(&r))?;
        let devices = opfs::acquire(&root, &name, false).await.map_err(|r| device_set(&r))?;
        let db = devices.open(pool_pages as usize).map_err(|r| device_set(&r))?;
        Ok(Self::over(db))
    }
    pub async fn destroy(name: String) -> Result<(), JsValue> {
        let root = opfs::root().await.map_err(|r| device_set(&r))?;
        match opfs::acquire(&root, &name, false).await {
            Ok(devices) => devices.destroy().map_err(|r| device_set(&r)),
            Err(Refusal::NoSuchStore) => Ok(()),
            Err(r) => Err(device_set(&r)),
        }
    }
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, JsValue> {
        self.autocommit(|t| t.get(key))
    }
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<(), JsValue> {
        self.autocommit(|t| t.insert(key, value))
    }
    pub fn delete(&self, key: &[u8]) -> Result<bool, JsValue> {
        self.autocommit(|t| t.remove(key))
    }
    pub fn scan(&self, from: &[u8], limit: u32) -> Result<js_sys::Array, JsValue> {
        self.autocommit(|t| rows(|visit| t.scan_limit(from, limit as usize, visit)))
    }
    pub fn begin(&self) -> Result<f64, JsValue> {
        let worker = self.free_worker()?;
        let _bound = bind(worker_id(worker)?);
        let txn = self.db.begin(WorkerId(worker)).map_err(|e| engine(&e))?;
        let id = self.next.get();
        self.next.set(id + 1);
        self.busy.borrow_mut()[worker] = true;
        self.open.borrow_mut().insert(id, Open { worker, txn });
        #[allow(clippy::cast_precision_loss)]
        Ok(id as f64)
    }
    pub fn tx_get(&self, id: f64, key: &[u8]) -> Result<Option<Vec<u8>>, JsValue> {
        self.with_open(id, |t| t.get(key))
    }
    pub fn tx_put(&self, id: f64, key: &[u8], value: &[u8]) -> Result<(), JsValue> {
        self.with_open(id, |t| t.insert(key, value))
    }
    pub fn tx_delete(&self, id: f64, key: &[u8]) -> Result<bool, JsValue> {
        self.with_open(id, |t| t.remove(key))
    }
    pub fn tx_scan(&self, id: f64, from: &[u8], limit: u32) -> Result<js_sys::Array, JsValue> {
        self.with_open(id, |t| rows(|visit| t.scan_limit(from, limit as usize, visit)))
    }
    pub fn commit(&self, id: f64) -> Result<(), JsValue> {
        let Open { worker, txn } = self.finish(id)?;
        let _bound = bind(worker_id(worker)?);
        txn.commit().map_err(|e| engine(&e))
    }
    pub fn abort(&self, id: f64) -> Result<(), JsValue> {
        let Open { worker, mut txn } = self.finish(id)?;
        let _bound = bind(worker_id(worker)?);
        txn.abort().map_err(|e| engine(&e))
    }
}
