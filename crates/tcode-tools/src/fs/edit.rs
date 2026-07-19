use std::collections::HashSet;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::freshness::content_hash;
use tcode_core::{AutoSafety, BatchPolicy, PermissionRequest, Tool, ToolCtx, ToolOutput};

use crate::redact::{marker_error, read_marker};

use super::{not_found_help, numbered, rel, write_error, write_with_windows_retry};

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn batch_policy(&self) -> BatchPolicy {
        BatchPolicy::ParallelPerFile
    }

    fn batch_label(&self, inputs: &[&Value]) -> String {
        let changes = inputs.len();
        let files: HashSet<&str> = inputs
            .iter()
            .filter_map(|input| input["path"].as_str())
            .collect();
        if changes == files.len() {
            format!(
                "Edit {changes} {}",
                if changes == 1 { "file" } else { "files" }
            )
        } else {
            format!(
                "Edit {changes} {} across {} {}",
                if changes == 1 { "change" } else { "changes" },
                files.len(),
                if files.len() == 1 { "file" } else { "files" },
            )
        }
    }

    fn description(&self) -> &str {
        "Exact string replacement in a UTF-8 text file. `old_string` must match the \
         current content exactly (including whitespace; line endings may be \
         LF/CRLF and are normalized to the file's style) and be unique unless \
         replace_all is set. `target_line` selects one otherwise-ambiguous match \
         by its 1-based starting line; it is mutually exclusive with replace_all. \
         Only edit text you have actually seen in this session (read or grep \
         output both count) and whose surroundings you understand; if you are \
         unsure of the exact content or the impact of the change, read the file \
         first instead of guessing. A separate read is not required when grep \
         already showed you the exact text with enough context around it."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" },
                "replace_all": { "type": "boolean", "default": false },
                "target_line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based start line of the one occurrence to replace; use a line reported by a prior ambiguous-match error. Cannot be combined with replace_all."
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn permission(&self, input: &Value) -> PermissionRequest {
        let path = input["path"].as_str().unwrap_or("?");
        PermissionRequest::Ask {
            descriptor: format!("edit({path})"),
            aliases: Vec::new(),
            summary: format!("edit {path}"),
            is_edit: true,
        }
    }

    fn auto_safety(&self, _input: &Value) -> AutoSafety {
        AutoSafety::AllowInProjectOrScratchEdit
    }

    fn touches(&self, input: &Value) -> Option<String> {
        input["path"].as_str().map(String::from)
    }

    fn context_paths(&self, input: &Value) -> Vec<String> {
        input["path"]
            .as_str()
            .map(String::from)
            .into_iter()
            .collect()
    }

    fn is_mutating(&self) -> bool {
        true
    }

    async fn run(&self, input: Value, ctx: &ToolCtx, _cancel: &CancellationToken) -> ToolOutput {
        let (Some(path_str), Some(old), Some(new)) = (
            input["path"].as_str(),
            input["old_string"].as_str(),
            input["new_string"].as_str(),
        ) else {
            return ToolOutput::err("missing required parameters: path, old_string, new_string");
        };
        if old.is_empty() {
            return ToolOutput::err("old_string must not be empty");
        }
        if old == new {
            return ToolOutput::err("old_string and new_string are identical");
        }
        // Checked before matching, which a clipped `old_string` could never
        // survive anyway: this turns the "no match, why?" puzzle into one
        // sentence naming the cause. On `new_string` it is the real guard —
        // that one would have been written to disk.
        for (field, value) in [("old_string", old), ("new_string", new)] {
            if let Some(kind) = read_marker(value) {
                return ToolOutput::err(marker_error(kind, field));
            }
        }
        let replace_all = input["replace_all"].as_bool().unwrap_or(false);
        let target_line = match input.get("target_line") {
            None | Some(Value::Null) => None,
            Some(value) => match value.as_u64().and_then(|line| usize::try_from(line).ok()) {
                Some(line @ 1..) => Some(line),
                _ => return ToolOutput::err("target_line must be a positive 1-based line number"),
            },
        };
        if replace_all && target_line.is_some() {
            return ToolOutput::err("target_line cannot be combined with replace_all=true");
        }
        let path = ctx.resolve(path_str);
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolOutput::err(not_found_help(&path));
            }
            Err(e) => return ToolOutput::err(format!("cannot read {}: {e}", path.display())),
        };
        // No read-before-edit gate: the exact, unique match against current
        // disk content is the verification. A stale or guessed old_string
        // fails safely below. (Lock scope: see the note in `write`.)
        let seen = {
            let freshness = ctx.freshness.lock().expect("freshness lock");
            freshness.seen_current(&path, content_hash(&bytes))
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => {
                return ToolOutput::err(format!(
                    "{} is not valid UTF-8; edit only supports text files and will not rewrite bytes lossily",
                    rel(&path, &ctx.cwd).display()
                ));
            }
        };

        let plan = match replacement_plan(&text, old, new) {
            Ok(Some(plan)) => match target_line {
                Some(line) => match select_exact_match_at_line(&text, plan, line) {
                    Ok(plan) => plan,
                    Err(error) => return ToolOutput::err(error),
                },
                None => plan,
            },
            Ok(None) => {
                let mut msg = near_miss_help(&text, old);
                if !seen {
                    msg.push_str(
                        "\nnote: you have not read the current version of this \
                             file; read it to get the exact text.",
                    );
                }
                return ToolOutput::err(msg);
            }
            Err(candidates) => {
                let Some(line) = target_line else {
                    return ToolOutput::err(format!(
                        "old_string has multiple whitespace/punctuation-normalized matches; \
                         add enough exact surrounding context to identify one occurrence, or \
                         pass target_line from one candidate below.\nCandidates:\n{}",
                        candidate_help(&text, &candidates).join("\n\n")
                    ));
                };
                let Some(candidate) = select_candidate_at_line(&text, &candidates, line) else {
                    return ToolOutput::err(format!(
                        "target_line {line} does not identify exactly one normalized match. \
                         Re-read the candidate list and choose a unique starting line.\nCandidates:\n{}",
                        candidate_help(&text, &candidates).join("\n\n")
                    ));
                };
                ReplacementPlan {
                    old: text[candidate.at..candidate.at + candidate.len].to_string(),
                    new: normalize_newlines(new, dominant_line_ending(&text)),
                    count: 1,
                    at: candidate.at,
                }
            }
        };
        match plan.count {
            0 => unreachable!("replacement_plan only returns matching needles"),
            1 => {}
            _ if target_line.is_some() => unreachable!("target_line selects one exact match"),
            n if !replace_all => {
                let occurrences = occurrence_help(&text, &plan.old, 8);
                return ToolOutput::err(format!(
                    "old_string appears {n} times; add surrounding context to make it \
                     unique, pass target_line from one occurrence below, or set replace_all=true.\nOccurrences:\n{}",
                    occurrences.join("\n")
                ));
            }
            _ => {}
        }

        let new_text = if replace_all {
            text.replace(&plan.old, &plan.new)
        } else {
            replace_once_at(&text, &plan)
        };
        if let Err(error) = write_with_windows_retry(&path, new_text.as_bytes()).await {
            return ToolOutput::err(write_error(&path, &error));
        }
        // Show the edited region so the model sees the result without
        // re-reading the file. Everything before the first replacement is
        // untouched, so its offset in the new text is the one the plan already
        // found — no second search of the file, and no `Vec` of all its lines.
        let line_no = new_text[..plan.at].bytes().filter(|b| *b == b'\n').count() + 1;
        let start = line_no.saturating_sub(3).max(1);
        let window = plan.new.lines().count() + 5;
        let shown: Vec<&str> = new_text.lines().skip(start - 1).take(window).collect();
        let snippet = numbered(&shown, start);
        // Record exactly what reached the model: the snippet above, under the
        // new content hash (same principle as `read`). Not `record_write` —
        // that would mark the whole file as seen and let a later offset read
        // incorrectly return an unchanged stub. `record_read` clears the old
        // version's ranges, which is the conservative truth: line numbers
        // after the edit point may have shifted. Without this record the
        // stored hash would stay stale and `append`/`write` gates would
        // mistake our own edit for an external change.
        if !shown.is_empty() {
            let shown_end = start + shown.len() - 1;
            let range = if start == 1 && shown_end >= new_text.lines().count() {
                None
            } else {
                Some((start, shown_end))
            };
            // Lock scope: see the note in `write`.
            ctx.freshness.lock().expect("freshness lock").record_read(
                &path,
                content_hash(new_text.as_bytes()),
                range,
            );
        }
        ToolOutput::ok(format!(
            "edited {} ({} replacement{}). Result:\n{snippet}",
            rel(&path, &ctx.cwd).display(),
            if replace_all { plan.count } else { 1 },
            if replace_all && plan.count > 1 {
                "s"
            } else {
                ""
            },
        ))
    }
}

