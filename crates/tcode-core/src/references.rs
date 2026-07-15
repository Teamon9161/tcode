//! Project-relative `@path` references shared by every user-input frontend.
//!
//! Completion only inserts a compact marker into the visible prompt. This module
//! resolves that marker immediately before the ledger append, so TUI, plain REPL,
//! `-p`, and queued input all receive the same bounded snapshot.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::blobs::approx_tokens;
use crate::types::ContentBlock;

const MAX_INDEX_ENTRIES: usize = 20_000;
const MAX_REFERENCES: usize = 8;
/// Candidate files may be read in full before deciding whether their text fits
/// the model budget. This cap prevents a pathological `@file` from allocating
/// unbounded memory before it can fall back to a bounded excerpt.
const MAX_COMPLETE_FILE_BYTES: u64 = 512 * 1024;
const MAX_FILE_REFERENCE_TOKENS: usize = 6_000;
const LARGE_FILE_HEAD_BYTES: usize = 4 * 1024;
const LARGE_FILE_TAIL_BYTES: usize = 2 * 1024;
const MAX_REFERENCE_TOKENS: usize = 8_000;
const TREE_MAX_ENTRIES: usize = 80;
const TREE_MAX_PER_DIR: usize = 20;
const TREE_MAX_DEPTH: usize = 2;

/// Directories which are never useful `@` candidates, even outside a repository
/// with an ignore file. Keep this aligned with search's safety-oriented pruning.
const PRUNE_DIRS: &[&str] = &[
    ".git",
    ".svn",
    ".hg",
    ".bzr",
    ".jj",
    ".sl",
    "node_modules",
    "target",
    "dist",
    "build",
    "zig-cache",
    "zig-out",
    ".zig-cache",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".nox",
    ".cargo",
    ".rustup",
    ".cache",
    ".npm",
    ".pnpm-store",
    ".yarn",
    ".gradle",
    ".m2",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".parcel-cache",
    "AppData",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReferenceKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceCandidate {
    /// Always slash-separated, relative to the session cwd.
    pub path: String,
    pub kind: ReferenceKind,
    pub bytes: Option<u64>,
}

impl ReferenceCandidate {
    pub fn display_kind(&self) -> &'static str {
        match self.kind {
            ReferenceKind::File => "file",
            ReferenceKind::Directory => "directory",
        }
    }
}

#[derive(Debug)]
pub struct ExpandedReferences {
    pub blocks: Vec<ContentBlock>,
    pub labels: Vec<String>,
    pub added_tokens: usize,
}

/// Build a bounded, ignored-aware project index. It is deliberately synchronous:
/// callers run it in `spawn_blocking`, keeping terminal input responsive.
pub fn index_project(cwd: &Path) -> Vec<ReferenceCandidate> {
    let mut candidates = Vec::new();
    let walker = project_walker(cwd).build();
    for entry in walker.flatten() {
        if candidates.len() >= MAX_INDEX_ENTRIES {
            break;
        }
        let path = entry.path();
        if path == cwd {
            continue;
        }
        let Ok(relative) = path.strip_prefix(cwd) else {
            continue;
        };
        let kind = if path.is_dir() {
            ReferenceKind::Directory
        } else if path.is_file() {
            ReferenceKind::File
        } else {
            continue;
        };
        let bytes = matches!(kind, ReferenceKind::File)
            .then(|| entry.metadata().ok().map(|meta| meta.len()))
            .flatten();
        candidates.push(ReferenceCandidate {
            path: relative_path(relative),
            kind,
            bytes,
        });
    }
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    candidates
}

/// Expand markers inside text blocks into distinct model-facing reference blocks.
/// The original prompt remains untouched, so transcript and rewind preserve what
/// the human actually typed.
pub async fn expand_references(cwd: PathBuf, blocks: Vec<ContentBlock>) -> ExpandedReferences {
    let has_references = blocks.iter().any(
        |block| matches!(block, ContentBlock::Text { text } if !parse_mentions(text).is_empty()),
    );
    if !has_references {
        return ExpandedReferences {
            blocks,
            labels: Vec::new(),
            added_tokens: 0,
        };
    }
    tokio::task::spawn_blocking(move || expand_references_blocking(&cwd, blocks))
        .await
        .expect("reference expansion task panicked")
}

