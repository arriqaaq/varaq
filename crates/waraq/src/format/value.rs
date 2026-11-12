use crate::page::checksum;
use crate::txn::tuple;
#[must_use]
#[allow(clippy::missing_panics_doc)]
pub fn descriptor() -> String {
    use core::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "header_bytes = {}", tuple::HEADER_BYTES);
    let _ = writeln!(out, "chained_header_bytes = {}", tuple::CHAINED_HEADER_BYTES);
    out.push_str("tag_visible_for_all = 0\n");
    out.push_str("tag_chained = 1\n");
    out.push_str("tag_chained_removed = 2\n");
    out.push_str("tags_defined = 3\n");
    out.push_str("unknown_tag = \"refused\"\n");
    out.push_str("chained_field writer offset 1 width 2\n");
    out.push_str("chained_field tx_ts offset 3 width 8\n");
    out.push_str("chained_field previous offset 11 width 8\n");
    out.push_str(
        "tx_ts_means = \"writer start timestamp, resolved through the commit registry\"\n",
    );
    out.push_str("chained_endianness = \"little\"\n");
    out.push_str("no_previous = 0\n");
    let _ = writeln!(out, "head_bytes = {}", tuple::HEAD_BYTES);
    out.push_str("separated_head = \"in the leaf, beside the value page id\"\n");
    out
}
#[must_use]
pub fn identity() -> u32 {
    checksum::crc32c(descriptor().as_bytes())
}
