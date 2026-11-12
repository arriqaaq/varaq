#[cfg(all(feature = "opfs", target_arch = "wasm32"))]
mod store {
    use varaq::WorkerId;
    use varaq::io::devices::Refusal;
    use varaq::io::opfs;
    const POOL_PAGES: usize = 4_096;
    const RING_BYTES: usize = 4 << 20;
    pub(crate) async fn run() -> Result<(), Refusal> {
        let root = opfs::root().await?;
        let db = match opfs::acquire(&root, "example", false).await.and_then(|d| d.open(POOL_PAGES))
        {
            Ok(db) => db,
            Err(Refusal::NoSuchStore) => {
                opfs::acquire(&root, "example", true).await?.create(2, POOL_PAGES, RING_BYTES)?
            }
            Err(refused) => return Err(refused),
        };
        let _bound = varaq::wal::worker::bind(0);
        db.insert(WorkerId(0), b"hello", b"opfs")?;
        Ok(())
    }
}
fn main() {
    #[cfg(all(feature = "opfs", target_arch = "wasm32"))]
    wasm_bindgen_futures::spawn_local(async {
        if let Err(refused) = store::run().await {
            web_sys_log(&refused.to_string());
        }
    });
}
#[cfg(all(feature = "opfs", target_arch = "wasm32"))]
fn web_sys_log(message: &str) {
    let console = js_sys::Reflect::get(&js_sys::global(), &"console".into()).ok();
    if let Some(error) = console.and_then(|c| js_sys::Reflect::get(&c, &"error".into()).ok())
        && let Ok(error) = wasm_bindgen::JsCast::dyn_into::<js_sys::Function>(error)
    {
        let _ = error.call1(&wasm_bindgen::JsValue::NULL, &message.into());
    }
}
