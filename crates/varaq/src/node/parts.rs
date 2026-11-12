use super::{Fence, Node, SLOT_BYTES, Slot, cmp_parts, head_of_parts};
pub(crate) struct Room<'k> {
    pub(crate) head: &'k [u8],
    pub(crate) tail: &'k [u8],
    pub(crate) at: usize,
    pub(crate) exact: bool,
    pub(crate) compact: bool,
}
impl Node {
    pub fn insert_parts(&mut self, head: &[u8], tail: &[u8], payload: &[u8], flags: u16) -> bool {
        self.insert_parts_reporting(head, tail, payload, flags).0
    }
    pub(crate) fn room_parts<'k>(
        &self,
        head: &'k [u8],
        tail: &'k [u8],
        payload_len: usize,
    ) -> Option<Room<'k>> {
        let p = self.prefix_len as usize;
        if head.len() + tail.len() < p {
            return None;
        }
        let (head, tail) = if p <= head.len() {
            (&head[p..], tail)
        } else {
            (&head[head.len()..], &tail[p - head.len()..])
        };
        let (at, exact) = self.lower_bound_parts(head, tail);
        let entry = head.len() + tail.len() + payload_len;
        let (slot, freed) = if exact {
            let old = self.slot(at);
            (0, usize::from(old.key_len) + usize::from(old.payload_len))
        } else {
            (SLOT_BYTES, 0)
        };
        let compact = if entry + slot <= self.free_space() {
            false
        } else if entry + slot <= self.free_space_after_compaction() + freed {
            true
        } else {
            return None;
        };
        Some(Room { head, tail, at, exact, compact })
    }
    pub fn insert_parts_reporting(
        &mut self,
        head: &[u8],
        tail: &[u8],
        payload: &[u8],
        flags: u16,
    ) -> (bool, bool) {
        let Some(mut room) = self.room_parts(head, tail, payload.len()) else {
            return (false, false);
        };
        if room.exact {
            let old = self.slot(room.at);
            if Node::replaces_in_place(
                (old.flags, usize::from(old.payload_len)),
                flags,
                payload.len(),
            ) {
                let replaced = self.replace_payload(room.at, payload);
                debug_assert!(replaced, "a same-length payload was refused");
                return (true, true);
            }
        }
        if room.compact {
            let mut fresh = *self;
            if room.exact {
                fresh.remove_slot(room.at);
            }
            fresh.compact();
            if fresh.free_space() < room.head.len() + room.tail.len() + payload.len() + SLOT_BYTES {
                return (false, false);
            }
            *self = fresh;
            let (at, exact) = self.lower_bound_parts(room.head, room.tail);
            (room.at, room.exact) = (at, exact);
        }
        let value = self.store(payload);
        let stored_key = self.store_parts(room.head, room.tail);
        debug_assert_eq!(
            stored_key.offset + stored_key.len,
            value.offset,
            "the payload must follow the key"
        );
        let slot = Slot {
            offset: stored_key.offset,
            key_len: (room.head.len() + room.tail.len()) as u16,
            payload_len: payload.len() as u16,
            flags,
            head: head_of_parts(room.head, room.tail),
        };
        if room.exact {
            let old = self.slot(room.at);
            self.space_used -= old.key_len + old.payload_len;
            self.set_slot(room.at, slot);
        } else {
            let n = self.len();
            for i in (room.at..n).rev() {
                let s = self.slot(i);
                self.set_slot(i + 1, s);
            }
            self.set_slot(room.at, slot);
            self.count += 1;
        }
        (true, room.compact)
    }
    fn lower_bound_parts(&self, head: &[u8], tail: &[u8]) -> (usize, bool) {
        let want = head_of_parts(head, tail);
        let found = calc::lower_bound_by(self.len(), |mid| {
            Ok::<_, core::convert::Infallible>(match self.slot(mid).head.cmp(&want) {
                core::cmp::Ordering::Equal => cmp_parts(self.key_at(mid), head, tail),
                other => other,
            })
        });
        match found {
            Ok(answer) => answer,
            Err(never) => match never {},
        }
    }
    fn store_parts(&mut self, head: &[u8], tail: &[u8]) -> Fence {
        let len = (head.len() + tail.len()) as u16;
        self.data_offset -= len;
        let at = Self::rel(self.data_offset);
        self.data[at..at + head.len()].copy_from_slice(head);
        self.data[at + head.len()..at + len as usize].copy_from_slice(tail);
        self.space_used += len;
        Fence { offset: self.data_offset, len }
    }
}
