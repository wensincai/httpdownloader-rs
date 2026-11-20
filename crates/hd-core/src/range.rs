//! Range ledger — mirrors `RANGE_INFO` lists and `UpdateRangeList` (connection.cpp).
//!
//! The original keeps a doubly linked list of ranges per download plus a queue of
//! unassigned ranges; failed parts push their remaining interval back for reassignment.
//! Here: `parts[i] = Some((next_offset, end_exclusive))` per active part, and
//! `unassigned` holds leftover intervals.

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PartRange {
    pub next_offset: u64,
    pub end: u64, // exclusive
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RangeLedger {
    pub parts: Vec<Option<PartRange>>,
    pub unassigned: Vec<(u64, u64)>, // inclusive start, exclusive end
}

impl RangeLedger {
    /// Split [0, total) evenly into `parts` intervals. `total == 0` (unknown size)
    /// yields a single open-ended part (server decides via chunked/stream).
    pub fn split_evenly(total: u64, parts: u16) -> Self {
        let parts = parts.max(1) as u64;
        if total == 0 {
            return RangeLedger {
                parts: vec![Some(PartRange { next_offset: 0, end: u64::MAX })],
                unassigned: Vec::new(),
            };
        }
        let chunk = total / parts;
        let rem = total % parts;
        let mut parts_v = Vec::with_capacity(parts as usize);
        let mut start = 0u64;
        for i in 0..parts {
            let len = chunk + u64::from(i < rem);
            parts_v.push(Some(PartRange { next_offset: start, end: start + len }));
            start += len;
        }
        RangeLedger { parts: parts_v, unassigned: Vec::new() }
    }

    pub fn is_empty_open(&self) -> bool {
        self.parts.len() == 1
            && self.parts[0].as_ref().map(|p| p.end == u64::MAX).unwrap_or(false)
            && self.unassigned.is_empty()
    }

    pub fn remaining(&self) -> u64 {
        let done: u64 = self
            .parts
            .iter()
            .flatten()
            .map(|p| p.end.saturating_sub(p.next_offset))
            .sum();
        let un: u64 = self.unassigned.iter().map(|(a, b)| b - a).sum();
        done + un
    }

    /// Take up to `n` bytes from part `idx` at its current offset.
    /// Returns the (offset, end_of_chunk) actually granted.
    pub fn take(&mut self, idx: usize, n: u64) -> Option<(u64, u64)> {
        let p = self.parts.get_mut(idx)?.as_mut()?;
        let take = n.min(p.end - p.next_offset);
        let off = p.next_offset;
        p.next_offset += take;
        if p.next_offset >= p.end && p.end != u64::MAX {
            self.parts[idx] = None;
        }
        Some((off, off + take))
    }

    /// Give back remaining bytes of a failed/finished part for reassignment
    /// (mirrors the original pushing the leftover range back onto the queue).
    pub fn release(&mut self, idx: usize) {
        if let Some(p) = self.parts.get_mut(idx).and_then(|s| s.take()) {
            if p.end > p.next_offset && p.end != u64::MAX {
                self.unassigned.push((p.next_offset, p.end));
            }
            self.parts[idx] = None;
        }
    }

    /// Take the next unassigned interval to start a new part.
    pub fn claim_unassigned(&mut self) -> Option<(u64, u64)> {
        if self.unassigned.is_empty() {
            return None;
        }
        // take the largest first (simple heuristic; original takes list head)
        let best = self
            .unassigned
            .iter()
            .enumerate()
            .max_by_key(|(_, (a, b))| b - a)
            .map(|(i, _)| i)?;
        let r = self.unassigned.swap_remove(best);
        self.parts.push(Some(PartRange { next_offset: r.0, end: r.1 }));
        Some(r)
    }

    /// Ensure part `idx` owns work: if its slot is empty, claim the largest
    /// unassigned interval into it. Returns true if the slot has work.
    /// (Mirrors the original moving leftover ranges between contexts in
    /// UpdateRangeList.)
    pub fn claim_into(&mut self, idx: usize) -> bool {
        if self.parts.get(idx).map(|p| p.is_some()).unwrap_or(false) {
            return true;
        }
        if self.unassigned.is_empty() {
            return false;
        }
        let best = self
            .unassigned
            .iter()
            .enumerate()
            .max_by_key(|(_, (a, b))| b - a)
            .map(|(i, _)| i);
        let best = match best {
            Some(i) => i,
            None => return false,
        };
        let r = self.unassigned.swap_remove(best);
        if idx < self.parts.len() {
            self.parts[idx] = Some(PartRange { next_offset: r.0, end: r.1 });
        } else {
            self.parts.push(Some(PartRange { next_offset: r.0, end: r.1 }));
        }
        true
    }

    pub fn done(&self) -> bool {
        self.parts.iter().all(|p| p.is_none()) && self.unassigned.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_even_sizes() {
        let l = RangeLedger::split_evenly(100, 4);
        assert_eq!(l.parts[0], Some(PartRange { next_offset: 0, end: 25 }));
        assert_eq!(l.parts[3], Some(PartRange { next_offset: 75, end: 100 }));
        assert_eq!(l.remaining(), 100);
    }

    #[test]
    fn split_remainder() {
        let l = RangeLedger::split_evenly(10, 4);
        let sizes: Vec<u64> = l
            .parts
            .iter()
            .map(|p| p.as_ref().unwrap().end - p.as_ref().unwrap().next_offset)
            .collect();
        assert_eq!(sizes, vec![3, 3, 2, 2]);
    }

    #[test]
    fn take_and_finish() {
        let mut l = RangeLedger::split_evenly(50, 1);
        assert_eq!(l.take(0, 20), Some((0, 20)));
        assert_eq!(l.take(0, 100), Some((20, 50)));
        assert!(l.parts[0].is_none());
        assert!(l.done());
    }

    #[test]
    fn release_and_claim() {
        let mut l = RangeLedger::split_evenly(100, 2);
        // part 0 grabs 10 bytes then "fails"
        l.take(0, 10);
        l.release(0);
        assert_eq!(l.unassigned, vec![(10, 50)]);
        assert!(l.parts[0].is_none());
        // a new part claims the leftover
        let r = l.claim_unassigned().unwrap();
        assert_eq!(r, (10, 50));
        assert_eq!(l.parts.len(), 3);
        assert_eq!(l.remaining(), 90);
    }

    #[test]
    fn open_ended_unknown_size() {
        let mut l = RangeLedger::split_evenly(0, 8);
        assert!(l.is_empty_open());
        assert_eq!(l.take(0, 5), Some((0, 5)));
        assert!(!l.done());
    }
}