pub(super) struct ReplacementPlan {
    pub(super) old: String,
    pub(super) new: String,
    pub(super) count: usize,
    /// Byte offset of the first match. The text before it survives the
    /// replacement unchanged, so this is also where the new text lands.
    at: usize,
}

/// A non-exact recovery match is safe only when it identifies one location.
/// It must never silently pick the first of several whitespace-equivalent
/// blocks: that would violate edit's public uniqueness contract.
#[derive(Debug)]
pub(super) struct MatchCandidate {
    pub(super) at: usize,
    len: usize,
}

enum NormalizedMatch {
    NotFound,
    Unique(String),
    Ambiguous(Vec<MatchCandidate>),
}

impl ReplacementPlan {
    /// `old` must be a substring of `text`; `None` when it does not occur.
    fn locate(text: &str, old: String, new: String) -> Option<Self> {
        let mut matches = text.match_indices(&old);
        let at = matches.next()?.0;
        Some(ReplacementPlan {
            count: 1 + matches.count(),
            old,
            new,
            at,
        })
    }
}

fn match_start_line(text: &str, at: usize) -> usize {
    text[..at].bytes().filter(|byte| *byte == b'\n').count() + 1
}

fn select_exact_match_at_line(
    text: &str,
    mut plan: ReplacementPlan,
    target_line: usize,
) -> Result<ReplacementPlan, String> {
    let mut matches = text
        .match_indices(&plan.old)
        .map(|(at, _)| at)
        .filter(|at| match_start_line(text, *at) == target_line);
    let Some(at) = matches.next() else {
        return Err(format!(
            "target_line {target_line} does not contain an exact old_string occurrence"
        ));
    };
    if matches.next().is_some() {
        return Err(format!(
            "target_line {target_line} contains multiple old_string occurrences; add surrounding context"
        ));
    }
    plan.at = at;
    plan.count = 1;
    Ok(plan)
}

