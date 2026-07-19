//! The declarative half of the filter chain: a filter written as TOML rather
//! than as Rust.
//!
//! Most command noise is line-shaped — progress counters, "Compiling …",
//! banners — and removing it needs a list of regexes, not a program. Keeping
//! those in data means a user can add one without building the harness, and
//! means the built-in set is reviewable as text.
//!
//! Pipeline, in order: `strip_ansi` → `replace` → `match_output` →
//! `strip_lines_matching`/`keep_lines_matching` → `truncate_lines_at` →
//! `tail_lines` → `max_lines` → `on_empty`.

use std::collections::BTreeMap;

use regex::{Regex, RegexSet};
use serde::Deserialize;

use super::OutputFilter;

/// One `filters.toml` file: filter definitions plus their inline tests.
///
/// Tests live in their own table rather than inside the filter so the filter
/// definition can keep `deny_unknown_fields` — a typo in a field name is a
/// loud error instead of a rule that silently never applies.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterFile {
    #[serde(default)]
    pub filters: BTreeMap<String, FilterDef>,
    #[serde(default)]
    pub tests: BTreeMap<String, Vec<FilterTest>>,
}

/// Read only by the test suite, which is the point: an inline test is how a
/// filter's rules are shown to work, and `dead_code` does not count tests.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub struct FilterTest {
    pub name: String,
    pub input: String,
    pub expected: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterDef {
    /// What this filter is for. Not used at runtime; it is what makes the
    /// built-in set readable.
    #[allow(dead_code)]
    pub description: Option<String>,
    /// Matched against the whole command string. Commands are often compound
    /// (`cd sub && cargo build`), so anchoring to the start is usually wrong.
    pub match_command: String,
    /// Skips this filter when it also matches — the `regex` crate has no
    /// lookahead, so "matches A but not B" needs a second pattern.
    #[serde(default)]
    pub exclude_command: Option<String>,
    #[serde(default)]
    pub strip_ansi: bool,
    #[serde(default)]
    pub replace: Vec<ReplaceRule>,
    #[serde(default)]
    pub match_output: Vec<MatchOutputRule>,
    #[serde(default)]
    pub strip_lines_matching: Vec<String>,
    #[serde(default)]
    pub keep_lines_matching: Vec<String>,
    pub truncate_lines_at: Option<usize>,
    pub tail_lines: Option<usize>,
    pub max_lines: Option<usize>,
    pub on_empty: Option<String>,
}

/// A substitution applied line by line. Rules chain: rule N+1 sees rule N's
/// result. Capture references (`$1`) work.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaceRule {
    pub pattern: String,
    pub replacement: String,
}

/// Collapses the whole output to one line when it matches — the "nothing
/// interesting happened" case, where even the surviving lines are not worth
/// their tokens. `unless` is the safety valve: a run that also printed a
/// warning is not the boring case, so the rule steps aside.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchOutputRule {
    pub pattern: String,
    pub message: String,
    #[serde(default)]
    pub unless: Option<String>,
}

#[derive(Debug)]
struct CompiledReplace {
    pattern: Regex,
    replacement: String,
}

#[derive(Debug)]
struct CompiledMatchOutput {
    pattern: Regex,
    message: String,
    unless: Option<Regex>,
}

/// Line selection is one or the other: keeping a whitelist and dropping a
/// blacklist in the same pass has no useful meaning, so the type refuses it.
#[derive(Debug)]
enum LineFilter {
    All,
    Strip(RegexSet),
    Keep(RegexSet),
}

/// A parsed filter with every regex already compiled — compilation failures
/// belong to load time, not to the middle of a tool call.
#[derive(Debug)]
pub struct TomlFilter {
    name: String,
    match_command: Regex,
    exclude_command: Option<Regex>,
    strip_ansi: bool,
    replace: Vec<CompiledReplace>,
    match_output: Vec<CompiledMatchOutput>,
    lines: LineFilter,
    truncate_lines_at: Option<usize>,
    tail_lines: Option<usize>,
    max_lines: Option<usize>,
    on_empty: Option<String>,
}

fn compile(what: &str, name: &str, pattern: &str) -> Result<Regex, String> {
    Regex::new(pattern).map_err(|e| format!("filter '{name}': invalid {what} regex: {e}"))
}

