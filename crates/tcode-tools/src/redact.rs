//! Redaction of credential-shaped values in file content that `read` and
//! `grep` return.
//!
//! This is **not** a security boundary: `shell` can `cat` the same file at any
//! time. The value is structural. `read`/`grep` are the only never-asking
//! channels in the harness, so a key they return lands in the provider's
//! request, in the local session JSONL (kept for days, replayed on resume), and
//! in a context that also holds `web_fetch`. Redacting moves accidental — or
//! injected — credential reads off the un-audited channel and onto the audited
//! one (`shell` goes through the classifier in Auto Mode, and through approval
//! everywhere else).
//!
//! The rule is content-based and applies to every path, project files
//! included. A path allowlist would be a slot someone eventually stuffs
//! exceptions into.

use std::borrow::Cow;

/// Key-name segments that mark the value as a secret on their own.
const SECRET_TAILS: &[&str] = &[
    "secret",
    "token",
    "password",
    "passwd",
    "pwd",
    "credential",
    "credentials",
];

/// `key` is far too common to match alone (`primary_key`, `cache_key`,
/// `partition_key`), so it needs one of these in front of it.
const KEY_QUALIFIERS: &[&str] = &[
    "api",
    "access",
    "secret",
    "private",
    "ssh",
    "auth",
    "session",
    "signing",
    "encryption",
    "master",
    "license",
];

/// Below this a value is too short to be a real credential, and short values
/// are where false positives live (`token = "abc"` in a parser test).
const MIN_SECRET_CHARS: usize = 16;

/// Split a key name into lowercase segments on `_`, `-`, `.` and camelCase
/// boundaries, so `apiKey`, `API_KEY` and `api-key` all yield `[api, key]`.
fn segments(key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in key.chars() {
        if c == '_' || c == '-' || c == '.' {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_ascii_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Key-name test. Deliberately keyed on the *last* segment: `api_key_env`
/// ends in `env` and names an environment variable, not a secret, and
/// `max_tokens` ends in `tokens`.
fn is_secret_key(key: &str) -> bool {
    let segs = segments(key);
    let Some(last) = segs.last() else {
        return false;
    };
    if SECRET_TAILS.contains(&last.as_str()) {
        return true;
    }
    if last == "apikey" {
        return true;
    }
    if last != "key" {
        return false;
    }
    match segs.len() {
        0 => false,
        // A bare `key = "..."` in a config file is a secret often enough.
        1 => true,
        n => KEY_QUALIFIERS.contains(&segs[n - 2].as_str()),
    }
}

/// Value-shape test, applied *after* the key matches. A key name alone is not
/// enough: `api_key_env = "OPENAI_API_KEY"` and `token = "$GITHUB_TOKEN"` name
/// a secret without containing one, and redacting a pointer destroys
/// information the model needs while protecting nothing.
fn looks_like_secret(value: &str) -> bool {
    if value.chars().count() < MIN_SECRET_CHARS {
        return false;
    }
    // Real credentials have no internal whitespace; prose does.
    if value.chars().any(char::is_whitespace) {
        return false;
    }
    // Environment-variable references are pointers, not secrets.
    if value.starts_with('$') {
        return false;
    }
    if value
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return false;
    }
    true
}

/// Keep the metadata the model actually needs — that a credential is present,
/// roughly how long, and enough of a prefix to recognize the provider — and
/// drop the rest. The shape is distinctive enough to be caught by
/// [`read_marker`] if it is ever fed back into a write.
fn placeholder(value: &str) -> String {
    let prefix: String = value.chars().take(4).collect();
    format!(
        "[redacted: {} chars, starts \"{prefix}\"]",
        value.chars().count()
    )
}

/// Byte range of the key token immediately before `sep`, if there is one.
fn key_before(bytes: &[u8], sep: usize) -> Option<(usize, usize)> {
    let mut end = sep;
    while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t') {
        end -= 1;
    }
    if end > 0 && (bytes[end - 1] == b'"' || bytes[end - 1] == b'\'') {
        end -= 1;
    }
    let mut start = end;
    while start > 0 {
        let c = bytes[start - 1];
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.' {
            start -= 1;
        } else {
            break;
        }
    }
    (start < end).then_some((start, end))
}

/// Byte range of the value token after `sep`, plus whether it was quoted. For
/// a quoted value the range covers the contents only, so the quotes survive
/// and the line keeps its syntax.
fn value_after(bytes: &[u8], sep: usize) -> Option<(usize, usize)> {
    let mut start = sep + 1;
    while start < bytes.len() && (bytes[start] == b' ' || bytes[start] == b'\t') {
        start += 1;
    }
    if start >= bytes.len() {
        return None;
    }
    if bytes[start] == b'"' || bytes[start] == b'\'' {
        let quote = bytes[start];
        let inner = start + 1;
        let end = bytes[inner..].iter().position(|b| *b == quote)? + inner;
        return Some((inner, end));
    }
    let end = bytes[start..]
        .iter()
        .position(|b| matches!(b, b',' | b' ' | b'\t' | b';'))
        .map_or(bytes.len(), |offset| start + offset);
    (start < end).then_some((start, end))
}