fn select_candidate_at_line<'a>(
    text: &str,
    candidates: &'a [MatchCandidate],
    target_line: usize,
) -> Option<&'a MatchCandidate> {
    let mut matches = candidates
        .iter()
        .filter(|candidate| match_start_line(text, candidate.at) == target_line);
    let candidate = matches.next()?;
    matches.next().is_none().then_some(candidate)
}

fn replace_once_at(text: &str, plan: &ReplacementPlan) -> String {
    let end = plan.at + plan.old.len();
    debug_assert_eq!(&text[plan.at..end], plan.old);
    let mut result = String::with_capacity(text.len() - plan.old.len() + plan.new.len());
    result.push_str(&text[..plan.at]);
    result.push_str(&plan.new);
    result.push_str(&text[end..]);
    result
}

pub(super) fn replacement_plan(
    text: &str,
    old: &str,
    new: &str,
) -> Result<Option<ReplacementPlan>, Vec<MatchCandidate>> {
    let eol = dominant_line_ending(text);
    let mut candidates = Vec::new();
    candidates.push((old.to_string(), normalize_newlines(new, eol)));
    if old.contains('\n') || old.contains('\r') {
        candidates.push((normalize_newlines(old, eol), normalize_newlines(new, eol)));
        candidates.push((normalize_newlines(old, "\n"), normalize_newlines(new, "\n")));
        candidates.push((
            normalize_newlines(old, "\r\n"),
            normalize_newlines(new, "\r\n"),
        ));
    }

    let mut seen = std::collections::HashSet::new();
    let exact = candidates.into_iter().find_map(|(old, new)| {
        if !seen.insert(old.clone()) {
            return None;
        }
        ReplacementPlan::locate(text, old, new)
    });
    if exact.is_some() {
        return Ok(exact);
    }
    // Last resort: models often emit typographic punctuation (– " " …) where
    // the file has plain ASCII, or drift a space inside an otherwise-identical
    // block. Match with those differences normalized away, but splice the
    // *actual* file bytes back in so nothing else is disturbed. Recovery is
    // intentionally stricter than exact replacement: a choice among several
    // normalized matches is a guess, not a self-heal.
    let normalized = match find_punct_normalized(text, old) {
        NormalizedMatch::NotFound => match find_ws_normalized(text, old) {
            // Reflow (cargo fmt joining/splitting lines) changes the line
            // count, which the line-anchored matcher above cannot follow.
            // Fall through to a whitespace-insensitive match across newlines.
            NormalizedMatch::NotFound => find_reflow_normalized(text, old),
            found => found,
        },
        found => found,
    };
    match normalized {
        NormalizedMatch::NotFound => Ok(None),
        NormalizedMatch::Unique(orig) => Ok(ReplacementPlan::locate(
            text,
            orig,
            normalize_newlines(new, eol),
        )),
        NormalizedMatch::Ambiguous(candidates) => Err(candidates),
    }
}