fn expand_references_blocking(cwd: &Path, mut blocks: Vec<ContentBlock>) -> ExpandedReferences {
    let root = match cwd.canonicalize() {
        Ok(root) => root,
        Err(_) => {
            return ExpandedReferences {
                blocks,
                labels: Vec::new(),
                added_tokens: 0,
            }
        }
    };
    let references: Vec<String> = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(parse_mentions(text)),
            _ => None,
        })
        .flatten()
        .collect();
    let mut seen = HashSet::new();
    let mut labels = Vec::new();
    let mut added_tokens: usize = 0;

    for raw in references {
        if !seen.insert(raw.clone()) {
            continue;
        }
        if labels.len() >= MAX_REFERENCES {
            break;
        }
        let Some((absolute, relative)) = resolve_reference(&root, &raw) else {
            continue;
        };
        let content = if absolute.is_dir() {
            directory_reference(&absolute, &relative)
        } else if absolute.is_file() {
            file_reference(&absolute, &relative)
        } else {
            continue;
        };
        let tokens = approx_tokens(&content);
        if added_tokens.saturating_add(tokens) > MAX_REFERENCE_TOKENS {
            let omitted = format!(
                "Project reference `{relative}` was selected but its content was not inlined because this message's reference budget is exhausted. Use read/glob on that path if needed."
            );
            added_tokens += approx_tokens(&omitted);
            blocks.push(ContentBlock::Text { text: omitted });
        } else {
            added_tokens += tokens;
            blocks.push(ContentBlock::Text { text: content });
        }
        labels.push(relative);
    }

    ExpandedReferences {
        blocks,
        labels,
        added_tokens,
    }
}

fn project_walker(cwd: &Path) -> ignore::WalkBuilder {
    let mut walker = ignore::WalkBuilder::new(cwd);
    walker.hidden(false).filter_entry(|entry| {
        !(entry.file_type().is_some_and(|kind| kind.is_dir())
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| PRUNE_DIRS.contains(&name)))
    });
    walker
}

fn relative_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn resolve_reference(root: &Path, raw: &str) -> Option<(PathBuf, String)> {
    if raw.is_empty() || Path::new(raw).is_absolute() {
        return None;
    }
    let candidate = root.join(raw);
    let resolved = candidate.canonicalize().ok()?;
    if !resolved.starts_with(root) {
        return None;
    }
    let relative = relative_path(resolved.strip_prefix(root).ok()?);
    Some((resolved, relative))
}

fn file_reference(path: &Path, relative: &str) -> String {
    let bytes = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    if bytes <= MAX_COMPLETE_FILE_BYTES {
        match fs::read(path)
            .ok()
            .and_then(|data| String::from_utf8(data).ok())
        {
            Some(content) if approx_tokens(&content) <= MAX_FILE_REFERENCE_TOKENS => {
                return format!(
                    "Project reference file `{relative}` ({bytes} bytes, complete contents):\n```\n{content}\n```"
                );
            }
            Some(_) => {
                return excerpt_reference(
                    path,
                    relative,
                    bytes,
                    "it exceeds the 6,000-token per-file reference budget",
                );
            }
            None => {
                return format!(
                    "Project reference file `{relative}` ({bytes} bytes) is binary or not valid UTF-8; its contents were not inlined."
                );
            }
        }
    }
    excerpt_reference(
        path,
        relative,
        bytes,
        "it exceeds the 512 KiB safe full-read limit",
    )
}

fn excerpt_reference(path: &Path, relative: &str, bytes: u64, reason: &str) -> String {
    match read_large_text_excerpt(path, bytes) {
        Some(content) => format!(
            "Project reference file `{relative}` ({bytes} bytes, excerpt only because {reason}):\n```\n{content}\n```\nThe middle was omitted; use read with offset/limit for the needed range."
        ),
        None => format!(
            "Project reference file `{relative}` ({bytes} bytes) is binary or not valid UTF-8; its contents were not inlined."
        ),
    }
}

