//! The repository's own instructions are layered: the root file carries only
//! cross-crate rules, and each crate's `AGENTS.md` reaches the model lazily
//! when a tool targets that crate. This pins both halves — a regression in
//! either one silently costs prefix tokens (rules hoisted back to the root) or
//! silently drops the rules (layer never discovered).

use std::path::PathBuf;

use tcode_core::MemoryManager;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crates/tcode-core has two ancestors")
        .to_path_buf()
}

#[test]
fn crate_rules_stay_out_of_the_startup_prefix_until_their_crate_is_touched() {
    let repo = repo_root();
    if !repo.join("crates/tcode-tui/AGENTS.md").is_file() {
        return; // Layering not set up in this checkout; nothing to pin.
    }
    let mut memory = MemoryManager::new(&repo);

    let startup = memory.startup_prompt();
    assert!(
        !startup.contains("tcode-tui 硬规则"),
        "crate-scoped rules must not ride the startup prefix: that costs tokens \
         on every request of every session, including ones that never open the crate"
    );

    let update = memory
        .discover_for_paths(&[repo.join("crates/tcode-tui/src/transcript.rs")])
        .expect("touching a file inside the crate must discover its AGENTS.md");
    assert!(
        update.note.contains("tcode-tui 硬规则"),
        "the lazy note must carry the crate's rules, not merely name the file"
    );
}
