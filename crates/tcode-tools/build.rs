use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn relative(manifest_dir: &Path, file: &Path) -> String {
    file.strip_prefix(manifest_dir)
        .expect("builtin file is inside the crate")
        .to_string_lossy()
        .replace('\\', "/")
}

fn markdown_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        .collect();
    files.sort();
    files
}

fn skill_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("SKILL.md"))
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files
}

/// Concatenate the built-in shell output filters into one TOML document, in
/// name order so the embedded blob is reproducible. Each file defines one
/// filter and its inline tests; the test suite parses this blob and runs them.
fn filter_blob(dir: &Path) -> String {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "toml")
        })
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "{} contains no builtin filters",
        dir.display()
    );
    let mut blob = String::new();
    for file in files {
        println!("cargo:rerun-if-changed={}", file.display());
        blob.push_str(&format!(
            "# --- {} ---\n",
            file.file_name().unwrap().to_string_lossy()
        ));
        blob.push_str(&fs::read_to_string(&file).expect("read builtin filter"));
        blob.push('\n');
    }
    blob
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let agent_dir = manifest_dir.join("src/agent/builtin");
    let skill_dir = manifest_dir.join("src/skills/builtin");
    let filter_dir = manifest_dir.join("src/shell_filter/builtin");
    println!("cargo:rerun-if-changed={}", agent_dir.display());
    println!("cargo:rerun-if-changed={}", skill_dir.display());
    println!("cargo:rerun-if-changed={}", filter_dir.display());

    let agents = markdown_files(&agent_dir);
    assert!(
        !agents.is_empty(),
        "{} contains no builtin agent definitions",
        agent_dir.display()
    );
    let skills = skill_files(&skill_dir);
    assert!(
        !skills.is_empty(),
        "{} contains no builtin skills",
        skill_dir.display()
    );

    let mut agent_manifest =
        String::from("pub(crate) const BUILTIN_AGENT_FILES: &[(&str, &str)] = &[\n");
    for file in agents {
        println!("cargo:rerun-if-changed={}", file.display());
        let path = relative(&manifest_dir, &file);
        agent_manifest.push_str(&format!("    ({path:?}, include_str!({file:?})),\n"));
    }
    agent_manifest.push_str("];\n");

    let mut skill_manifest =
        String::from("pub(crate) const BUILTIN_SKILL_FILES: &[(&str, &str, &str)] = &[\n");
    for file in skills {
        println!("cargo:rerun-if-changed={}", file.display());
        let path = relative(&manifest_dir, &file);
        let fallback_name = file
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .expect("builtin skill directory has a UTF-8 name");
        skill_manifest.push_str(&format!(
            "    ({path:?}, {fallback_name:?}, include_str!({file:?})),\n"
        ));
    }
    skill_manifest.push_str("];\n");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("output directory"));
    fs::write(out_dir.join("builtin_agents.rs"), agent_manifest)
        .expect("write builtin agent manifest");
    fs::write(out_dir.join("builtin_skills.rs"), skill_manifest)
        .expect("write builtin skill manifest");
    fs::write(
        out_dir.join("builtin_filters.toml"),
        filter_blob(&filter_dir),
    )
    .expect("write builtin filter blob");
}
