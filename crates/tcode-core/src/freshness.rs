use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Tracks what file content the model has already seen, so the harness
/// can (a) short-circuit redundant reads of unchanged files, (b) demand
/// a read before edits, and (c) detect external modifications.
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

impl FreshnessTracker {
    pub fn check_read(
        &self,
        path: &Path,
        hash: u64,
        range: Option<(usize, usize)>,
    ) -> ReadStatus {
        let Some(rec) = self.files.get(path) else {
            return ReadStatus::New;
        };
        if rec.hash != hash {
            return ReadStatus::ChangedOnDisk;
        }
        let covered = match range {
            None => rec.full,
            Some((s, e)) => {
                rec.full || rec.ranges.iter().any(|(rs, re)| *rs <= s && e <= *re)
            }
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
            Some(r) => rec.ranges.push(r),
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

    /// Has the model seen the current on-disk version (required for edits)?
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
    fn write_marks_current_version_seen() {
        let mut t = FreshnessTracker::default();
        let p = Path::new("a.rs");
        assert!(!t.seen_current(p, 7));
        t.record_write(p, 7);
        assert!(t.seen_current(p, 7));
        assert_eq!(t.check_read(p, 7, None), ReadStatus::Unchanged);
    }
}
