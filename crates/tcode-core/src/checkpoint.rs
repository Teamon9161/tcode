//! File checkpoints: before a mutating tool runs, the original file is
//! copied aside, keyed by the ledger length at that moment. Rewinding
//! the conversation to length L can then restore every file to its
//! state at L.

use std::fs;
use std::path::{Path, PathBuf};

use crate::store::LogEvent;

#[derive(Debug, Clone)]
struct Record {
    ledger_len: usize,
    path: PathBuf,
    /// Checkpoint file name in `dir`; None = file did not exist yet.
    saved: Option<String>,
}

/// Disabled (no-op) unless given a directory — checkpoints only make
/// sense alongside session persistence.
#[derive(Debug, Default)]
pub struct CheckpointStore {
    dir: Option<PathBuf>,
    records: Vec<Record>,
}

/// 128-bit FNV-1a. Names a pre-image by what it contains, so identical content
/// saved twice is stored once. Width chosen for the consequence of a collision:
/// two different pre-images sharing a name would restore the wrong file, so the
/// 64-bit variant used for directory keys is not good enough here.
fn content_hash(bytes: &[u8]) -> u128 {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;
    let mut h = OFFSET;
    for b in bytes {
        h ^= *b as u128;
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// Outcome of restoring one file during a rewind.
#[derive(Debug, PartialEq, Eq)]
pub enum Restore {
    Restored,
    Deleted,
    Failed(String),
}

impl CheckpointStore {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir: Some(dir),
            records: Vec::new(),
        }
    }

    /// Rebuild from replayed log events on resume.
    pub fn load(dir: PathBuf, replayed: Vec<(usize, String, Option<String>)>) -> Self {
        let records: Vec<Record> = replayed
            .into_iter()
            .map(|(ledger_len, path, saved)| Record {
                ledger_len,
                path: PathBuf::from(path),
                saved,
            })
            .collect();
        Self {
            dir: Some(dir),
            records,
        }
    }

    /// Save the current content of `path` (or note its absence) before a
    /// mutation. Returns the log event to persist, if checkpointing is on.
    ///
    /// Named by content hash, so the twentieth edit of one file adds a record,
    /// not a twentieth copy of it: a session that rewrites a large file all day
    /// costs one pre-image per *distinct* state it passed through. Records still
    /// point at names, so rewind is unaffected by the sharing.
    pub fn save(&mut self, ledger_len: usize, path: &Path) -> Option<LogEvent> {
        let dir = self.dir.as_ref()?;
        let saved = match fs::read(path) {
            Ok(content) => {
                if fs::create_dir_all(dir).is_err() {
                    return None;
                }
                let name = format!("{:032x}.orig", content_hash(&content));
                let file = dir.join(&name);
                if !file.exists() && fs::write(&file, content).is_err() {
                    return None;
                }
                Some(name)
            }
            // New file: restoring means deleting it.
            Err(_) => None,
        };
        self.records.push(Record {
            ledger_len,
            path: path.to_path_buf(),
            saved: saved.clone(),
        });
        Some(LogEvent::Checkpoint {
            ledger_len,
            path: path.to_string_lossy().into_owned(),
            saved,
        })
    }

    /// Any file changes recorded at or after ledger length `len`?
    pub fn dirty_since(&self, len: usize) -> bool {
        self.records.iter().any(|r| r.ledger_len >= len)
    }

    /// Restore every touched file to its state when the ledger had
    /// `target_len` entries; drops the records that were undone.
    pub fn restore_to(&mut self, target_len: usize) -> Vec<(PathBuf, Restore)> {
        let Some(dir) = self.dir.clone() else {
            return Vec::new();
        };
        // The EARLIEST record at/after the target holds each file's
        // content as of the target moment.
        let mut results = Vec::new();
        let mut seen: Vec<&Path> = Vec::new();
        for r in self.records.iter().filter(|r| r.ledger_len >= target_len) {
            if seen.iter().any(|p| *p == r.path) {
                continue;
            }
            seen.push(&r.path);
            let outcome = match &r.saved {
                Some(name) => match fs::copy(dir.join(name), &r.path) {
                    Ok(_) => Restore::Restored,
                    Err(e) => Restore::Failed(e.to_string()),
                },
                None => match fs::remove_file(&r.path) {
                    Ok(_) => Restore::Deleted,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Restore::Deleted,
                    Err(e) => Restore::Failed(e.to_string()),
                },
            };
            results.push((r.path.clone(), outcome));
        }
        self.records.retain(|r| r.ledger_len < target_len);
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_restore_roundtrip() {
        let base = std::env::temp_dir().join(format!("tcode-ckpt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let file = base.join("a.txt");
        fs::write(&file, "v1").unwrap();

        let mut store = CheckpointStore::new(base.join("ckpts"));
        // Turn at ledger len 2 edits the file twice, len 4 once more.
        assert!(store.save(2, &file).is_some());
        fs::write(&file, "v2").unwrap();
        assert!(store.save(2, &file).is_some());
        fs::write(&file, "v3").unwrap();
        assert!(store.save(4, &file).is_some());
        fs::write(&file, "v4").unwrap();

        // New file created at len 4.
        let new_file = base.join("new.txt");
        assert!(store.save(4, &new_file).is_some());
        fs::write(&new_file, "brand new").unwrap();

        // Three saves of two distinct pre-images ("v1", "v2", "v3") plus a
        // missing file: content addressing means copies, not calls, cost disk.
        let saved = fs::read_dir(base.join("ckpts")).unwrap().count();
        assert_eq!(saved, 3, "one file per distinct pre-image");

        assert!(store.dirty_since(4));
        // Rewind to len 3: keep the len-2 edits, undo the len-4 ones.
        let results = store.restore_to(3);
        assert_eq!(fs::read_to_string(&file).unwrap(), "v3");
        assert!(!new_file.exists());
        assert!(results
            .iter()
            .any(|(p, r)| p == &new_file && *r == Restore::Deleted));
        assert!(!store.dirty_since(3));

        // Rewind to 0: back to the original.
        store.restore_to(0);
        assert_eq!(fs::read_to_string(&file).unwrap(), "v1");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn disabled_store_is_noop() {
        let mut store = CheckpointStore::default();
        assert!(store.save(0, Path::new("C:/nonexistent/x.txt")).is_none());
        assert!(store.restore_to(0).is_empty());
    }
}
