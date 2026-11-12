use crate::PositionalFile;
use crate::devices::{Devices, Refusal};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    DomException, FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemGetDirectoryOptions,
    FileSystemGetFileOptions, FileSystemReadWriteOptions, FileSystemSyncAccessHandle,
};
pub const FILES: [&str; 4] = ["catalog", "log", "data", "versions"];
const EXACT: u64 = 1 << 53;
const EXACT_F64: f64 = 9_007_199_254_740_992.0;
struct Held {
    handle: FileSystemSyncAccessHandle,
    options: FileSystemReadWriteOptions,
}
thread_local! {
    static HANDLES : RefCell < HashMap < u64, Held >> = RefCell::new(HashMap::new());
}
static NEXT: AtomicU64 = AtomicU64::new(1);
#[derive(Debug)]
pub struct OpfsFile {
    id: u64,
}
impl OpfsFile {
    #[must_use]
    pub fn adopt(handle: FileSystemSyncAccessHandle) -> Self {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let held = Held { handle, options: FileSystemReadWriteOptions::new() };
        HANDLES.with(|handles| handles.borrow_mut().insert(id, held));
        Self { id }
    }
    fn with<R>(&self, f: impl FnOnce(&Held) -> std::io::Result<R>) -> std::io::Result<R> {
        HANDLES.with(|handles| match handles.borrow().get(&self.id) {
            Some(held) => f(held),
            None => Err(std::io::ErrorKind::NotConnected.into()),
        })
    }
}
impl Drop for OpfsFile {
    fn drop(&mut self) {
        if let Some(held) = HANDLES.with(|handles| handles.borrow_mut().remove(&self.id)) {
            held.handle.close();
        }
    }
}
fn offset(at: u64) -> std::io::Result<f64> {
    if at >= EXACT {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    #[allow(clippy::cast_precision_loss)]
    Ok(at as f64)
}
#[must_use]
pub fn whole(n: f64) -> Option<u64> {
    if n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n < EXACT_F64 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some(n as u64)
    } else {
        None
    }
}
fn count(n: f64) -> std::io::Result<usize> {
    whole(n)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| std::io::ErrorKind::InvalidData.into())
}
fn file_size(n: f64) -> std::io::Result<u64> {
    whole(n).ok_or_else(|| std::io::ErrorKind::InvalidData.into())
}
fn thrown(e: &JsValue) -> String {
    if let Some(dom) = e.dyn_ref::<DomException>() {
        return dom.name();
    }
    if e.is_instance_of::<js_sys::TypeError>() {
        return "TypeError".into();
    }
    "Error".into()
}
#[must_use]
pub fn kind_of(name: &str) -> std::io::ErrorKind {
    match name {
        "QuotaExceededError" => std::io::ErrorKind::QuotaExceeded,
        "NoModificationAllowedError" => std::io::ErrorKind::ResourceBusy,
        "InvalidStateError" => std::io::ErrorKind::NotConnected,
        "NotFoundError" => std::io::ErrorKind::NotFound,
        "TypeError" => std::io::ErrorKind::InvalidInput,
        _ => std::io::ErrorKind::Other,
    }
}
fn error(e: &JsValue) -> std::io::Error {
    let name = thrown(e);
    std::io::Error::new(kind_of(&name), name)
}
impl PositionalFile for OpfsFile {
    fn read_at(&self, buf: &mut [u8], at: u64) -> std::io::Result<usize> {
        let at = offset(at)?;
        self.with(|held| {
            held.options.set_at(at);
            held.handle
                .read_with_u8_array_and_options(buf, &held.options)
                .map_err(|e| error(&e))
                .and_then(count)
        })
    }
    fn write_at(&self, buf: &[u8], at: u64) -> std::io::Result<usize> {
        let at = offset(at)?;
        self.with(|held| {
            held.options.set_at(at);
            held.handle
                .write_with_u8_array_and_options(buf, &held.options)
                .map_err(|e| error(&e))
                .and_then(count)
        })
    }
    fn size(&self) -> std::io::Result<u64> {
        self.with(|held| held.handle.get_size().map_err(|e| error(&e)).and_then(file_size))
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        let len = offset(len)?;
        self.with(|held| held.handle.truncate_with_f64(len).map_err(|e| error(&e)))
    }
    fn sync_data(&self) -> std::io::Result<()> {
        self.with(|held| held.handle.flush().map_err(|e| error(&e)))
    }
}
async fn resolved<T: JsCast>(promise: js_sys::Promise) -> Result<T, JsValue> {
    JsFuture::from(promise).await?.dyn_into::<T>()
}
fn acquire_refusal(e: &JsValue) -> Refusal {
    match thrown(e).as_str() {
        "NoModificationAllowedError" | "InvalidStateError" => Refusal::StoreBusy,
        _ => Refusal::Engine(varaq::Error::Device { page: None, op: varaq::DeviceOp::Read }),
    }
}
pub async fn root() -> Result<FileSystemDirectoryHandle, Refusal> {
    let storage = js_sys::Reflect::get(&js_sys::global(), &"navigator".into())
        .and_then(|navigator| js_sys::Reflect::get(&navigator, &"storage".into()))
        .and_then(JsCast::dyn_into::<web_sys::StorageManager>)
        .map_err(|e| acquire_refusal(&e))?;
    resolved(storage.get_directory()).await.map_err(|e| acquire_refusal(&e))
}
pub async fn acquire(
    root: &FileSystemDirectoryHandle,
    name: &str,
    create: bool,
) -> Result<Devices<OpfsFile>, Refusal> {
    let options = FileSystemGetDirectoryOptions::new();
    options.set_create(create);
    let directory: FileSystemDirectoryHandle =
        resolved(root.get_directory_handle_with_options(name, &options)).await.map_err(|e| {
            if thrown(&e) == "NotFoundError" { Refusal::NoSuchStore } else { acquire_refusal(&e) }
        })?;
    let mut held = Vec::with_capacity(FILES.len());
    for file in FILES {
        let options = FileSystemGetFileOptions::new();
        options.set_create(true);
        let opened = async {
            let handle: FileSystemFileHandle =
                resolved(directory.get_file_handle_with_options(file, &options)).await?;
            resolved::<FileSystemSyncAccessHandle>(handle.create_sync_access_handle()).await
        };
        held.push(OpfsFile::adopt(opened.await.map_err(|e| acquire_refusal(&e))?));
    }
    let [catalog, log, data, versions]: [OpfsFile; 4] =
        held.try_into().map_err(|_| Refusal::Engine(varaq::Error::Corrupt))?;
    Ok(Devices::new(catalog, log, data, versions))
}
