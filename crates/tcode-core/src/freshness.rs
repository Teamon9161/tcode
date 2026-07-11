use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Tracks what file content the model has already seen, so the harness
/// can (a) short-circuit redundant reads of unchanged files, (b) demand
/// a read before `write` overwrites an existing file, and (c) detect
/// external modifications. `edit` needs no gate: its exact-match against
/// current disk content is the verification.
/// Zero-guessing principle: the model never spends tokens discovering
/// what the harness already knows.
#[derive(Debug, Default)]
pub struct FreshnessTracker {
    files: HashMap<PathBuf, FileRecord>,
}

#[derive(Debug, Clone)]
pub struct FileRecord {
    hash: u64,
    /// The model saw the entire file (vs a range).
    full: bool,
    /// Ranges seen, as (start_line, end_line) inclusive, 1-based.
    ranges: Vec<(usize, usize)>,
}

/// Answer to "should this read actually return content?".
#[derive(Debug, PartialEq, Eq)]
pub enum ReadStatus {
    /// First sighting (or tracker was reset).
    New,
    /// Same content already in context — return a stub instead.
    Unchanged,
    /// File changed on disk since the model last saw it.
    ChangedOnDisk,
    /// Same file version, but a range the model hasn't seen.
    NewRange,
}

pub fn content_hash(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// Insert `(s, e)` into a sorted, non-overlapping range list, merging any
/// ranges it touches (adjacent counts: `[1,50]` + `[51,80]` → `[1,80]`).
fn insert_coalesced(ranges: &mut Vec<(usize, usize)>, (mut s, mut e): (usize, usize)) {
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len() + 1);
    let mut inserted = false;
    for &(rs, re) in ranges.iter() {
        if re + 1 < s {
            merged.push((rs, re)); // wholly before the new range
        } else if e + 1 < rs {
            if !inserted {
                merged.push((s, e)); // new range slots in here
                inserted = true;
            }
            merged.push((rs, re));
        } else {
            // Overlapping or adjacent: absorb into the growing new range.
            s = s.min(rs);
            e = e.max(re);
        }
    }
    if !inserted {
        merged.push((s, e));
    }
    *ranges = merged;
}

impl FreshnessTracker {
    pub fn check_read(&self, path: &Path, hash: u64, range: Option<(usize, usize)>) -> ReadStatus {
        let Some(rec) = self.files.get(path) else {
            return ReadStatus::New;
        };
        if rec.hash != hash {
            return ReadStatus::ChangedOnDisk;
        }
        let covered = match range {
            None => rec.full,
            Some((s, e)) => rec.full || rec.ranges.iter().any(|(rs, re)| *rs <= s && e <= *re),
        };
        if covered {
            ReadStatus::Unchanged
        } else {
            ReadStatus::NewRange
        }
    }

    pub fn record_read(&mut self, path: &Path, hash: u64, range: Option<(usize, usize)>) {
        let rec = self.files.entry(path.to_path_buf()).or_insert(FileRecord {
            hash,
            full: false,
            ranges: Vec::new(),
        });
        if rec.hash != hash {
            // New version: everything previously seen is stale.
            rec.hash = hash;
            rec.full = false;
            rec.ranges.clear();
        }
        match range {
            None => rec.full = true,
            // Coalesced so accumulated small reads combine into the union
            // they cover — a later read spanning two prior windows is then
            // correctly recognized as already seen.
            Some(r) => insert_coalesced(&mut rec.ranges, r),
        }
    }

    /// The single contiguous slice of `(s, e)` the model has not seen yet,
    /// when the remainder is already covered. Returns `None` when the request
    /// is wholly new, wholly seen, or its uncovered part is fragmented — the
    /// caller reads the full requested range in those cases. Lets an
    /// overlapping re-read (same offset, wider window) return only the delta
    /// instead of re-appending already-seen lines to the ledger.
    pub fn uncovered_gap(
        &self,
        path: &Path,
        hash: u64,
        (s, e): (usize, usize),
    ) -> Option<(usize, usize)> {
        let rec = self.files.get(path)?;
        if rec.hash != hash || rec.full {
            return None;
        }
        // ranges is sorted and coalesced; walk the gaps within [s, e].
        let mut cursor = s;
        let mut gaps: Vec<(usize, usize)> = Vec::new();
        for &(rs, re) in &rec.ranges {
            if re < cursor || rs > e {
                continue;
            }
            if rs > cursor {
                gaps.push((cursor, rs - 1));
            }
            cursor = cursor.max(re + 1);
            if cursor > e {
                break;
            }
        }
        if cursor <= e {
            gaps.push((cursor, e));
        }
        match gaps.as_slice() {
            // A single gap strictly inside the request is a real trim; a lone
            // gap equal to the whole request means nothing was covered.
            [gap] if *gap != (s, e) => Some(*gap),
            _ => None,
        }
    }

