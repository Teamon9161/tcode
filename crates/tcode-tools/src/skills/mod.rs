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

use crate::frontmatter::{clip, front_matter, strip_front_matter};
use tcode_core::{PermissionRequest, PromptVariables, Tool, ToolCtx, ToolOutput, SKILL_ECHO_OPEN};

/// One listed line per skill; longer descriptions are clipped, not dropped.
const DESCRIPTION_CAP: usize = 200;
/// Budget for the whole listing inside the tool description (~1.5k tokens).
/// Skills beyond it stay invocable but appear as names only.
const LISTING_CAP: usize = 6_000;

// Rust cannot expand `include_str!` over a directory. The build script scans
// `src/skills/builtin/*/SKILL.md` and emits this resource manifest, so adding a
// builtin skill never requires a Rust registration edit.
include!(concat!(env!("OUT_DIR"), "/builtin_skills.rs"));

/// Where a skill's SKILL.md body comes from. `Builtin` skills ship inside the
/// binary (`include_str!`), so upgrading tcode upgrades them too; a
/// filesystem skill of the same name overrides one, matching Claude Code.
#[derive(Debug, Clone)]
pub enum SkillSource {
    Dir(PathBuf),
    Builtin(&'static str),
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub source: SkillSource,
}

/// Skills baked into the binary. Not materialized to disk: a user override
/// lives at `.tcode/skills/<name>/SKILL.md` (or `~/.tcode/skills/<name>/`),
/// found first by `discover_skills` and so preferred by its first-wins dedup.
fn builtin_skills() -> Vec<Skill> {
    BUILTIN_SKILL_FILES
        .iter()
        .map(|(_, fallback_name, body)| {
            let meta = front_matter(body);
            Skill {
                name: meta
                    .get("name")
                    .cloned()
                    .unwrap_or_else(|| (*fallback_name).into()),
                description: meta.get("description").cloned().unwrap_or_default(),
                source: SkillSource::Builtin(body),
            }
        })
        .collect()
}

pub struct SkillTool {
    skills: Vec<Skill>,
    description: String,
}

impl SkillTool {
    pub fn new(skills: Vec<Skill>) -> Option<Self> {
        (!skills.is_empty()).then(|| {
            let description = tool_description(&skills);
            Self {
                skills,
                description,
            }
        })
    }

    /// Builtin skills mean this is never `None` in practice; kept as `Option`
    /// so a future all-filesystem-skills-removed state degrades safely.
    pub fn discover(cwd: &Path) -> Option<Self> {
        Self::new(discover_skills(cwd))
    }

    #[cfg(test)]
    fn from_skills(skills: Vec<Skill>) -> Self {
        Self::new(skills).expect("non-empty in tests")
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
        match &skill.source {
            // Read at call time: skill files may change while tcode runs.
            SkillSource::Dir(dir) => match tokio::fs::read_to_string(dir.join("SKILL.md")).await {
                Ok(body) => {
                    let rendered =
                        render_skill(skill, &body, arguments, &ctx.cwd, &ctx.scratch_dir);
                    ToolOutput::ok(format!(
                        "# Skill: {} (files in {})\n\n{rendered}",
                        skill.name,
                        dir.display(),
                    ))
                }
                Err(e) => ToolOutput::err(format!(
                    "cannot read {}: {e}",
                    dir.join("SKILL.md").display()
                )),
            },
            SkillSource::Builtin(body) => {
                let rendered = render_skill(skill, body, arguments, &ctx.cwd, &ctx.scratch_dir);
                ToolOutput::ok(format!("# Skill: {}\n\n{rendered}", skill.name))
            }
        }
    }
}

/// Pure rendering shared by the `skill` tool call and the user-triggered
/// `/name` fallback (TUI and plain REPL): front-matter strip + variable
/// expansion. Each caller reads the body its own way (async `tokio::fs` from
/// the tool's async `run`, sync `std::fs` from the frontends' non-async
/// command dispatch) so this stays IO-free and cannot drift between the two
/// paths.
pub fn render_skill(
    skill: &Skill,
    body: &str,
    arguments: &str,
    cwd: &Path,
    scratch_dir: &Path,
) -> String {
    let vars = match &skill.source {
        SkillSource::Dir(dir) => PromptVariables::new(cwd, scratch_dir).with_skill(dir, arguments),
        SkillSource::Builtin(_) => PromptVariables::new(cwd, scratch_dir).with_arguments(arguments),
    };
    vars.expand(strip_front_matter(body))
}

/// Wraps a rendered skill body for injection as a normal `Entry::User`
/// prompt (the user-triggered `/name` path, not the `skill` tool call): pure
/// append, one turn cheaper than making the model call the tool itself. The
/// sentinel lets a transcript recognize and fold this back down from the
/// ledger text alone — live and replay both call `parse_skill_echo` on it, so
/// there is exactly one place that knows the format. The opening marker is
/// `tcode_core::SKILL_ECHO_OPEN` because core also has to recognize it: the
/// body is a repository file wearing a user message's clothes, and Auto Mode's
/// authorization check must not read it as the user speaking.
pub fn wrap_skill_echo(name: &str, args: &str, body: &str) -> String {
    format!(
        "{SKILL_ECHO_OPEN}name=\"{}\" args=\"{}\">\n{body}\n</user-skill>",
        escape_attr(name),
        escape_attr(args),
    )
}

/// The fields a transcript needs to fold a `wrap_skill_echo` block back down
/// to a `/name args` line + a collapsed summary, without re-parsing the body.
pub struct SkillEcho {
    pub name: String,
    pub args: String,
    pub body_line_count: usize,
}

pub fn parse_skill_echo(text: &str) -> Option<SkillEcho> {
    let rest = text.strip_prefix(SKILL_ECHO_OPEN)?;
    let (tag, after_tag) = rest.split_once('>')?;
    let name = attr(tag, "name")?;
    let args = attr(tag, "args").unwrap_or_default();
    let body = after_tag.strip_prefix('\n').unwrap_or(after_tag);
    let body = body
        .strip_suffix("\n</user-skill>")
        .or_else(|| body.strip_suffix("</user-skill>"))
        .unwrap_or(body);
    Some(SkillEcho {
        name,
        args,
        body_line_count: body.lines().count(),
    })
}

fn attr(tag: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')?;
    Some(unescape_attr(&tag[start..start + end]))
}

fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;")
}

