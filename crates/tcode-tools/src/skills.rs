//! Skills: reusable instruction packages the model loads on demand.
//!
//! Discovery is filesystem-only (`<dir>/skills/*/SKILL.md`); the always-paid
//! cost is one capped line per skill inside the tool description, which lives
//! in the cached prompt prefix. The full SKILL.md body costs tokens only when
//! the model actually invokes the skill.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::{PermissionRequest, PromptVariables, Tool, ToolCtx, ToolOutput};

/// One listed line per skill; longer descriptions are clipped, not dropped.
const DESCRIPTION_CAP: usize = 200;
/// Budget for the whole listing inside the tool description (~1.5k tokens).
/// Skills beyond it stay invocable but appear as names only.
const LISTING_CAP: usize = 6_000;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Directory containing SKILL.md, for resolving the skill's own files.
    pub dir: PathBuf,
}

pub struct SkillTool {
    skills: Vec<Skill>,
    description: String,
}

impl SkillTool {
    /// None when no skills exist — the tool then costs zero prompt tokens.
    pub fn discover(cwd: &Path) -> Option<Self> {
        let skills = discover_skills(cwd);
        (!skills.is_empty()).then(|| {
            let description = tool_description(&skills);
            Self {
                skills,
                description,
            }
        })
    }

    #[cfg(test)]
    fn from_skills(skills: Vec<Skill>) -> Self {
        let description = tool_description(&skills);
        Self {
            skills,
            description,
        }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Skill name from the list" },
                "arguments": { "type": "string", "description": "Optional raw arguments for $ARGUMENTS, $0, $1, and $ARGUMENTS[N] substitutions" }
            },
            "required": ["name"]
        })
    }

    fn permission(&self, _: &Value) -> PermissionRequest {
        PermissionRequest::None
    }

    async fn run(&self, input: Value, ctx: &ToolCtx, _: &CancellationToken) -> ToolOutput {
        let name = input["name"].as_str().unwrap_or("");
        let arguments = input["arguments"].as_str().unwrap_or("");
        let Some(skill) = self.skills.iter().find(|skill| skill.name == name) else {
            // Self-healing error: the model fixes the call without a
            // discovery turn.
            return ToolOutput::err(format!(
                "unknown skill '{name}'. Available: {}",
                self.skills
                    .iter()
                    .map(|skill| skill.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        };
        // Read at call time: skill files may change while tcode runs.
        match tokio::fs::read_to_string(skill.dir.join("SKILL.md")).await {
            Ok(body) => {
                let rendered = PromptVariables::new(&ctx.cwd, &ctx.scratch_dir)
                    .with_skill(&skill.dir, arguments)
                    .expand(strip_front_matter(&body));
                ToolOutput::ok(format!(
                    "# Skill: {} (files in {})\n\n{rendered}",
                    skill.name,
                    skill.dir.display(),
                ))
            }
            Err(e) => ToolOutput::err(format!(
                "cannot read {}: {e}",
                skill.dir.join("SKILL.md").display()
            )),
        }
    }
}

/// Project skills override personal ones of the same name; `.tcode` wins
/// over the `.claude` compatibility locations.
fn discover_skills(cwd: &Path) -> Vec<Skill> {
    let mut roots = vec![cwd.join(".tcode/skills"), cwd.join(".claude/skills")];
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".tcode/skills"));
        roots.push(home.join(".claude/skills"));
    }
    let mut skills: Vec<Skill> = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.join("SKILL.md").is_file())
            .collect();
        dirs.sort();
        for dir in dirs {
            let Ok(text) = std::fs::read_to_string(dir.join("SKILL.md")) else {
                continue;
            };
            let meta = front_matter(&text);
            let name = meta
                .get("name")
                .cloned()
                .or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default();
            if name.is_empty() || skills.iter().any(|skill| skill.name == name) {
                continue;
            }
            skills.push(Skill {
                name,
                description: meta.get("description").cloned().unwrap_or_default(),
                dir,
            });
        }
    }
    skills
}

fn tool_description(skills: &[Skill]) -> String {
    let mut out = String::from(
        "Load a skill: packaged instructions for a specific kind of task. When the \
         user's request matches a skill below, call this tool FIRST and follow the \
         returned instructions instead of improvising.\n\nAvailable skills:\n",
    );
    let mut overflow: Vec<&str> = Vec::new();
    for skill in skills {
        if overflow.is_empty() && out.len() < LISTING_CAP {
            let description = clip(&skill.description, DESCRIPTION_CAP);
            out.push_str(&format!("- {}: {description}\n", skill.name));
        } else {
            overflow.push(&skill.name);
        }
    }
    if !overflow.is_empty() {
        out.push_str(&format!(
            "\nAlso available (names only): {}\n",
            overflow.join(", ")
        ));
    }
    out
}

