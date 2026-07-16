//! One-pass expansion of trusted runtime placeholders in harness prompts.
//!
//! The context is captured before a session sends its first request. Expanding
//! values at injection time rather than asking the model to infer them keeps
//! paths usable while preserving a byte-stable cached prefix.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptVariables {
    project_dir: String,
    scratch_dir: String,
    session_id: String,
    skill_dir: Option<String>,
    arguments: Option<String>,
}

impl PromptVariables {
    pub fn new(project_dir: &Path, scratch_dir: &Path) -> Self {
        let session_id = scratch_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self {
            project_dir: project_dir.display().to_string(),
            scratch_dir: scratch_dir.display().to_string(),
            session_id,
            skill_dir: None,
            arguments: None,
        }
    }

    pub fn with_skill(mut self, skill_dir: &Path, arguments: impl Into<String>) -> Self {
        self.skill_dir = Some(skill_dir.display().to_string());
        self.arguments = Some(arguments.into());
        self
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Expands only documented placeholders and never re-scans replacement
    /// values. A literal placeholder in an argument therefore stays literal.
    pub fn expand(&self, template: &str) -> String {
        let arguments = self.arguments.as_deref().unwrap_or_default();
        let indexed_arguments = shell_words(arguments);
        let mut out = String::with_capacity(template.len());
        let mut cursor = 0;

        while cursor < template.len() {
            let rest = &template[cursor..];
            let Some(dollar) = rest.find('$') else {
                out.push_str(rest);
                break;
            };
            let at = cursor + dollar;
            out.push_str(&template[cursor..at]);

            if let Some(after_open) = template[at..].strip_prefix("${") {
                if let Some(close) = after_open.find('}') {
                    let name = &after_open[..close];
                    if let Some(value) = self.braced(name) {
                        out.push_str(value);
                        cursor = at + 2 + close + 1;
                        continue;
                    }
                }
            }

            if let Some(after) = template[at..].strip_prefix("$ARGUMENTS") {
                if let Some(after_open) = after.strip_prefix('[') {
                    if let Some(close) = after_open.find(']') {
                        if let Ok(index) = after_open[..close].parse::<usize>() {
                            out.push_str(
                                indexed_arguments
                                    .get(index)
                                    .map(String::as_str)
                                    .unwrap_or(""),
                            );
                            cursor = at + "$ARGUMENTS".len() + 1 + close + 1;
                            continue;
                        }
                    }
                }
                out.push_str(arguments);
                cursor = at + "$ARGUMENTS".len();
                continue;
            }

            let after = &template[at + 1..];
            let digits = after.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 0 {
                let index = after[..digits]
                    .parse::<usize>()
                    .expect("digits parsed as usize");
                out.push_str(
                    indexed_arguments
                        .get(index)
                        .map(String::as_str)
                        .unwrap_or(""),
                );
                cursor = at + 1 + digits;
                continue;
            }

            out.push('$');
            cursor = at + 1;
        }
        out
    }

    fn braced(&self, name: &str) -> Option<&str> {
        match name {
            "TCODE_PROJECT_DIR" | "CLAUDE_PROJECT_DIR" => Some(&self.project_dir),
            "TCODE_SCRATCH_DIR" => Some(&self.scratch_dir),
            "TCODE_SESSION_ID" | "CLAUDE_SESSION_ID" => Some(&self.session_id),
            "TCODE_SKILL_DIR" | "CLAUDE_SKILL_DIR" => self.skill_dir.as_deref(),
            _ => None,
        }
    }
}

/// Enough shell-style splitting for Skill's positional substitutions. Unclosed
/// quotes are treated as the rest of the argument rather than causing a second
/// tool round-trip solely for template parsing.
fn shell_words(input: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;

    for ch in input.chars() {
        if escaped {
            word.push(ch);
            escaped = false;
        } else if ch == '\\' && quote != Some('\'') {
            escaped = true;
        } else if matches!(ch, '\'' | '"') {
            match quote {
                Some(current) if current == ch => quote = None,
                None => quote = Some(ch),
                _ => word.push(ch),
            }
        } else if quote.is_none() && ch.is_whitespace() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(ch);
        }
    }
    if escaped {
        word.push('\\');
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::PromptVariables;
    use std::path::Path;

    fn variables() -> PromptVariables {
        PromptVariables::new(Path::new("/repo"), Path::new("/scratch/runs/s1")).with_skill(
            Path::new("/skills/inspect"),
            "first 'two words' ${TCODE_SCRATCH_DIR}",
        )
    }

    #[test]
    fn expands_runtime_and_claude_compatible_skill_variables_once() {
        let rendered = variables().expand(
            "${TCODE_PROJECT_DIR}|${TCODE_SCRATCH_DIR}|${CLAUDE_SESSION_ID}|${CLAUDE_SKILL_DIR}|$0|$1|$ARGUMENTS[2]|$ARGUMENTS",
        );
        assert_eq!(
            rendered,
            "/repo|/scratch/runs/s1|s1|/skills/inspect|first|two words|${TCODE_SCRATCH_DIR}|first 'two words' ${TCODE_SCRATCH_DIR}"
        );
    }

    #[test]
    fn preserves_unknown_or_literal_dollar_sequences() {
        assert_eq!(
            variables().expand("${UNKNOWN} $HOME $1.00"),
            "${UNKNOWN} $HOME two words.00"
        );
    }
}
