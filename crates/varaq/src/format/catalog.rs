use crate::catalog;
use crate::format::Field;
#[must_use]
pub fn fields() -> Vec<Field> {
    use catalog::offsets as o;
    vec![
        Field { name: "checksum", offset: o::CHECKSUM, width: 4 },
        Field { name: "catalog_identity", offset: o::CATALOG_IDENTITY, width: 4 },
        Field { name: "page_identity", offset: o::PAGE_IDENTITY, width: 4 },
        Field { name: "value_identity", offset: o::VALUE_IDENTITY, width: 4 },
        Field { name: "log_identity", offset: o::LOG_IDENTITY, width: 4 },
        Field { name: "workers", offset: o::WORKERS, width: 4 },
        Field { name: "padding", offset: o::PADDING, width: 4 },
        Field { name: "sequence", offset: o::SEQUENCE, width: 8 },
        Field { name: "ring_base", offset: o::RING_BASE, width: 8 },
        Field { name: "ring_capacity", offset: o::RING_CAPACITY, width: 8 },
        Field { name: "root", offset: o::ROOT, width: 8 },
        Field { name: "highest_page", offset: o::HIGHEST_PAGE, width: 8 },
        Field { name: "txn_clock", offset: o::TXN_CLOCK, width: 8 },
        Field { name: "gsn_clock", offset: o::GSN_CLOCK, width: 8 },
        Field { name: "data_pages", offset: o::DATA_PAGES, width: 8 },
        Field { name: "free_pages", offset: o::FREE_PAGES, width: 8 },
        Field { name: "stamp_ceiling", offset: o::STAMP_CEILING, width: 8 },
        Field { name: "blob_serial", offset: o::BLOB_SERIAL, width: 8 },
        Field { name: "blob_free_pages", offset: o::BLOB_FREE_PAGES, width: 8 },
    ]
}
#[must_use]
#[allow(clippy::missing_panics_doc)]
pub fn descriptor() -> String {
    use core::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "identity = {:#010x}", catalog::IDENTITY);
    let _ = writeln!(out, "head_bytes = {}", catalog::HEAD_BYTES);
    let _ = writeln!(out, "max_workers = {}", catalog::MAX_WORKERS);
    let _ = writeln!(out, "first_slot = {}", catalog::FIRST_SLOT);
    let _ = writeln!(out, "second_slot = {}", catalog::SECOND_SLOT);
    for f in fields() {
        let _ = writeln!(out, "field {} offset {} width {}", f.name, f.offset, f.width);
    }
    out.push_str("per_worker = \"begin:8 flushed:8 committed:8\"\n");
    out.push_str("workers_at = \"head_bytes\"\n");
    out.push_str("endianness = \"little\"\n");
    out.push_str("padding_must_be_zero = true\n");
    out.push_str("checksum_algorithm = \"crc32c\"\n");
    out.push_str("checksum_covers_from = \"catalog_identity\"\n");
    out.push_str("checksum_mixes_page_id = true\n");
    out.push_str("slot_rule = \"even-sequence-to-first-slot\"\n");
    out.push_str("newer_slot = \"highest-valid-sequence\"\n");
    out.push_str("both_slots_invalid = \"refused\"\n");
    out.push_str("foreign_identity = \"refused\"\n");
    out
}