/// Map common typographic punctuation to its ASCII equivalent. Only 1-char →
/// 1-char maps, so char positions stay aligned between original and normalized.
fn normalize_punct(c: char) -> char {
    match c {
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-', // hyphens, dashes, minus
        '\u{2018}' | '\u{2019}' | '\u{201B}' => '\'', // single quotes
        '\u{201C}' | '\u{201D}' | '\u{201F}' => '"', // double quotes
        _ => c,
    }
}

/// Find `old` in `text` comparing with punctuation normalized, and return the
/// exact original substring at that location (so the real bytes are replaced).
/// Returns `Ambiguous` rather than silently choosing when multiple file ranges
/// normalize to the same requested text.
fn find_punct_normalized(text: &str, old: &str) -> NormalizedMatch {
    let pat: Vec<char> = old.chars().map(normalize_punct).collect();
    if pat.iter().copied().eq(old.chars()) {
        return NormalizedMatch::NotFound; // exact pass already tried this
    }
    let tchars: Vec<(usize, char)> = text.char_indices().collect();
    if pat.is_empty() || pat.len() > tchars.len() {
        return NormalizedMatch::NotFound;
    }
    let matches: Vec<(usize, String)> = (0..=tchars.len() - pat.len())
        .filter_map(|i| {
            let window = &tchars[i..i + pat.len()];
            window
                .iter()
                .map(|(_, c)| normalize_punct(*c))
                .eq(pat.iter().copied())
                .then(|| {
                    (
                        window[0].0,
                        window.iter().map(|(_, c)| c).collect::<String>(),
                    )
                })
        })
        .take(6)
        .collect();
    match matches.as_slice() {
        [] => NormalizedMatch::NotFound,
        [(_, found)] => NormalizedMatch::Unique(found.clone()),
        _ => NormalizedMatch::Ambiguous(
            matches
                .into_iter()
                .map(|(at, found)| MatchCandidate {
                    at,
                    len: found.len(),
                })
                .collect(),
        ),
    }
}

/// Locate `old` in `text` line-by-line, ignoring *every* whitespace difference
/// (indentation, trailing, and internal runs) plus typographic punctuation, and
/// return the exact original file substring spanning the matched lines. This is
/// the pattern behind the most common near-miss: the model reproduces a block
/// verbatim but drifts one space, so nothing else in the block differs.
///
/// Only whole-line blocks match — a sub-line fragment fails here and falls
/// through (its whitespace rarely differs, and the exact pass already tried it).
/// Because the real file bytes are spliced back, the file's true formatting is
/// what survives; the model's whitespace guess is discarded.
fn find_ws_normalized(text: &str, old: &str) -> NormalizedMatch {
    let key = |s: &str| -> String {
        s.chars()
            .filter(|c| !c.is_whitespace())
            .map(normalize_punct)
            .collect()
    };
    let old_keys: Vec<String> = old.lines().map(key).collect();
    // Need at least one line with real content to anchor on; an all-blank
    // needle would match anywhere.
    if old_keys.iter().all(String::is_empty) {
        return NormalizedMatch::NotFound;
    }
    // (byte_start, content_without_terminator, full_piece_len, key) per file
    // line. The key is computed once per line, not once per (window, line):
    // re-keying inside the sliding comparison below made a failed edit on a
    // large file quadratic in allocations.
    let mut lines: Vec<(usize, &str, usize, String)> = Vec::new();
    let mut off = 0usize;
    for piece in text.split_inclusive('\n') {
        let content = piece
            .strip_suffix('\n')
            .unwrap_or(piece)
            .strip_suffix('\r')
            .unwrap_or_else(|| piece.strip_suffix('\n').unwrap_or(piece));
        lines.push((off, content, piece.len(), key(content)));
        off += piece.len();
    }
    let m = old_keys.len();
    if m == 0 || m > lines.len() {
        return NormalizedMatch::NotFound;
    }
    let include_trailing = old.ends_with('\n');
    let matches: Vec<(usize, String)> = (0..=lines.len() - m)
        .filter_map(|w| {
            let matched = (0..m).all(|k| lines[w + k].3 == old_keys[k]);
            if !matched {
                return None;
            }
            let start = lines[w].0;
            let (last_off, last_content, last_len, _) = lines[w + m - 1];
            let end = if include_trailing {
                last_off + last_len
            } else {
                last_off + last_content.len()
            };
            Some((start, text[start..end].to_string()))
        })
        .take(6)
        .collect();
    match matches.as_slice() {
        [] => NormalizedMatch::NotFound,
        [(_, found)] => NormalizedMatch::Unique(found.clone()),
        _ => NormalizedMatch::Ambiguous(
            matches
                .into_iter()
                .map(|(at, found)| MatchCandidate {
                    at,
                    len: found.len(),
                })
                .collect(),
        ),
    }
}