/// Minimal YAML front matter: top-level `key: value` pairs between `---`
/// fences, including block scalars (`description: |` with indented lines) —
/// the layout Claude Code skills use in practice. Everything else is ignored.
fn front_matter(text: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
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

fn strip_front_matter(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---") else {
        return text;
    };
    rest.split_once("\n---")
        .map(|(_, body)| body.trim_start_matches('-').trim_start())
        .unwrap_or(text)
}

fn clip(text: &str, cap: usize) -> String {
    let text = text.lines().next().unwrap_or("").trim();
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(cap).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, description: &str) -> Skill {
        Skill {
            name: name.into(),
            description: description.into(),
            dir: PathBuf::from("unused"),
        }
    }

    #[test]
    fn front_matter_extracts_name_and_description() {
        let meta = front_matter(
            "---\nname: pybond\ndescription: \"债券定价\"\nallowed-tools: [x]\n---\nbody",
        );
        assert_eq!(meta.get("name").map(String::as_str), Some("pybond"));
        assert_eq!(
            meta.get("description").map(String::as_str),
            Some("债券定价")
        );
    }

    #[test]
    fn front_matter_folds_block_scalar_descriptions() {
        let meta = front_matter(
            "---\nname: ddb\ndescription: |\n  第一行触发条件,\n  第二行更多细节.\n---\nbody",
        );
        assert_eq!(
            meta.get("description").map(String::as_str),
            Some("第一行触发条件, 第二行更多细节.")
        );
    }

    #[test]
    fn listing_caps_long_descriptions_and_overflows_to_names_only() {
        let mut skills = vec![skill("verbose", &"x".repeat(500))];
        for i in 0..80 {
            skills.push(skill(&format!("s{i}"), &"d".repeat(150)));
        }
        let description = tool_description(&skills);
        assert!(description.len() < LISTING_CAP + 2_000);
        assert!(description.contains("…"));
        assert!(description.contains("names only"));
        // Every skill remains discoverable by name.
        assert!(description.contains("s79"));
    }

    #[tokio::test]
    async fn unknown_name_error_lists_valid_names() {
        let tool = SkillTool::from_skills(vec![skill("alpha", "a"), skill("beta", "b")]);
        let ctx = ToolCtx::new(std::env::temp_dir(), 4_000);
        let out = tool
            .run(json!({"name": "gamma"}), &ctx, &CancellationToken::new())
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("alpha, beta"));
    }

    #[test]
    fn project_skills_are_discovered_and_listed_first() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".tcode/skills/deploy");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: deploy\ndescription: ship it\n---\nsteps",
        )
        .unwrap();
        let skills = discover_skills(tmp.path());
        assert_eq!(skills[0].name, "deploy");
        assert_eq!(skills[0].description, "ship it");
    }

    #[tokio::test]
    async fn loaded_skill_expands_runtime_and_argument_placeholders_once() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("inspect");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: inspect\n---\n${CLAUDE_SKILL_DIR}|${TCODE_PROJECT_DIR}|${TCODE_SCRATCH_DIR}|${CLAUDE_SESSION_ID}|$0|$1|$ARGUMENTS[2]|$ARGUMENTS",
        )
        .unwrap();
        let tool = SkillTool::from_skills(vec![Skill {
            name: "inspect".into(),
            description: String::new(),
            dir: dir.clone(),
        }]);
        let project = tmp.path().join("project");
        let scratch = tmp.path().join("scratch/runs/session-a");
        let ctx = ToolCtx::with_scratch_dir(project.clone(), 4_000, scratch.clone());

        let out = tool
            .run(
                json!({"name": "inspect", "arguments": "first 'two words' ${TCODE_SCRATCH_DIR}"}),
                &ctx,
                &CancellationToken::new(),
            )
            .await;

        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains(&dir.display().to_string()));
        assert!(out.content.contains(&project.display().to_string()));
        assert!(out.content.contains(&scratch.display().to_string()));
        assert!(out.content.contains(
            "session-a|first|two words|${TCODE_SCRATCH_DIR}|first 'two words' ${TCODE_SCRATCH_DIR}"
        ));
    }

    #[test]
    fn body_is_returned_without_front_matter() {
        assert_eq!(
            strip_front_matter("---\nname: x\n---\n\nDo the thing."),
            "Do the thing."
        );
    }
}