fn read_large_text_excerpt(path: &Path, len: u64) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut head = vec![0; LARGE_FILE_HEAD_BYTES.min(len as usize)];
    file.read_exact(&mut head).ok()?;
    let tail_len = LARGE_FILE_TAIL_BYTES.min(len.saturating_sub(head.len() as u64) as usize);
    let mut tail = vec![0; tail_len];
    if tail_len > 0 {
        file.seek(SeekFrom::End(-(tail_len as i64))).ok()?;
        file.read_exact(&mut tail).ok()?;
    }
    let mut text = String::from_utf8(head).ok()?;
    if tail_len > 0 {
        text.push_str("\n\n… [middle omitted] …\n\n");
        text.push_str(&String::from_utf8(tail).ok()?);
    }
    Some(text)
}

fn directory_reference(path: &Path, relative: &str) -> String {
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let walker = project_walker(path)
        .max_depth(Some(TREE_MAX_DEPTH + 1))
        .sort_by_file_name(std::cmp::Ord::cmp)
        .build();
    for entry in walker.flatten() {
        let child = entry.path();
        if child == path {
            continue;
        }
        let Ok(rel) = child.strip_prefix(path) else {
            continue;
        };
        let depth = rel.components().count();
        if depth > TREE_MAX_DEPTH + 1 {
            continue;
        }
        let parent = rel.parent().map(relative_path).unwrap_or_default();
        let name = rel.file_name().unwrap_or_default().to_string_lossy();
        let name = if child.is_dir() {
            format!("{name}/")
        } else {
            name.into_owned()
        };
        children.entry(parent).or_default().push(name);
    }

    let mut lines = Vec::new();
    emit_tree(&mut lines, &children, "", "", 0);
    if lines.is_empty() {
        lines.push("(empty or all descendants ignored)".into());
    }
    if lines.len() > TREE_MAX_ENTRIES {
        lines.truncate(TREE_MAX_ENTRIES);
        lines.push("… (truncated)".into());
    }
    format!(
        "Project reference directory `{relative}/` (bounded tree; file contents were not inlined):\n```text\n{}\n```",
        lines.join("\n")
    )
}

fn emit_tree(
    out: &mut Vec<String>,
    children: &BTreeMap<String, Vec<String>>,
    parent: &str,
    indent: &str,
    depth: usize,
) {
    let Some(names) = children.get(parent) else {
        return;
    };
    for name in names.iter().take(TREE_MAX_PER_DIR) {
        if out.len() >= TREE_MAX_ENTRIES {
            return;
        }
        out.push(format!("{indent}{name}"));
        if depth < TREE_MAX_DEPTH {
            if let Some(dir) = name.strip_suffix('/') {
                let next = if parent.is_empty() {
                    dir.to_string()
                } else {
                    format!("{parent}/{dir}")
                };
                emit_tree(out, children, &next, &format!("{indent}  "), depth + 1);
            }
        }
    }
    if names.len() > TREE_MAX_PER_DIR && out.len() < TREE_MAX_ENTRIES {
        out.push(format!(
            "{indent}… (+{} more)",
            names.len() - TREE_MAX_PER_DIR
        ));
    }
}

/// Parse syntax only; existence and project-boundary validation happen later.
pub fn parse_mentions(text: &str) -> Vec<String> {
    let mut mentions = Vec::new();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index].1 != '@' || !mention_boundary(index, &chars) {
            index += 1;
            continue;
        }
        let start = index + 1;
        if start >= chars.len() {
            break;
        }
        let (end, value) = if chars[start].1 == '"' {
            let mut end = start + 1;
            while end < chars.len() && chars[end].1 != '"' {
                end += 1;
            }
            if end == chars.len() {
                index += 1;
                continue;
            }
            (end + 1, collect_chars(&chars[start + 1..end]))
        } else {
            let mut end = start;
            while end < chars.len() && is_path_char(chars[end].1) {
                end += 1;
            }
            (end, collect_chars(&chars[start..end]))
        };
        if !value.is_empty() {
            mentions.push(value);
        }
        index = end;
    }
    mentions
}

fn mention_boundary(index: usize, chars: &[(usize, char)]) -> bool {
    index == 0 || !chars[index - 1].1.is_alphanumeric() && chars[index - 1].1 != '_'
}

fn is_path_char(c: char) -> bool {
    !c.is_whitespace()
        && !matches!(
            c,
            '@' | '`' | '"' | '\'' | ')' | '(' | '[' | ']' | '{' | '}' | ',' | ';' | ':'
        )
}

