//! Minimal YAML front matter shared by skills and agent definitions.

use std::collections::HashMap;

use serde_yaml::Mapping;

/// Top-level `key: value` pairs between `---` fences, including block
/// scalars (`description: |` with indented lines) — the layout Claude Code
/// skills use in practice. Everything else is ignored.
pub(crate) fn front_matter(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut lines = text.lines().peekable();
    if lines.next().map(str::trim) != Some("---") {
        return out;
    }
    while let Some(line) = lines.next() {
        if line.trim() == "---" {
            break;
        }
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        let value = if matches!(value, "|" | ">" | "|-" | ">-" | "|+" | ">+") {
            // Block scalar: fold the following indented lines into one line;
            // the listing shows a single line per skill anyway.
            let mut parts: Vec<String> = Vec::new();
            while let Some(next) = lines.peek() {
                if next.trim().is_empty() || next.starts_with([' ', '\t']) {
                    if !next.trim().is_empty() {
                        parts.push(next.trim().to_owned());
                    }
                    lines.next();
                } else {
                    break;
                }
            }
            parts.join(" ")
        } else {
            value.trim_matches(['"', '\'']).to_owned()
        };
        out.insert(key.trim().to_owned(), value);
    }
    out
}

/// Parse the fenced front matter as real YAML. Agent definitions use this
/// rather than the legacy string-only reader above so Claude Code-compatible
/// list syntax (`tools: [read, grep]`) has its ordinary YAML meaning.
pub(crate) fn yaml_front_matter(text: &str) -> Result<Mapping, String> {
    let Some(rest) = text.strip_prefix("---") else {
        return Ok(Mapping::new());
    };
    let Some((yaml, _)) = rest.split_once("\n---") else {
        return Err("front matter starts with `---` but has no closing fence".into());
    };
    serde_yaml::from_str(yaml).map_err(|error| format!("invalid YAML front matter: {error}"))
}

pub(crate) fn strip_front_matter(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---") else {
        return text;
    };
    rest.split_once("\n---")
        .map(|(_, body)| body.trim_start_matches('-').trim_start())
        .unwrap_or(text)
}

pub(crate) fn clip(text: &str, cap: usize) -> String {
    let text = text.lines().next().unwrap_or("").trim();
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(cap).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}
