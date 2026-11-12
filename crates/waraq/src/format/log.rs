use crate::format::Field;
use crate::page::checksum;
use crate::wal::record;
use crate::wal::record::offsets;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetiredKind {
    pub kind: u8,
    pub was: &'static str,
    pub removed: &'static str,
    pub why: &'static str,
}
pub const RETIRED_KINDS: &[RetiredKind] = &[
    RetiredKind {
        kind: 4,
        was: "page record whose payload was the page's whole 4096-byte image",
        removed: "2026-10-04",
        why: "a page image leaves out the page's free gap; the kind took the number 9",
    },
    RetiredKind {
        kind: 6,
        was: "structure modification of fixed 4112-byte entries, each a whole page image",
        removed: "2026-10-04",
        why: "each entry's image leaves out its page's free gap; the kind took the number 10",
    },
    RetiredKind {
        kind: 8,
        was: "value write of fixed 4112-byte entries, each a whole page image",
        removed: "2026-10-04",
        why: "each entry's image leaves out its page's free gap; the kind took the number 11",
    },
    RetiredKind {
        kind: 11,
        was: "value write whose every entry was its page's image, its free gap left out",
        removed: "2026-10-04",
        why: "each entry is its page's image or its change, behind a form byte; the kind took the number 13",
    },
    RetiredKind {
        kind: 12,
        was: "page change: the bytes of the changed page that differed from the page before, as \
              fragments behind the changed page's gap",
        removed: "2026-10-05",
        why: "a page record that is not an image carries the tuple operation that changed the page, which redo performs; the kind took the number 14",
    },
    RetiredKind {
        kind: 13,
        was: "value write whose entries were each its page's image or its byte change",
        removed: "2026-10-05",
        why: "each entry is its page's image or the tuple operation that changed it; the kind took the number 15",
    },
    RetiredKind {
        kind: 7,
        was: "undo record: a key, its before bytes and the header of the version its change left, \
              appended before the page record of the change it undid",
        removed: "2026-10-05",
        why: "one record per key change carries the change and what takes it back; the kind took the number 16",
    },
];
#[must_use]
pub fn fields() -> Vec<Field> {
    vec![
        Field { name: "checksum", offset: offsets::CHECKSUM, width: 4 },
        Field { name: "kind", offset: offsets::KIND, width: 1 },
        Field { name: "worker", offset: offsets::WORKER, width: 2 },
        Field { name: "padding", offset: offsets::PADDING, width: 1 },
        Field { name: "len", offset: offsets::LEN, width: 4 },
        Field { name: "gsn", offset: offsets::GSN, width: 8 },
        Field { name: "page", offset: offsets::PAGE, width: 8 },
        Field { name: "txn", offset: offsets::TXN, width: 8 },
        Field { name: "prev", offset: offsets::PREV, width: 8 },
        Field { name: "lap", offset: offsets::LAP, width: 8 },
    ]
}
#[must_use]
#[allow(clippy::missing_panics_doc)]
pub fn descriptor() -> String {
    use core::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "header_bytes = {}", record::HEADER_BYTES);
    let _ = writeln!(out, "max_payload = {}", record::MAX_PAYLOAD);
    for f in fields() {
        let _ = writeln!(out, "field {} offset {} width {}", f.name, f.offset, f.width);
    }
    out.push_str("endianness = \"little\"\n");
    out.push_str("kind_start = 1\n");
    out.push_str("kind_commit = 2\n");
    out.push_str("kind_abort = 3\n");
    let _ = writeln!(out, "kind_page = {}", record::Kind::Page as u8);
    out.push_str("kind_carriage_return = 5\n");
    let _ = writeln!(out, "kind_smo = {}", record::Kind::Smo as u8);
    let _ = writeln!(out, "kind_value_write = {}", record::Kind::ValueWrite as u8);
    let _ = writeln!(out, "kind_page_op = {}", record::Kind::PageOp as u8);
    let _ = writeln!(out, "kind_change = {}", record::Kind::Change as u8);
    out.push_str("kinds_defined = 9\n");
    let retired: Vec<String> = RETIRED_KINDS.iter().map(|r| r.kind.to_string()).collect();
    let _ = writeln!(out, "kinds_retired = \"{}\"", retired.join(" "));
    out.push_str("unknown_kind = \"refused\"\n");
    out.push_str("padding_must_be_zero = true\n");
    let _ = writeln!(out, "image_prefix_bytes = {}", record::payload::IMAGE_PREFIX_BYTES);
    out.push_str("image_prefix = \"gap_start:2 gap_end:2\"\n");
    out.push_str("image_body = \"page[0..gap_start] page[gap_end..page_size]\"\n");
    out.push_str("image_gap = \"node-slots-end-to-data-offset\"\n");
    let (none_start, none_end) = record::payload::NO_GAP;
    let _ = writeln!(out, "image_no_gap = \"{none_start} {none_end}\"");
    out.push_str("image_gap_disagreeing_with_header = \"refused\"\n");
    out.push_str("image_gap_restored_as = \"zeroes\"\n");
    let _ = writeln!(out, "smo_entry_prefix_bytes = {}", record::payload::SMO_ENTRY_PREFIX_BYTES);
    out.push_str("smo_entry = \"page:8 prev:8 image\"\n");
    let _ = writeln!(out, "smo_min_pages = {}", record::payload::SMO_MIN_PAGES);
    let _ = writeln!(out, "smo_max_pages = {}", record::payload::SMO_MAX_PAGES);
    out.push_str("smo_count = \"walked-to-length\"\n");
    out.push_str(
        "payload_commit = \"remote-flag, then optionally u16-le count and per named dependency u16-le worker, u64-le tag, u64-le commit end\"\n",
    );
    out.push_str("payload_start_abort = \"empty\"\n");
    out.push_str("payload_carriage_return = \"whatever-it-skips\"\n");
    out.push_str("payload_page = \"image\"\n");
    let _ = writeln!(out, "op_prefix_bytes = {}", record::payload::OP_PREFIX_BYTES);
    out.push_str(
        "payload_page_op = \"gap_start:2 gap_end:2 op:1 flags:2 key_len:2 payload_len:2 \
         key[key_len] payload[payload_len]\"\n",
    );
    let _ = writeln!(
        out,
        "op_codes = \"insert:{} replace:{} remove:{}\"",
        record::payload::OP_INSERT,
        record::payload::OP_REPLACE,
        record::payload::OP_REMOVE
    );
    out.push_str("op_key = \"whole-key-truncated-by-the-node\"\n");
    out.push_str("op_addressed_by = \"key\"\n");
    out.push_str("op_redo = \"node-insert-reporting-or-remove-slot-compaction-repeated\"\n");
    out.push_str("op_presence_disagreeing = \"refused\"\n");
    out.push_str("op_remove_with_flags_or_payload = \"refused\"\n");
    out.push_str("op_undefined_flags = \"refused\"\n");
    let stamped: Vec<String> =
        record::payload::STAMPED.iter().map(|(a, b)| format!("{a}..{b}")).collect();
    let _ = writeln!(out, "op_never_compares = \"{}\"", stamped.join(" "));
    out.push_str("op_gap_restored_as = \"zeroes\"\n");
    out.push_str("op_gap_disagreeing_with_rebuilt_header = \"refused\"\n");
    out.push_str("op_without_predecessor = \"refused\"\n");
    let _ =
        writeln!(out, "value_entry_prefix_bytes = {}", record::payload::VALUE_ENTRY_PREFIX_BYTES);
    out.push_str("value_entry = \"page:8 prev:8 form:1 image-or-op-or-run\"\n");
    let _ = writeln!(
        out,
        "value_entry_forms = \"image:{} op:{} run:{}\"",
        record::payload::FORM_IMAGE,
        record::payload::FORM_OP,
        record::payload::FORM_RUN
    );
    out.push_str(
        "value_entry_run = \"page:first prev:page_count serial:8; zero page or count refused; \
         applied by nothing; counted as named to its last page\"\n",
    );
    let _ = writeln!(out, "undo_prefix_bytes = {}", record::payload::UNDO_PREFIX_BYTES);
    out.push_str(
        "payload_change = \"flags:1 key_len:2 before_len:2 head:19 key[key_len] \
         before[before_len] then value entries\"\n",
    );
    out.push_str("undo_flag_before = 1\n");
    out.push_str("undo_reserved_flags = \"refused\"\n");
    out.push_str("undo_empty_value = \"flag-set-length-zero\"\n");
    out.push_str("undo_length_without_flag = \"refused\"\n");
    let _ = writeln!(out, "undo_head_bytes = {}", record::payload::UNDO_HEAD_BYTES);
    out.push_str("undo_head = \"version-header: tag:1 writer:2 start:8 displaced:8\"\n");
    out.push_str("undo_restore_condition = \"stored-value-begins-with-head\"\n");
    let _ = writeln!(out, "change_min_pages = {}", record::payload::CHANGE_MIN_PAGES);
    out.push_str("change_header_page_and_prev = \"zero\"\n");
    out.push_str("change_header_txn = \"the-transaction\"\n");
    out.push_str("wrap_filler = \"carriage_return_when_at_least_one_header_remains\"\n");
    out.push_str("wrap_short_tail = \"skipped_and_zeroed\"\n");
    out.push_str("checksum_algorithm = \"crc32c\"\n");
    out.push_str("checksum_covers_from = 4\n");
    out
}
#[must_use]
pub fn identity() -> u32 {
    checksum::crc32c(descriptor().as_bytes())
}
