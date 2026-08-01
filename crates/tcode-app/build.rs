use std::path::Path;

fn main() {
    rerun_if_changed_tree(Path::new("ui/dist"));
    tauri_build::build()
}

/// Tauri embeds `frontendDist` into the executable. Cargo does not infer that
/// relationship from `tauri.conf.json`, so record every built asset explicitly:
/// otherwise a fresh `ui/dist` can be paired with an executable that still
/// contains yesterday's JavaScript.
fn rerun_if_changed_tree(path: &Path) {
    println!("cargo:rerun-if-changed={}", path.display());
    let Ok(entries) = path.read_dir() else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rerun_if_changed_tree(&path);
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