fn unescape_attr(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&amp;", "&")
}

/// Project skills override personal ones of the same name; `.tcode` wins
/// over the `.claude` compatibility locations; any of those override a
/// builtin skill of the same name (checked last, so first-wins dedup keeps
/// the filesystem version).
pub fn discover_skills(cwd: &Path) -> Vec<Skill> {
    let mut roots = vec![cwd.join(".tcode/skills"), cwd.join(".claude/skills")];
    if let Some(home) = tcode_core::home_dir() {
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
                source: SkillSource::Dir(dir),
            });
        }
    }
    for skill in builtin_skills() {
        if !skills.iter().any(|existing| existing.name == skill.name) {
            skills.push(skill);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, description: &str) -> Skill {
        Skill {
            name: name.into(),
            description: description.into(),
            source: SkillSource::Dir(PathBuf::from("unused")),
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
        let ctx = ToolCtx::for_test(std::env::temp_dir(), 4_000);
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
            source: SkillSource::Dir(dir.clone()),
        }]);
        let project = tmp.path().join("project");
        let scratch = tmp.path().join("scratch/runs/session-a");
        tcode_core::home::testing::temp_home();
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

    #[test]
    fn generated_manifest_entries_become_builtin_skills() {
        let builtins = builtin_skills();
        assert_eq!(builtins.len(), BUILTIN_SKILL_FILES.len());

        for ((_, fallback_name, body), skill) in BUILTIN_SKILL_FILES.iter().zip(&builtins) {
            let meta = front_matter(body);
            let expected_name = meta
                .get("name")
                .map(String::as_str)
                .unwrap_or(fallback_name);
            assert_eq!(skill.name, expected_name);
            assert_eq!(
                skill.description,
                meta.get("description")
                    .map(String::as_str)
                    .unwrap_or_default()
            );
            assert!(matches!(skill.source, SkillSource::Builtin(source) if source == *body));
        }
    }

    #[test]
    fn builtin_configuration_skill_is_discoverable() {
        let skill = builtin_skills()
            .into_iter()
            .find(|skill| skill.name == "tcode-config")
            .expect("configuration skill is embedded");
        assert_eq!(
            skill.description,
            "Configure tcode profiles, models, sub-agents, permissions, limits, MCP servers, and skills"
        );
        let SkillSource::Builtin(body) = skill.source else {
            panic!("configuration skill must be builtin");
        };
        assert!(body.contains("## Profiles and models"));
        assert!(body.contains("## Watchdog and retries"));
        assert!(body.contains("## Custom agent definitions"));
        assert!(body.contains("## Auto Mode policy"));
        assert!(body.contains("## Hooks"));
    }

    #[test]
    fn filesystem_skill_overrides_a_generated_builtin() {
        let builtin = builtin_skills()
            .into_iter()
            .next()
            .expect("build script requires at least one builtin skill");
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".tcode/skills/override");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: {}\ndescription: custom override\n---\ncustom steps",
                builtin.name
            ),
        )
        .unwrap();
        let skills = discover_skills(tmp.path());
        let matches: Vec<&Skill> = skills
            .iter()
            .filter(|skill| skill.name == builtin.name)
            .collect();
        assert_eq!(matches.len(), 1, "override must not duplicate the builtin");
        assert_eq!(matches[0].description, "custom override");
        assert!(matches!(matches[0].source, SkillSource::Dir(_)));
    }

    #[test]
    fn render_skill_expands_builtin_without_a_skill_dir() {
        let skill = Skill {
            name: "probe".into(),
            description: String::new(),
            source: SkillSource::Builtin(""),
        };
        let rendered = render_skill(
            &skill,
            "---\nname: probe\n---\narg=$0 dir=${CLAUDE_SKILL_DIR} proj=${TCODE_PROJECT_DIR}",
            "one two",
            Path::new("/repo"),
            Path::new("/scratch"),
        );
        // No skill directory to substitute: the placeholder stays literal
        // rather than resolving to something misleading.
        assert_eq!(rendered, "arg=one dir=${CLAUDE_SKILL_DIR} proj=/repo");
    }

    #[test]
    fn wrap_and_parse_skill_echo_round_trips_through_special_characters() {
        let wrapped = wrap_skill_echo("init", "say \"hi\" & bye", "line one\nline two\nline three");
        let echo = parse_skill_echo(&wrapped).expect("sentinel recognized");
        assert_eq!(echo.name, "init");
        assert_eq!(echo.args, "say \"hi\" & bye");
        assert_eq!(echo.body_line_count, 3);
    }

    #[test]
    fn ordinary_user_text_is_not_mistaken_for_a_skill_echo() {
        assert!(parse_skill_echo("just a normal message").is_none());
    }
}