/// Redact every `key = value` / `key: value` pair on one line whose key names
/// a secret and whose value looks like one. Returns `None` when nothing
/// matched, so callers can keep borrowing the original.
///
/// Scanning every separator (rather than only the first) is what makes a
/// single-line JSON object work; each hit resumes after the value it replaced.
pub(crate) fn redact_line(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'=' && bytes[i] != b':' {
            i += 1;
            continue;
        }
        let sep = i;
        i += 1;
        let Some((key_start, key_end)) = key_before(bytes, sep) else {
            continue;
        };
        if !is_secret_key(&line[key_start..key_end]) {
            continue;
        }
        let Some((value_start, value_end)) = value_after(bytes, sep) else {
            continue;
        };
        let value = &line[value_start..value_end];
        if !looks_like_secret(value) {
            continue;
        }
        let out = out.get_or_insert_with(String::new);
        out.push_str(&line[copied..value_start]);
        out.push_str(&placeholder(value));
        copied = value_end;
        i = value_end;
    }
    out.map(|mut out| {
        out.push_str(&line[copied..]);
        out
    })
}

fn is_pem_begin(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("-----BEGIN") && t.contains("PRIVATE KEY") && t.ends_with("-----")
}

fn is_pem_end(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("-----END") && t.ends_with("-----")
}

/// Redact a slice of file lines. The result has exactly one entry per input
/// line — `read` numbers these, so dropping or merging a line would silently
/// shift every line number after it.
///
/// PEM private keys need the block state a single line cannot carry: the
/// `BEGIN`/`END` markers stay (they say *what* is here) and the base64 body
/// between them is replaced line by line. `read id_rsa` is the ugliest leak
/// path there is and costs one rule.
///
/// The state machine starts cold at `lines[0]`, so a range read that begins
/// *inside* a key block does not recognize it. Private key files are small
/// and get read whole; not worth a full-file prescan on every read.
pub(crate) fn redact_lines<'a>(lines: &[&'a str]) -> Vec<Cow<'a, str>> {
    let mut in_pem = false;
    lines
        .iter()
        .map(|line| {
            if in_pem {
                if is_pem_end(line) {
                    in_pem = false;
                    return Cow::Borrowed(*line);
                }
                let body = line.trim();
                if body.is_empty() {
                    return Cow::Borrowed(*line);
                }
                return Cow::Owned(format!("[redacted: {} chars]", body.chars().count()));
            }
            if is_pem_begin(line) {
                in_pem = true;
                return Cow::Borrowed(*line);
            }
            match redact_line(line) {
                Some(redacted) => Cow::Owned(redacted),
                None => Cow::Borrowed(*line),
            }
        })
        .collect()
}

/// Detect a marker this harness added to file content it returned: a clip
/// marker from `read`/`grep`, or a redaction placeholder. Writing such a
/// string back into a file corrupts it, so `write` and `edit` refuse.
///
/// One implementation for both writers — two copies of a detection rule drift
/// apart, and the half that stops being checked is the one that matters.
pub(crate) fn read_marker(text: &str) -> Option<&'static str> {
    if find_clip_marker(text) {
        return Some("truncation");
    }
    if find_redaction_marker(text) {
        return Some("redaction");
    }
    None
}

/// `…[+123 chars]` (read) or `…[+123 bytes]` (grep).
fn find_clip_marker(text: &str) -> bool {
    let open = "\u{2026}[+";
    text.match_indices(open).any(|(at, _)| {
        let rest = &text[at + open.len()..];
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        digits > 0
            && (rest[digits..].starts_with(" chars]") || rest[digits..].starts_with(" bytes]"))
    })
}

/// `[redacted: 51 chars…`, with or without the `starts "…"` tail.
fn find_redaction_marker(text: &str) -> bool {
    let open = "[redacted: ";
    text.match_indices(open).any(|(at, _)| {
        let rest = &text[at + open.len()..];
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        digits > 0 && rest[digits..].starts_with(" chars")
    })
}