/// Match `old` in `text` ignoring *all* whitespace, newlines included, plus
/// typographic punctuation, and return the exact original file substring at the
/// match. This is the reflow case the line-anchored `find_ws_normalized` cannot
/// reach: cargo fmt joined a call onto one line or split it across several, so
/// the line *count* changed and nothing lines up.
///
/// Deliberately the loosest rung of the recovery ladder, hence last: collapsing
/// newlines lets a needle straddle token boundaries. It stays safe because the
/// exact/punct/ws passes ran first, the `Ambiguous` guard rejects a
/// looks-unique-but-isn't match, and only the real file bytes are spliced back
/// so the file's true formatting — not the model's whitespace guess — survives.
fn find_reflow_normalized(text: &str, old: &str) -> NormalizedMatch {
    // (normalized char, original byte start, original char len). The original
    // len is kept because punctuation normalization is 1-char but not 1-byte
    // (an em-dash is 3 bytes, its ASCII '-' is 1), so the byte offset must span
    // the original character.
    let haystack: Vec<(char, usize, usize)> = text
        .char_indices()
        .filter(|(_, c)| !c.is_whitespace())
        .map(|(i, c)| (normalize_punct(c), i, c.len_utf8()))
        .collect();
    let needle: Vec<char> = old
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(normalize_punct)
        .collect();
    let m = needle.len();
    // An all-whitespace needle carries no anchor and would match anywhere.
    if m == 0 || m > haystack.len() {
        return NormalizedMatch::NotFound;
    }
    // Slide char-by-char, comparing without allocating; only a hit materializes
    // the original substring (same shape as `find_punct_normalized`).
    let matches: Vec<(usize, String)> = (0..=haystack.len() - m)
        .filter_map(|i| {
            if !(0..m).all(|k| haystack[i + k].0 == needle[k]) {
                return None;
            }
            let start = haystack[i].1;
            let (_, last_start, last_len) = haystack[i + m - 1];
            Some((start, text[start..last_start + last_len].to_string()))
        })
        .take(6)
        .collect();
    match matches.as_slice() {
        [] => NormalizedMatch::NotFound,
        [(_, found)] => NormalizedMatch::Unique(found.clone()),
        _ => NormalizedMatch::Ambiguous(
            matches
                .into_iter()
                .map(|(at, found)| MatchCandidate {
                    at,
                    len: found.len(),
                })
                .collect(),
        ),
    }
}

fn normalize_newlines(s: &str, eol: &str) -> String {
    let lf = s.replace("\r\n", "\n").replace('\r', "\n");
    if eol == "\n" {
        lf
    } else {
        lf.replace('\n', eol)
    }
}

fn dominant_line_ending(text: &str) -> &'static str {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count().saturating_sub(crlf);
    if crlf > lf {
        "\r\n"
    } else {
        "\n"
    }
}

pub(super) const MAX_EDIT_CANDIDATES: usize = 5;
const CANDIDATE_CONTEXT_LINES: usize = 2;
const MAX_CANDIDATE_LINE_CHARS: usize = 120;