impl TomlFilter {
    pub fn compile(name: String, def: FilterDef) -> Result<Self, String> {
        if !def.strip_lines_matching.is_empty() && !def.keep_lines_matching.is_empty() {
            return Err(format!(
                "filter '{name}': strip_lines_matching and keep_lines_matching are mutually exclusive"
            ));
        }
        let set = |patterns: &[String]| {
            RegexSet::new(patterns).map_err(|e| format!("filter '{name}': invalid line regex: {e}"))
        };
        let lines = if !def.strip_lines_matching.is_empty() {
            LineFilter::Strip(set(&def.strip_lines_matching)?)
        } else if !def.keep_lines_matching.is_empty() {
            LineFilter::Keep(set(&def.keep_lines_matching)?)
        } else {
            LineFilter::All
        };
        let match_command = compile("match_command", &name, &def.match_command)?;
        let exclude_command = def
            .exclude_command
            .as_deref()
            .map(|p| compile("exclude_command", &name, p))
            .transpose()?;
        let replace = def
            .replace
            .into_iter()
            .map(|rule| {
                Ok(CompiledReplace {
                    pattern: compile("replace", &name, &rule.pattern)?,
                    replacement: rule.replacement,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let match_output = def
            .match_output
            .into_iter()
            .map(|rule| {
                Ok(CompiledMatchOutput {
                    pattern: compile("match_output", &name, &rule.pattern)?,
                    message: rule.message,
                    unless: rule
                        .unless
                        .as_deref()
                        .map(|p| compile("unless", &name, p))
                        .transpose()?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self {
            name,
            match_command,
            exclude_command,
            strip_ansi: def.strip_ansi,
            replace,
            match_output,
            lines,
            truncate_lines_at: def.truncate_lines_at,
            tail_lines: def.tail_lines,
            max_lines: def.max_lines,
            on_empty: def.on_empty,
        })
    }

    /// Run the pipeline without consulting `match_command`. Inline tests use
    /// this so a test case states what the rules do, not what command was run.
    pub fn transform(&self, output: &str) -> String {
        let mut lines: Vec<String> = output.lines().map(str::to_string).collect();

        if self.strip_ansi {
            lines = lines.iter().map(|line| strip_ansi(line)).collect();
        }

        for rule in &self.replace {
            for line in &mut lines {
                *line = rule
                    .pattern
                    .replace_all(line, rule.replacement.as_str())
                    .into_owned();
            }
        }

        if !self.match_output.is_empty() {
            let blob = lines.join("\n");
            for rule in &self.match_output {
                if !rule.pattern.is_match(&blob) {
                    continue;
                }
                if rule.unless.as_ref().is_some_and(|re| re.is_match(&blob)) {
                    continue;
                }
                return rule.message.clone();
            }
        }

        match &self.lines {
            LineFilter::Strip(set) => lines.retain(|line| !set.is_match(line)),
            LineFilter::Keep(set) => lines.retain(|line| set.is_match(line)),
            LineFilter::All => {}
        }

        if let Some(width) = self.truncate_lines_at {
            for line in &mut lines {
                if line.chars().count() > width {
                    *line = line.chars().take(width).collect::<String>() + "…";
                }
            }
        }

        // Cuts announce themselves. The model must be able to tell a short
        // output from a shortened one; the harness's pointer line says where
        // the rest is, but only this says that there is a rest.
        if let Some(tail) = self.tail_lines {
            if lines.len() > tail {
                let omitted = lines.len() - tail;
                lines = lines.split_off(omitted);
                lines.insert(0, format!("… ({omitted} lines omitted)"));
            }
        }
        if let Some(max) = self.max_lines {
            if lines.len() > max {
                let omitted = lines.len() - max;
                lines.truncate(max);
                lines.push(format!("… ({omitted} lines omitted)"));
            }
        }

        let result = lines.join("\n");
        match &self.on_empty {
            Some(message) if result.trim().is_empty() => message.clone(),
            _ => result,
        }
    }
}

impl OutputFilter for TomlFilter {
    fn name(&self) -> &str {
        &self.name
    }

    fn apply(&self, command: &str, output: &str) -> Option<String> {
        if !self.match_command.is_match(command) {
            return None;
        }
        if self
            .exclude_command
            .as_ref()
            .is_some_and(|re| re.is_match(command))
        {
            return None;
        }
        Some(self.transform(output))
    }
}

/// Drop ANSI CSI/OSC escapes. Colored build output is common and the codes
/// are pure cost in a transcript.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters, then a final byte in @..~
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: runs until BEL or ST (ESC \).
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // Any other two-byte escape: drop both bytes.
            Some(_) | None => {}
        }
    }
    out
}

/// Parse one file's worth of TOML into compiled filters, paired with their
/// inline tests. Every filter is checked, so one broken rule names itself
/// instead of taking the file down anonymously.
pub fn parse_file(source: &str) -> Result<Vec<(TomlFilter, Vec<FilterTest>)>, String> {
    let file: FilterFile = toml::from_str(source).map_err(|e| e.to_string())?;
    let mut tests = file.tests;
    let mut out = Vec::with_capacity(file.filters.len());
    for (name, def) in file.filters {
        let cases = tests.remove(&name).unwrap_or_default();
        out.push((TomlFilter::compile(name, def)?, cases));
    }
    if let Some(orphan) = tests.keys().next() {
        return Err(format!("tests for '{orphan}' name no such filter"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(toml: &str) -> TomlFilter {
        parse_file(toml)
            .expect("parses")
            .pop()
            .expect("one filter")
            .0
    }

    #[test]
    fn unknown_fields_are_rejected_rather_than_ignored() {
        let error = parse_file(
            r#"
[filters.demo]
match_command = "demo"
strip_lines_maching = ["typo"]
"#,
        )
        .unwrap_err();
        assert!(error.contains("strip_lines_maching"), "{error}");
    }

    #[test]
    fn strip_and_keep_cannot_both_be_set() {
        let error = parse_file(
            r#"
[filters.demo]
match_command = "demo"
strip_lines_matching = ["a"]
keep_lines_matching = ["b"]
"#,
        )
        .unwrap_err();
        assert!(error.contains("mutually exclusive"), "{error}");
    }

    #[test]
    fn exclude_command_stands_in_for_a_lookahead() {
        let f = filter(
            r#"
[filters.demo]
match_command = "\\bdemo\\b"
exclude_command = "--verbose"
strip_lines_matching = ["^noise"]
"#,
        );
        assert_eq!(f.apply("demo run", "noise\nkeep"), Some("keep".into()));
        assert_eq!(f.apply("demo run --verbose", "noise\nkeep"), None);
    }

    #[test]
    fn match_output_short_circuits_but_yields_to_unless() {
        let f = filter(
            r#"
[filters.demo]
match_command = "demo"
match_output = [{ pattern = "up to date", message = "ok (up to date)", unless = "warning:" }]
"#,
        );
        assert_eq!(
            f.apply("demo", "checking\nup to date"),
            Some("ok (up to date)".into())
        );
        assert_eq!(
            f.apply("demo", "checking\nup to date\nwarning: stale lockfile"),
            Some("checking\nup to date\nwarning: stale lockfile".into())
        );
    }

    #[test]
    fn cuts_are_announced_so_a_short_output_is_not_mistaken_for_a_whole_one() {
        let f = filter(
            r#"
[filters.demo]
match_command = "demo"
max_lines = 2
"#,
        );
        assert_eq!(
            f.apply("demo", "a\nb\nc\nd"),
            Some("a\nb\n… (2 lines omitted)".into())
        );
    }

    #[test]
    fn ansi_escapes_are_removed_including_osc_sequences() {
        let f = filter(
            r#"
[filters.demo]
match_command = "demo"
strip_ansi = true
"#,
        );
        assert_eq!(
            f.apply("demo", "\u{1b}[32mgreen\u{1b}[0m\n\u{1b}]0;title\u{7}plain"),
            Some("green\nplain".into())
        );
    }

    #[test]
    fn on_empty_replaces_an_output_that_filtered_down_to_nothing() {
        let f = filter(
            r#"
[filters.demo]
match_command = "demo"
strip_lines_matching = ["."]
on_empty = "ok"
"#,
        );
        assert_eq!(f.apply("demo", "all noise"), Some("ok".into()));
    }

    #[test]
    fn tests_naming_no_filter_are_an_error_not_silently_skipped() {
        let error = parse_file(
            r#"
[filters.demo]
match_command = "demo"

[[tests.dmeo]]
name = "typo"
input = "a"
expected = "a"
"#,
        )
        .unwrap_err();
        assert!(error.contains("dmeo"), "{error}");
    }
}
