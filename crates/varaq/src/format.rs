pub mod catalog;
pub mod log;
pub mod value;
use crate::node::{HEADER_BYTES, PAGE_SIZE, SLOT_BYTES, offsets, slot_layout};
use crate::page::checksum;
const _: () = assert!(
    offsets::CHECKSUM == checksum::CHECKSUM_OFFSET,
    "the node header and the checksum module disagree about where the checksum lives"
);
const _: () = assert!(checksum::COVERED_FROM == offsets::COUNT);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    pub name: &'static str,
    pub offset: usize,
    pub width: usize,
}
#[derive(Clone, Copy, Debug)]
pub struct Retired {
    pub name: &'static str,
    pub was: &'static str,
    pub removed: &'static str,
    pub why: &'static str,
}
pub const RETIRED: &[Retired] = &[
    Retired {
        name: "hints",
        was: "offset 24, width 64: sixteen u32 key heads sampled from the slot directory",
        removed: "2026-09-29",
        why: "each hint is an atomic load, and sixteen of them narrowed a search of about eight steps at a cost above what they saved. upper, right and gsn moved down by 64 bytes and the header went from 112 bytes to 48",
    },
    Retired {
        name: "is_leaf",
        was: "offset 12, width 1: 1 for a leaf, 0 for an inner node",
        removed: "2026-10-02",
        why: "renamed kind, with a third value, 2, for a value page; the byte and its offset are unchanged, and a leaf and an inner node keep their values",
    },
];
const _: () = assert!(
    crate::node::flags::DEFINED == crate::node::flags::SEPARATED | crate::node::flags::BLOB
);
#[must_use]
pub fn fields() -> Vec<Field> {
    vec![
        Field { name: "checksum", offset: offsets::CHECKSUM, width: 4 },
        Field { name: "count", offset: offsets::COUNT, width: 2 },
        Field { name: "space_used", offset: offsets::SPACE_USED, width: 2 },
        Field { name: "data_offset", offset: offsets::DATA_OFFSET, width: 2 },
        Field { name: "prefix_len", offset: offsets::PREFIX_LEN, width: 2 },
        Field { name: "kind", offset: offsets::KIND, width: 1 },
        Field { name: "padding", offset: offsets::PADDING, width: 3 },
        Field { name: "lower_fence", offset: offsets::LOWER_FENCE, width: 4 },
        Field { name: "upper_fence", offset: offsets::UPPER_FENCE, width: 4 },
        Field { name: "upper", offset: offsets::UPPER, width: 8 },
        Field { name: "right", offset: offsets::RIGHT, width: 8 },
        Field { name: "gsn", offset: offsets::GSN, width: 8 },
        Field { name: "data", offset: offsets::DATA, width: PAGE_SIZE - HEADER_BYTES },
    ]
}
#[must_use]
#[allow(clippy::missing_panics_doc)]
pub fn descriptor() -> String {
    use core::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "page_size = {PAGE_SIZE}");
    let _ = writeln!(out, "header_bytes = {HEADER_BYTES}");
    let _ = writeln!(out, "slot_bytes = {SLOT_BYTES}");
    out.push_str("checksum_algorithm = \"crc32c\"\n");
    let _ = writeln!(out, "checksum_covers_from = {}", checksum::COVERED_FROM);
    out.push_str("child_ref_bits = 64\n");
    out.push_str("child_ref_encoding = \"page-id\"\n");
    out.push_str("child_ref_absent = \"0\"\n");
    out.push_str("kind_values = \"0 inner, 1 leaf, 2 value page, 3 blob run page\"\n");
    out.push_str("multi_byte_order = \"little-endian\"\n");
    let slot = |name: &str, (at, width): (usize, usize)| format!("{name} {at} {width}");
    let _ = writeln!(
        out,
        "slot_fields = \"{}, {}, {}, {}, {}\"",
        slot("key_offset", slot_layout::OFFSET),
        slot("key_len", slot_layout::KEY_LEN),
        slot("payload_len", slot_layout::PAYLOAD_LEN),
        slot("flags", slot_layout::FLAGS),
        slot("head", slot_layout::HEAD),
    );
    let _ = writeln!(
        out,
        "slot_flags_defined = \"bit 0 ({}) separated: a leaf entry whose payload is the value page \
         id, 8 bytes little-endian, non-zero, then 0 to {} of the value's first bytes, which the \
         value page does not hold; bit 1 ({}) blob: a leaf entry whose payload is the run's first \
         page, the value's length and the blob's serial, 8 bytes each little-endian, page and \
         serial non-zero, then 0 to {} of the value's first bytes, which the run does not hold; \
         no other bit\"",
        crate::node::flags::SEPARATED,
        crate::node::separated::LOCAL_MOST,
        crate::node::flags::BLOB,
        crate::node::blob::LOCAL_MOST
    );
    out.push_str("value_page_header = \"no fences, no prefix, upper 0, right 0\"\n");
    out.push_str(
        "blob_page_header = \"no slots, no fences, no prefix, upper the serial (non-zero), right \
         the index in the run, space_used the bytes held (at most the data area), data_offset 48, \
         the bytes from offset 48\"\n",
    );
    out.push_str("fence_fields = \"offset 0 2, len 2 2\"\n");
    for f in fields() {
        let _ = writeln!(out, "field {} offset {} width {}", f.name, f.offset, f.width);
    }
    out
}
#[must_use]
pub fn identity() -> u32 {
    checksum::crc32c(descriptor().as_bytes())
}