/// Show a small, line-numbered window around each rejected exact occurrence.
/// The model still has to submit a unique `old_string`; line numbers are
/// evidence for disambiguation, never an alternate edit addressing scheme.
pub(super) fn occurrence_help(text: &str, needle: &str, limit: usize) -> Vec<String> {
    let candidates = text
        .match_indices(needle)
        .take(limit)
        .map(|(at, _)| MatchCandidate {
            at,
            len: needle.len(),
        })
        .collect::<Vec<_>>();
    candidate_help(text, &candidates)
}

pub(super) fn candidate_help(text: &str, candidates: &[MatchCandidate]) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut rendered: Vec<String> = candidates
        .iter()
        .take(MAX_EDIT_CANDIDATES)
        .enumerate()
        .map(|(index, candidate)| {
            let start_line = text[..candidate.at].bytes().filter(|b| *b == b'\n').count() + 1;
            let matched_lines = text[candidate.at..candidate.at + candidate.len]
                .lines()
                .count()
                .max(1);
            let end_line = start_line + matched_lines - 1;
            let first = start_line.saturating_sub(CANDIDATE_CONTEXT_LINES + 1);
            let last = (end_line + CANDIDATE_CONTEXT_LINES).min(lines.len());
            let window = lines[first..last]
                .iter()
                .enumerate()
                .map(|(offset, line)| {
                    format!("{:>6}\t{}", first + offset + 1, candidate_line(line))
                })
                .collect::<Vec<_>>()
                .join("\n");
            let range = if start_line == end_line {
                format!("line {start_line}")
            } else {
                format!("lines {start_line}–{end_line}")
            };
            format!("  candidate {} ({range}):\n{window}", index + 1)
        })
        .collect();
    if candidates.len() > MAX_EDIT_CANDIDATES {
        rendered.push(format!(
            "  … {} additional candidate matches omitted",
            candidates.len() - MAX_EDIT_CANDIDATES
        ));
    }
    rendered
}

fn candidate_line(line: &str) -> String {
    let mut clipped = line
        .chars()
        .take(MAX_CANDIDATE_LINE_CHARS)
        .collect::<String>();
    if line.chars().nth(MAX_CANDIDATE_LINE_CHARS).is_some() {
        clipped.push('…');
    }
    clipped
}

/// Self-healing "old_string not found": show every bounded diagnostic hint
/// rather than biasing the model toward the first matching region in the file.
pub(super) fn near_miss_help(text: &str, old: &str) -> String {
    let mut msg = String::from("old_string not found in file.");
    let probe = old
        .lines()
        .map(str::trim)
        .filter(|line| line.len() >= 8)
        .max_by_key(|line| line.len());
    let Some(probe) = probe else {
        msg.push_str(
            " No similar line found — the content may differ more than expected; re-read the relevant range.",
        );
        return msg;
    };

    let (candidates, omitted) = similar_line_candidates(text, probe);
    if candidates.is_empty() {
        msg.push_str(
            " No similar line found — the content may differ more than expected; re-read the relevant range.",
        );
        return msg;
    }

    msg.push_str(
        " Similar locations below are diagnostic hints, not replacement targets. \
         Add unique exact surrounding context and retry:\n",
    );
    msg.push_str(&candidate_help(text, &candidates).join("\n\n"));
    if omitted > 0 {
        let suffix = if omitted == 1 { "" } else { "s" };
        msg.push_str(&format!(
            "\n  … {omitted} additional similar location{suffix} omitted"
        ));
    }
    msg.push_str("\nRe-read the relevant range if none of these is the intended edit.");
    msg
}

fn similar_line_candidates(text: &str, probe: &str) -> (Vec<MatchCandidate>, usize) {
    let mut candidates = Vec::with_capacity(MAX_EDIT_CANDIDATES);
    let mut total: usize = 0;
    let mut at = 0;
    for piece in text.split_inclusive('\n') {
        let line = piece.strip_suffix('\n').unwrap_or(piece);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.contains(probe) || line.trim() == probe {
            total += 1;
            if candidates.len() < MAX_EDIT_CANDIDATES {
                candidates.push(MatchCandidate {
                    at,
                    len: line.len(),
                });
            }
        }
        at += piece.len();
    }
    let omitted = total.saturating_sub(candidates.len());
    (candidates, omitted)
}
