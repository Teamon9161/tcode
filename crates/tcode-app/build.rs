use std::path::Path;

fn main() {
    gtk_cfg();
    rerun_if_changed_tree(Path::new("ui/dist"));
    tauri_build::build()
}

/// `cfg(gtk)`: this build draws its windows with GTK, and therefore has to
/// position child webviews itself (`src/browser/place.rs`).
///
/// A named cfg rather than the five-target `any(...)` repeated at each use. The
/// list is copied from tauri's own `gtk` dependency — the same one that decides
/// whether the `gtk` crate is available here at all, so the two must agree or
/// the code behind this flag will not compile.
fn gtk_cfg() {
    println!("cargo::rustc-check-cfg=cfg(gtk)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if matches!(
        os.as_str(),
        "linux" | "dragonfly" | "freebsd" | "netbsd" | "openbsd"
    ) {
        println!("cargo::rustc-cfg=gtk");
    }
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
