//! Create a fresh tcode session from already-normalized external history.
//! Source-specific scanning and parsing live in adapter crates; core only owns
//! its own ledger and store semantics.

use std::path::Path;

use crate::ledger::Entry;
use crate::store::{Resumed, SessionStore, StoreError};

/// Copy normalized history into a new tcode log. Imported tool records remain
/// transcript-only, and the note makes the second-hand boundary explicit.
pub fn import_entries(
    data_dir: &Path,
    cwd: &Path,
    source_label: &str,
    mut entries: Vec<Entry>,
) -> Result<Resumed, StoreError> {
    if entries.is_empty() {
        return Err(StoreError::External("no importable text messages".into()));
    }
    entries.push(Entry::Note(format!(
        "This conversation was imported from a {source_label} transcript. Tool calls and their outputs \
         were omitted, and files may have changed since; re-read any file before relying on \
         or editing it."
    )));
    let mut store = SessionStore::create(data_dir, cwd)?;
    let mut ledger = crate::ledger::Ledger::new();
    for entry in entries {
        store.record(&crate::store::LogEvent::Append {
            entry: entry.clone(),
        });
        ledger.append(entry);
    }
    Ok(Resumed {
        store,
        ledger,
        checkpoints: Vec::new(),
        startup: None,
        environment: None,
        delivered_environment: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ContentBlock;

    #[test]
    fn imports_normalized_entries_with_a_second_hand_note() {
        let temp = tempfile::tempdir().unwrap();
        let resumed = import_entries(
            temp.path(),
            temp.path(),
            "Example",
            vec![Entry::User(vec![ContentBlock::Text {
                text: "hello".into(),
            }])],
        )
        .unwrap();
        assert_eq!(resumed.ledger.entries().len(), 2);
        assert!(
            matches!(resumed.ledger.entries()[1], Entry::Note(ref text) if text.contains("Example"))
        );
    }
}