    /// After our own write/edit the produced content is known and shown
    /// to the model, so the new version counts as seen in full.
    pub fn record_write(&mut self, path: &Path, hash: u64) {
        self.files.insert(
            path.to_path_buf(),
            FileRecord {
                hash,
                full: true,
                ranges: Vec::new(),
            },
        );
    }

    /// Has the model seen the current on-disk version (required before
    /// `write` overwrites an existing file)?
    pub fn seen_current(&self, path: &Path, hash: u64) -> bool {
        self.files.get(path).is_some_and(|r| r.hash == hash)
    }

    /// Context no longer contains old reads (compaction/rewind).
    pub fn clear(&mut self) {
        self.files.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupes_unchanged_full_read() {
        let mut t = FreshnessTracker::default();
        let p = Path::new("a.rs");
        assert_eq!(t.check_read(p, 1, None), ReadStatus::New);
        t.record_read(p, 1, None);
        assert_eq!(t.check_read(p, 1, None), ReadStatus::Unchanged);
        assert_eq!(t.check_read(p, 1, Some((5, 10))), ReadStatus::Unchanged);
        assert_eq!(t.check_read(p, 2, None), ReadStatus::ChangedOnDisk);
    }

    #[test]
    fn range_reads_cover_subranges_only() {
        let mut t = FreshnessTracker::default();
        let p = Path::new("a.rs");
        t.record_read(p, 1, Some((10, 50)));
        assert_eq!(t.check_read(p, 1, Some((20, 30))), ReadStatus::Unchanged);
        assert_eq!(t.check_read(p, 1, Some((40, 60))), ReadStatus::NewRange);
        assert_eq!(t.check_read(p, 1, None), ReadStatus::NewRange);
    }

    #[test]
    fn new_version_resets_ranges() {
        let mut t = FreshnessTracker::default();
        let p = Path::new("a.rs");
        t.record_read(p, 1, Some((1, 100)));
        t.record_read(p, 2, Some((1, 10)));
        assert_eq!(t.check_read(p, 2, Some((50, 60))), ReadStatus::NewRange);
    }

    #[test]
    fn coalesced_ranges_recognize_the_union_as_seen() {
        let mut t = FreshnessTracker::default();
        let p = Path::new("a.rs");
        t.record_read(p, 1, Some((1, 50)));
        t.record_read(p, 1, Some((51, 80))); // adjacent → merges into (1,80)
                                             // A read spanning both prior windows is now fully covered.
        assert_eq!(t.check_read(p, 1, Some((20, 70))), ReadStatus::Unchanged);
    }

    #[test]
    fn uncovered_gap_returns_only_the_new_suffix() {
        let mut t = FreshnessTracker::default();
        let p = Path::new("a.rs");
        t.record_read(p, 1, Some((1300, 1449)));
        // Same offset, wider window: only 1450-1479 is new.
        assert_eq!(t.uncovered_gap(p, 1, (1300, 1479)), Some((1450, 1479)));
        // A wholly-new range has no partial gap to trim.
        assert_eq!(t.uncovered_gap(p, 1, (2000, 2100)), None);
        // A fragmented request (hole in the middle already seen) reads whole.
        t.record_read(p, 1, Some((1600, 1650)));
        assert_eq!(t.uncovered_gap(p, 1, (1500, 1700)), None);
    }

    #[test]
    fn write_marks_current_version_seen() {
        let mut t = FreshnessTracker::default();
        let p = Path::new("a.rs");
        assert!(!t.seen_current(p, 7));
        t.record_write(p, 7);
        assert!(t.seen_current(p, 7));
        assert_eq!(t.check_read(p, 7, None), ReadStatus::Unchanged);
    }
}