fn collect_chars(chars: &[(usize, char)]) -> String {
    chars.iter().map(|(_, c)| *c).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_paths_but_not_email_addresses() {
        assert_eq!(
            parse_mentions("read @src/main.rs and @\"docs/my plan.md\""),
            ["src/main.rs", "docs/my plan.md"]
        );
        assert!(parse_mentions("mail me@example.com or write @ later").is_empty());
    }

    #[test]
    fn indexes_files_and_directories_without_build_products() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        fs::write(dir.path().join("src/nested/lib.rs"), "pub fn x() {}").unwrap();
        fs::write(dir.path().join("target/debug/nope"), "no").unwrap();
        let index = index_project(dir.path());
        assert!(index.iter().any(|entry| entry.path == "src/nested/lib.rs"));
        assert!(index
            .iter()
            .any(|entry| entry.path == "src/nested" && entry.kind == ReferenceKind::Directory));
        assert!(!index.iter().any(|entry| entry.path.starts_with("target/")));
    }

    #[tokio::test]
    async fn expands_small_file_and_directory_without_changing_user_text() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn answer() -> u8 { 42 }",
        )
        .unwrap();
        let original = "review @src/lib.rs and @src";
        let expanded = expand_references(
            dir.path().to_path_buf(),
            vec![ContentBlock::Text {
                text: original.into(),
            }],
        )
        .await;
        assert!(matches!(
            &expanded.blocks[0],
            ContentBlock::Text { text } if text == original
        ));
        let extras: Vec<&str> = expanded.blocks[1..]
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(extras
            .iter()
            .any(|text| text.contains("complete contents") && text.contains("answer")));
        assert!(extras.iter().any(|text| text.contains("bounded tree")));
    }

    #[tokio::test]
    async fn inlines_textual_markdown_larger_than_the_old_byte_limit() {
        let dir = tempfile::tempdir().unwrap();
        let markdown = "- 项目\n".repeat(3_500);
        assert!(markdown.len() > 12 * 1024);
        fs::write(dir.path().join("notes.md"), &markdown).unwrap();

        let expanded = expand_references(
            dir.path().to_path_buf(),
            vec![ContentBlock::Text {
                text: "review @notes.md".into(),
            }],
        )
        .await;
        let rendered = match &expanded.blocks[1] {
            ContentBlock::Text { text } => text,
            _ => panic!("reference should be text"),
        };
        assert!(rendered.contains("complete contents"));
        assert!(rendered.contains(markdown.trim_end()));
    }

    #[tokio::test]
    async fn excerpts_a_single_file_that_exceeds_its_token_budget() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("large.md"), "token ".repeat(7_000)).unwrap();
        let expanded = expand_references(
            dir.path().to_path_buf(),
            vec![ContentBlock::Text {
                text: "review @large.md".into(),
            }],
        )
        .await;
        let rendered = match &expanded.blocks[1] {
            ContentBlock::Text { text } => text,
            _ => panic!("reference should be text"),
        };
        assert!(rendered.contains("6,000-token per-file reference budget"));
        assert!(rendered.contains("middle was omitted"));
    }

    #[tokio::test]
    async fn shared_budget_omits_later_references_with_an_explanation() {
        let dir = tempfile::tempdir().unwrap();
        let content = "abc\n".repeat(3_000);
        fs::write(dir.path().join("one.md"), &content).unwrap();
        fs::write(dir.path().join("two.md"), &content).unwrap();
        let expanded = expand_references(
            dir.path().to_path_buf(),
            vec![ContentBlock::Text {
                text: "review @one.md @two.md".into(),
            }],
        )
        .await;
        let text: Vec<&str> = expanded.blocks[1..]
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.iter().any(|text| text.contains("complete contents")));
        assert!(text
            .iter()
            .any(|text| text.contains("reference budget is exhausted")));
    }

    #[tokio::test]
    async fn refuses_reference_escaping_project_root() {
        let dir = tempfile::tempdir().unwrap();
        let parent_file = dir.path().parent().unwrap().join("outside-reference.txt");
        fs::write(&parent_file, "secret").unwrap();
        let expanded = expand_references(
            dir.path().to_path_buf(),
            vec![ContentBlock::Text {
                text: "@../outside-reference.txt".into(),
            }],
        )
        .await;
        assert_eq!(expanded.blocks.len(), 1);
        let _ = fs::remove_file(parent_file);
    }
}
