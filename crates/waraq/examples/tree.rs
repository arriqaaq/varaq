use waraq::tree::Tree;
use waraq::tree::scan::Step;
fn main() -> Result<(), waraq::Error> {
    let tree = Tree::with_capacity(1_024)?;
    for i in 0u32..10_000 {
        tree.insert(&i.to_be_bytes(), &(i * 2).to_le_bytes())?;
    }
    assert_eq!(tree.get(&7u32.to_be_bytes())?, Some(14u32.to_le_bytes().to_vec()));
    assert!(tree.remove(&7u32.to_be_bytes())?);
    assert_eq!(tree.get(&7u32.to_be_bytes())?, None);
    let mut seen = 0u32;
    let mut last: Option<Vec<u8>> = None;
    tree.scan(&[], |key, _| {
        assert!(last.as_deref() < Some(key));
        last = Some(key.to_vec());
        seen += 1;
        Step::Continue
    })?;
    assert_eq!(seen, 9_999);
    Ok(())
}
