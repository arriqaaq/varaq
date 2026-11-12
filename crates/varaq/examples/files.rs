use std::fs::{File, OpenOptions};
use std::path::Path;
use varaq::WorkerId;
use varaq::io::devices::{Devices, Refusal};
const POOL_PAGES: usize = 1_024;
const RING_BYTES: usize = 4 << 20;
fn devices(dir: &Path) -> std::io::Result<Devices<File>> {
    let file = |name: &str| {
        OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join(name))
    };
    Ok(Devices::new(file("catalog")?, file("log")?, file("data")?, file("versions")?))
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
    let dir = std::env::temp_dir().join(format!("varaq-example-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let _bound = varaq::wal::worker::bind(0);
    {
        let db = devices(&dir)?.create(1, POOL_PAGES, RING_BYTES)?;
        db.insert(WorkerId(0), b"hello", b"files")?;
    }
    let db = devices(&dir)?.open(POOL_PAGES)?;
    assert_eq!(db.get(WorkerId(0), b"hello").map_err(Refusal::from)?, Some(b"files".to_vec()));
    drop(db);
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}
