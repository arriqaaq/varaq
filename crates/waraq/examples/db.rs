use waraq::WorkerId;
use waraq::tree::scan::Step;
use waraq::txn::db::Db;
fn main() -> Result<(), waraq::Error> {
    let db = Db::with_capacity(2, 1_024)?;
    let me = WorkerId(0);
    let _bound = waraq::wal::worker::bind(0);
    db.insert(me, b"apple", b"1")?;
    db.insert(me, b"pear", b"2")?;
    let mut tx = db.begin(me)?;
    tx.insert(b"plum", b"3")?;
    assert!(tx.remove(b"apple")?);
    tx.commit()?;
    assert_eq!(db.get(me, b"apple")?, None);
    assert_eq!(db.get(me, b"plum")?, Some(b"3".to_vec()));
    let tx = db.begin(me)?;
    let mut keys = Vec::new();
    tx.scan(b"", |key, _| {
        keys.push(key.to_vec());
        Step::Continue
    })?;
    tx.commit()?;
    assert_eq!(keys, [b"pear".to_vec(), b"plum".to_vec()]);
    Ok(())
}