/// Shared self-healing message for both writers: name the marker, say it is
/// not file content, and point at the channel that can produce the real bytes.
pub(crate) fn marker_error(kind: &str, field: &str) -> String {
    format!(
        "{field} contains a {kind} marker that `read`/`grep` added to their output — \
         it is not the file's actual content, and writing it would corrupt the file. \
         Get the real text first: `grep` with a narrow pattern for a clipped line, or \
         `shell` for a file whose credentials were redacted."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_common_config_shapes() {
        for line in [
            "api_key = \"sk-ant-api03-abcdefghijklmnop\"",
            "  \"apiKey\": \"sk-ant-api03-abcdefghijklmnop\",",
            "API_KEY=sk-ant-api03-abcdefghijklmnop",
            "password: hunter2hunter2hunter2",
            "auth-token = \"ghp_abcdefghijklmnopqrst\"",
        ] {
            let out = redact_line(line).unwrap_or_else(|| panic!("not redacted: {line}"));
            assert!(out.contains("redacted"), "{out}");
            assert!(!out.contains("hunter2hunter2"), "{out}");
            assert!(!out.contains("abcdefghijklmnop"), "{out}");
        }
    }

    #[test]
    fn placeholder_keeps_length_and_prefix() {
        let out = redact_line("api_key = \"sk-ant-api03-abcdefghijklmnop\"").unwrap();
        assert!(out.starts_with("api_key = \""), "{out}");
        assert!(out.ends_with("\""), "quotes must survive: {out}");
        assert!(out.contains("29 chars"), "{out}");
        assert!(out.contains("starts \"sk-a\""), "{out}");
    }

    /// Every one of these has bitten a naive key-name matcher.
    #[test]
    fn leaves_lookalikes_alone() {
        for line in [
            // last segment is not a secret word
            "max_tokens = 8192",
            "password_min_length = 12",
            // tcode's own pattern: the value names an env var, not a secret
            "api_key_env = \"ANTHROPIC_API_KEY\"",
            // `key` without a qualifying prefix
            "primary_key = \"customer_identifier_col\"",
            "partition_key = \"trade_date_partition\"",
            "cache_key = \"render:v3:abcdefghijklm\"",
            // value shapes that are pointers or prose, not credentials
            "api_key = \"${ANTHROPIC_API_KEY}\"",
            "token = \"$GITHUB_TOKEN\"",
            "secret = \"see the ops runbook for this\"",
            "password = \"short\"",
        ] {
            assert!(redact_line(line).is_none(), "wrongly redacted: {line}");
        }
    }

    #[test]
    fn redacts_every_pair_on_one_line() {
        let out = redact_line(
            "{\"model\":\"opus\",\"api_key\":\"sk-ant-abcdefghijklmnop\",\"token\":\"ghp_abcdefghijklmnop\"}",
        )
        .unwrap();
        assert!(out.contains("\"model\":\"opus\""), "{out}");
        assert_eq!(out.matches("redacted").count(), 2, "{out}");
        assert!(out.ends_with("\"}"), "{out}");
    }

    #[test]
    fn pem_body_is_redacted_without_changing_the_line_count() {
        let lines = vec![
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAA",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "-----END OPENSSH PRIVATE KEY-----",
            "trailing = ok",
        ];
        let out = redact_lines(&lines);
        assert_eq!(out.len(), lines.len());
        assert_eq!(out[0], lines[0], "the BEGIN marker says what this is");
        assert!(out[1].contains("redacted"), "{}", out[1]);
        assert!(out[2].contains("redacted"), "{}", out[2]);
        assert_eq!(out[3], lines[3]);
        assert_eq!(out[4], "trailing = ok");
    }

    #[test]
    fn clean_lines_are_borrowed() {
        let lines = vec!["fn main() {", "    println!(\"hi\");", "}"];
        for line in redact_lines(&lines) {
            assert!(matches!(line, Cow::Borrowed(_)));
        }
    }

    #[test]
    fn markers_are_detected_in_both_flavors() {
        // Assembled rather than written literally: a literal marker in this
        // file would make the file itself un-editable by `edit`.
        let clip = format!("some text\u{2026}[+{} chars]", 900);
        let grep = format!("some text\u{2026}[+{} bytes]", 900);
        let redacted = format!("api_key = \"[redacted: {} chars, starts \"sk-a\"]\"", 51);
        assert_eq!(read_marker(&clip), Some("truncation"));
        assert_eq!(read_marker(&grep), Some("truncation"));
        assert_eq!(read_marker(&redacted), Some("redaction"));
        // Near misses: the shapes without a count are ordinary prose.
        assert_eq!(read_marker("an ellipsis \u{2026} alone"), None);
        assert_eq!(read_marker("[redacted: see policy]"), None);
        assert_eq!(read_marker("let n = a[+1 chars];"), None);
    }

    #[test]
    fn key_segmentation_handles_every_casing() {
        assert_eq!(segments("api_key"), ["api", "key"]);
        assert_eq!(segments("apiKey"), ["api", "key"]);
        assert_eq!(segments("API_KEY"), ["api", "key"]);
        assert_eq!(segments("auth.token"), ["auth", "token"]);
        assert!(is_secret_key("sshKey"));
        assert!(!is_secret_key("api_key_env"));
    }
}
