use std::path::Path;

const DEFAULT_READ_LIMIT: usize = 2000;
/// Requests below this are widened: extra lines are cheap, but a model
/// walking a file in 10-line slices costs a round-trip per slice.
const MIN_READ_WINDOW: usize = 120;
/// Per-line ceiling. The real constraint on a read is `MAX_READ_OUTPUT_BYTES`
/// below; this only stops one minified/JSONL line from spending the whole
/// budget on noise. It is deliberately two orders of magnitude above prose,
/// config and long markdown lines — a second gate that fires on ordinary
/// files produces false positives that cost the model a shell round-trip to
/// undo (the previous 500 did exactly that).
const MAX_LINE_CHARS: usize = 16384;
/// Files above this are never slurped into memory. A range read of a giant
/// log/dataset belongs to grep or `sed -n`, not a full load.
const MAX_READ_FILE_BYTES: u64 = 10 * 1024 * 1024;
/// Cap the bytes a single read emits into context, independent of the line
/// count — 2000 lines of long lines would otherwise be ~1 MB.
const MAX_READ_OUTPUT_BYTES: usize = 128 * 1024;
/// Largest encoded image accepted before decode/normalization. This coarse
/// source-byte gate avoids spending CPU on absurd uploads; the normalized
/// result still has the stricter inline-byte limit in `core::images`.
const MAX_IMAGE_SOURCE_BYTES: u64 = 20 * 1024 * 1024;

fn rel<'a>(path: &'a Path, cwd: &Path) -> &'a Path {
    path.strip_prefix(cwd).unwrap_or(path)
}

/// Windows rejects a write while another process has the file memory-mapped
/// with `ERROR_USER_MAPPED_FILE` (1224). This is normally transient (for
/// example, an editor or indexer releasing a just-read file).
#[cfg(windows)]
fn is_windows_user_mapped_file(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(1224)
}

#[cfg(not(windows))]
fn is_windows_user_mapped_file(_error: &std::io::Error) -> bool {
    false
}

async fn write_with_windows_retry(path: &Path, content: &[u8]) -> std::io::Result<()> {
    match tokio::fs::write(path, content).await {
        Err(error) if is_windows_user_mapped_file(&error) => {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            tokio::fs::write(path, content).await
        }
        result => result,
    }
}

fn write_error(path: &Path, error: &std::io::Error) -> String {
    if is_windows_user_mapped_file(error) {
        format!(
            "cannot write {}: {error}. Windows has the file temporarily mapped or locked \
             (os error 1224); retried once after 50ms. Close the program holding it and retry.",
            path.display()
        )
    } else {
        format!("cannot write {}: {error}", path.display())
    }
}

/// Result of rendering numbered lines: what to emit, how far it got (so the
/// caller can say where to resume), and which lines lost content on the way.
struct Numbered {
    text: String,
    emitted: usize,
    /// `(1-based line number, the line's true char count)` per clipped line.
    /// Never silently drop content: what the model gets back is the basis for
    /// its next `edit`, so a clip it does not know about becomes a failed
    /// match one turn later.
    clipped: Vec<(usize, usize)>,
}

/// Render numbered lines until the line count runs out or the byte budget is
/// hit.
fn numbered_capped(lines: &[impl AsRef<str>], start: usize, budget: usize) -> Numbered {
    use std::fmt::Write as _;

    // One buffer for the whole read; a `format!` per line would allocate once
    // per line of every file the model reads.
    let mut text = String::new();
    let mut emitted = 0;
    let mut clipped_lines = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let line = line.as_ref();
        let clipped = clip(line);
        // A row is the number, a tab, the line and a newline. Always emit at
        // least one so a single huge line still makes progress.
        if emitted > 0 && text.len() + clipped.len() + 8 > budget {
            break;
        }
        if matches!(clipped, std::borrow::Cow::Owned(_)) {
            clipped_lines.push((start + i, line.chars().count()));
        }
        let _ = writeln!(text, "{:>6}\t{clipped}", start + i);
        emitted += 1;
    }
    Numbered {
        text,
        emitted,
        clipped: clipped_lines,
    }
}

/// Long lines are clipped by *character* count, so a wide line cannot blow the
/// output budget. Borrowed unless it actually needs clipping.
///
/// The marker is self-describing (matching grep's `…[+N bytes]`) rather than a
/// bare ellipsis: a bare `…` is indistinguishable from file content, so the
/// model copies it into an `edit` and only finds out at the no-match error.
fn clip(line: &str) -> std::borrow::Cow<'_, str> {
    // Cheap reject: a line can only exceed the char limit if it exceeds it in
    // bytes, and most lines are far below.
    if line.len() <= MAX_LINE_CHARS {
        return std::borrow::Cow::Borrowed(line);
    }
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((cut, _)) => std::borrow::Cow::Owned(format!(
            "{}…[+{} chars]",
            &line[..cut],
            line[cut..].chars().count()
        )),
        None => std::borrow::Cow::Borrowed(line),
    }
}

/// Tail note naming every clipped line, so the model knows which lines it must
/// not reuse verbatim and how to get the real text.
fn clip_note(clipped: &[(usize, usize)]) -> Option<String> {
    let (first, total) = *clipped.first()?;
    let which = if clipped.len() == 1 {
        format!("line {first} was clipped at {MAX_LINE_CHARS} of {total} chars")
    } else {
        let numbers: Vec<String> = clipped.iter().map(|(n, _)| n.to_string()).collect();
        format!(
            "lines {} were clipped at {MAX_LINE_CHARS} chars",
            numbers.join(", ")
        )
    };
    Some(format!(
        "note: {which}; the \u{2026}[+N chars] marker is not file content, so a clipped \
         line cannot be used as an edit old_string. Fetch such a line verbatim with grep \
         (narrow pattern) or shell if you need it."
    ))
}

fn numbered(lines: &[impl AsRef<str>], start: usize) -> String {
    numbered_capped(lines, start, usize::MAX).text
}

/// Self-healing ENOENT: show what IS there so the model can correct the
/// path without another exploratory turn.
fn not_found_help(path: &Path) -> String {
    let mut msg = format!("File not found: {}", path.display());
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    match parent {
        Some(dir) if dir.is_dir() => {
            let mut entries: Vec<String> = std::fs::read_dir(dir)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| {
                            let name = e.file_name().to_string_lossy().into_owned();
                            if e.path().is_dir() {
                                format!("{name}/")
                            } else {
                                name
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            entries.sort();
            entries.truncate(20);
            msg.push_str(&format!(
                "\nThe directory {} exists and contains: {}",
                dir.display(),
                entries.join(", ")
            ));
        }
        Some(dir) => {
            msg.push_str(&format!(
                "\nThe directory {} does not exist either.",
                dir.display()
            ));
        }
        None => {}
    }
    msg
}

mod append;
mod edit;
mod read;
mod write;

#[cfg(test)]
mod tests;

pub(crate) use append::AppendTool;
pub(crate) use edit::EditTool;
pub(crate) use read::ReadTool;
pub(crate) use write::WriteTool;
