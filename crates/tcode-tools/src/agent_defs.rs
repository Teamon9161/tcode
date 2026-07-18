//! Sub-agent definitions: the builtin task kinds and user-authored
//! `.tcode/agents/*.md` personas live in one registry, so a custom agent is
//! never a special case anywhere the `task` tool dispatches.
//!
//! A definition file is YAML front matter + a markdown body; the body is the
//! sub-agent's system prompt. Discovery mirrors skills: filesystem-only,
//! first-wins across `.tcode/agents` → `.claude/agents` → the same pair under
//! the home directory. The always-paid cost is one capped listing line per
//! custom agent inside the `task` tool description.

use std::path::{Path, PathBuf};

use tcode_core::Tool;

use crate::frontmatter::{clip, front_matter, strip_front_matter};

/// One listed line per custom agent; longer descriptions are clipped.
const DESCRIPTION_CAP: usize = 200;
/// Budget for the whole custom-agent listing inside the tool description.
const LISTING_CAP: usize = 2_000;
/// Nesting bound: an agent at this depth no longer receives a `task` tool,
/// so definition cycles terminate without any graph analysis.
pub const MAX_TASK_DEPTH: usize = 3;

const EXPLORE_SYSTEM: &str = include_str!("../../../prompts/task-explore-system.md");
const PLAN_SYSTEM: &str = include_str!("../../../prompts/task-plan-system.md");
const GENERAL_SYSTEM: &str = include_str!("../../../prompts/task-general-system.md");

#[derive(Debug, Clone)]
pub enum AgentSource {
    Builtin,
    File(PathBuf),
}

/// Raw model pin from front matter. main.rs folds it into `config.agents` as
/// a default, so hand-written config and `/agents` picks always win.
#[derive(Debug, Clone, Default)]
pub struct AgentModelHint {
    pub profile: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl AgentModelHint {
    pub fn is_empty(&self) -> bool {
        self.profile.is_none() && self.model.is_none() && self.effort.is_none()
    }
}

#[derive(Debug, Clone)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// The sub-agent's system prompt (markdown body of the definition file).
    pub system: String,
    /// Read-only agents lose every mutating tool and never prompt.
    pub read_only: bool,
    /// Explicit tool allowlist; `None` keeps the default set for the tier.
    pub tools: Option<Vec<String>>,
    /// Internal denials (builtin `plan` carries `exit_plan`); front matter
    /// cannot set this — an allowlist already expresses any user intent.
    pub deny_tools: Vec<String>,
    /// Agent kinds this one may spawn; empty = no `task` tool (a leaf).
    pub agents: Vec<String>,
    pub model: Option<AgentModelHint>,
    pub max_steps: Option<usize>,
    /// Follow-up turns a caller may send to one delegated run; 0 = one-shot.
    pub max_exchanges: u32,
    pub source: AgentSource,
}

impl AgentDef {
    fn builtin(name: &str, description: &str, system: &'static str, read_only: bool) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            system: system.into(),
            read_only,
            tools: None,
            deny_tools: Vec::new(),
            agents: Vec::new(),
            model: None,
            max_steps: None,
            max_exchanges: 0,
            source: AgentSource::Builtin,
        }
    }
}

/// Should `tool` be in this agent's toolset? One rule for builtin and custom
/// kinds: read-only strips mutators, internal denials apply, then the
/// allowlist (if any). Unknown allowlist names are tolerated here — the list
/// intersects whatever tools the host actually assembled.
pub fn keeps_tool(def: &AgentDef, tool: &dyn Tool) -> bool {
    if def.read_only && tool.is_mutating() {
        return false;
    }
    if def.deny_tools.iter().any(|deny| deny == tool.name()) {
        return false;
    }
    match &def.tools {
        Some(allow) => allow.iter().any(|name| name == tool.name()),
        None => true,
    }
}

#[derive(Debug, Clone)]
pub struct AgentRegistry {
    defs: Vec<AgentDef>,
}

impl AgentRegistry {
    /// The three compiled-in task kinds. `plan` is explore minus `exit_plan`:
    /// approval and the plan-mode transition remain exclusive to the parent.
    pub fn builtin() -> Self {
        let mut plan = AgentDef::builtin(
            "plan",
            "Read-only implementation-plan draft the parent reviews and submits",
            PLAN_SYSTEM,
            true,
        );
        plan.deny_tools = vec!["exit_plan".into()];
        Self {
            defs: vec![
                AgentDef::builtin(
                    "explore",
                    "Read-only reconnaissance that returns a report",
                    EXPLORE_SYSTEM,
                    true,
                ),
                plan,
                AgentDef::builtin(
                    "general",
                    "Independent multi-step work with the full toolset",
                    GENERAL_SYSTEM,
                    false,
                ),
            ],
        }
    }

    /// Builtin kinds plus user definitions. Validation is warn-and-skip, never
    /// fatal: a broken definition file must not take the CLI down.
    pub fn discover(cwd: &Path) -> (Self, Vec<String>) {
        let mut registry = Self::builtin();
        let mut warnings = Vec::new();
        let mut roots = vec![cwd.join(".tcode/agents"), cwd.join(".claude/agents")];
        if let Some(home) = dirs::home_dir() {
            roots.push(home.join(".tcode/agents"));
            roots.push(home.join(".claude/agents"));
        }
        for root in roots {
            let Ok(entries) = std::fs::read_dir(&root) else {
                continue;
            };
            let mut files: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
                .collect();
            files.sort();
            for file in files {
                match parse_def(&file) {
                    Ok(def) => {
                        let taken_by_builtin = registry
                            .get(&def.name)
                            .map(|existing| matches!(existing.source, AgentSource::Builtin));
                        match taken_by_builtin {
                            // Builtin kinds are reserved: their never-ask
                            // permission tier is welded to read_only, and an
                            // override could silently widen it. This is a
                            // deliberate difference from skills.
                            Some(true) => warnings.push(format!(
                                "agent '{}' ({}): name is reserved for a builtin kind, skipped",
                                def.name,
                                file.display()
                            )),
                            // First-wins dedup, like skills: project beats home.
                            Some(false) => {}
                            None => registry.defs.push(def),
                        }
                    }
                    Err(reason) => {
                        warnings.push(format!("agent file {}: {reason}, skipped", file.display()))
                    }
                }
            }
        }
        // Spawn lists may only reference known kinds; unknown names would
        // surface as schema enums the tool then rejects.
        let known: Vec<String> = registry.defs.iter().map(|def| def.name.clone()).collect();
        for def in &mut registry.defs {
            let name = def.name.clone();
            def.agents.retain(|target| {
                if *target == name {
                    warnings.push(format!(
                        "agent '{name}': spawning itself is bounded only by task depth; dropped from its agents list"
                    ));
                    return false;
                }
                let ok = known.contains(target);
                if !ok {
                    warnings.push(format!(
                        "agent '{name}': unknown agent '{target}' in agents list, dropped"
                    ));
                }
                ok
            });
        }
        (registry, warnings)
    }

    pub fn get(&self, name: &str) -> Option<&AgentDef> {
        self.defs.iter().find(|def| def.name == name)
    }

    /// Kind names for the `task` input schema, optionally restricted to a
    /// caller's spawn allowlist.
    pub fn names_for(&self, allow: Option<&[String]>) -> Vec<&str> {
        self.defs
            .iter()
            .map(|def| def.name.as_str())
            .filter(|name| allow.is_none_or(|allow| allow.iter().any(|a| a == name)))
            .collect()
    }

    /// User-authored definitions only (for model-hint merging and warnings).
    pub fn custom(&self) -> impl Iterator<Item = &AgentDef> {
        self.defs
            .iter()
            .filter(|def| matches!(def.source, AgentSource::File(_)))
    }

    /// The `Custom agents:` section of the task tool description, budgeted
    /// like the skills listing. Empty when only builtins exist, so the
    /// description stays byte-identical to the static one in that case.
    pub fn custom_listing(&self, allow: Option<&[String]>) -> String {
        let listed: Vec<&AgentDef> = self
            .custom()
            .filter(|def| allow.is_none_or(|allow| allow.contains(&def.name)))
            .collect();
        if listed.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n\nCustom agents (project/user defined):\n");
        let mut overflow: Vec<&str> = Vec::new();
        for def in listed {
            if overflow.is_empty() && out.len() < LISTING_CAP {
                let resumable = if def.max_exchanges > 0 {
                    format!(" [resumable: up to {} follow-up turns]", def.max_exchanges)
                } else {
                    String::new()
                };
                out.push_str(&format!(
                    "- {}: {}{resumable}\n",
                    def.name,
                    clip(&def.description, DESCRIPTION_CAP)
                ));
            } else {
                overflow.push(&def.name);
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

    /// Warn about allowlist entries that name no assembled tool. Definitions
    /// are kept: tool sets vary per host (e.g. `bash` only exists next to Git
    /// Bash), and a hard drop would make definitions non-portable.
    pub fn validate_tools(&self, tool_names: &[&str]) -> Vec<String> {
        let mut warnings = Vec::new();
        for def in self.custom() {
            for name in def.tools.iter().flatten().chain(&def.deny_tools) {
                if !tool_names.contains(&name.as_str()) && name != "task" {
                    warnings.push(format!(
                        "agent '{}': tool '{name}' is not available in this environment",
                        def.name
                    ));
                }
            }
        }
        warnings
    }
}

/// Names double as cache-scope fragments and schema enum entries, so the
/// character set stays deliberately narrow.
fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && name.len() <= 48
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn comma_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(String::from)
        .collect()
}

fn parse_def(file: &Path) -> Result<AgentDef, String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("cannot read: {e}"))?;
    let meta = front_matter(&text);
    let name = meta
        .get("name")
        .cloned()
        .or_else(|| {
            file.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    if !valid_name(&name) {
        return Err(format!(
            "invalid name '{name}' (want ^[a-z0-9][a-z0-9_-]{{0,47}}$)"
        ));
    }
    let description = meta.get("description").cloned().unwrap_or_default();
    if description.trim().is_empty() {
        // The description is the only signal the model has for choosing this
        // agent; a definition without one is unreachable in practice.
        return Err("missing description".into());
    }
    let system = strip_front_matter(&text).trim().to_string();
    if system.is_empty() {
        return Err("empty body (the body is the agent's system prompt)".into());
    }
    let model = AgentModelHint {
        profile: meta.get("profile").cloned(),
        model: meta.get("model").cloned(),
        effort: meta.get("effort").cloned(),
    };
    Ok(AgentDef {
        name,
        description,
        system,
        read_only: meta.get("readonly").map(String::as_str) == Some("true"),
        tools: meta.get("tools").map(String::as_str).map(comma_list),
        deny_tools: Vec::new(),
        agents: meta
            .get("agents")
            .map(String::as_str)
            .map(comma_list)
            .unwrap_or_default(),
        model: (!model.is_empty()).then_some(model),
        max_steps: meta.get("max_steps").and_then(|v| v.parse().ok()),
        max_exchanges: meta
            .get("max_exchanges")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        source: AgentSource::File(file.to_path_buf()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_def(dir: &Path, file: &str, contents: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(file), contents).unwrap();
    }

    #[test]
    fn builtin_registry_matches_the_legacy_kind_semantics() {
        let registry = AgentRegistry::builtin();
        assert_eq!(registry.names_for(None), ["explore", "plan", "general"]);
        assert!(registry.get("explore").unwrap().read_only);
        assert_eq!(registry.get("plan").unwrap().deny_tools, ["exit_plan"]);
        assert!(!registry.get("general").unwrap().read_only);
        assert!(registry.custom_listing(None).is_empty());
    }

    #[test]
    fn keeps_tool_reproduces_the_builtin_tiers() {
        let registry = AgentRegistry::builtin();
        let tools = crate::builtin_tools(&std::env::temp_dir());
        let names = |def: &AgentDef| -> Vec<&str> {
            tools
                .iter()
                .filter(|tool| keeps_tool(def, tool.as_ref()))
                .map(|tool| tool.name())
                .collect::<Vec<_>>()
        };
        let explore = names(registry.get("explore").unwrap());
        let plan = names(registry.get("plan").unwrap());
        let general = names(registry.get("general").unwrap());
        assert!(tools
            .iter()
            .filter(|tool| keeps_tool(registry.get("explore").unwrap(), tool.as_ref()))
            .all(|tool| !tool.is_mutating()));
        assert_eq!(
            plan,
            explore
                .iter()
                .copied()
                .filter(|name| *name != "exit_plan")
                .collect::<Vec<_>>()
        );
        assert_eq!(general.len(), tools.len());
    }

    #[test]
    fn keeps_tool_applies_allowlists_on_top_of_readonly() {
        let mut def = AgentDef::builtin("t", "d", "", true);
        def.tools = Some(vec!["read".into(), "write".into()]);
        let tools = crate::builtin_tools(&std::env::temp_dir());
        let kept: Vec<&str> = tools
            .iter()
            .filter(|tool| keeps_tool(&def, tool.as_ref()))
            .map(|tool| tool.name())
            .collect();
        // write is mutating: readonly wins over the allowlist.
        assert_eq!(kept, ["read"]);
    }

    #[test]
    fn discover_parses_a_full_definition() {
        let tmp = tempfile::tempdir().unwrap();
        write_def(
            &tmp.path().join(".tcode/agents"),
            "quant-dev.md",
            "---\ndescription: backtests strategies\ntools: read, shell , edit\nagents: helper\nmodel: gpt-5.2\nmax_steps: 80\nmax_exchanges: 5\n---\nYou are a quant developer.",
        );
        write_def(
            &tmp.path().join(".tcode/agents"),
            "helper.md",
            "---\ndescription: helps\n---\nHelp.",
        );
        let (registry, warnings) = AgentRegistry::discover(tmp.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let def = registry.get("quant-dev").unwrap();
        assert_eq!(def.description, "backtests strategies");
        assert_eq!(def.system, "You are a quant developer.");
        assert_eq!(def.tools.as_deref().unwrap(), ["read", "shell", "edit"]);
        assert_eq!(def.agents, ["helper"]);
        assert_eq!(
            def.model.as_ref().unwrap().model.as_deref(),
            Some("gpt-5.2")
        );
        assert_eq!(def.max_steps, Some(80));
        assert_eq!(def.max_exchanges, 5);
        assert_eq!(
            registry.names_for(None),
            ["explore", "plan", "general", "helper", "quant-dev"]
        );
    }

    #[test]
    fn discover_skips_broken_and_reserved_definitions_with_warnings() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".tcode/agents");
        write_def(&dir, "explore.md", "---\ndescription: hijack\n---\nbody");
        write_def(&dir, "no-desc.md", "---\nname: no-desc\n---\nbody");
        write_def(&dir, "Bad Name.md", "---\ndescription: d\n---\nbody");
        write_def(
            &dir,
            "dangling.md",
            "---\ndescription: d\nagents: ghost, dangling\n---\nbody",
        );
        let (registry, warnings) = AgentRegistry::discover(tmp.path());
        assert!(registry.get("no-desc").is_none());
        assert!(matches!(
            registry.get("explore").unwrap().source,
            AgentSource::Builtin
        ));
        let def = registry.get("dangling").unwrap();
        assert!(def.agents.is_empty(), "ghost + self dropped");
        assert_eq!(warnings.len(), 5, "{warnings:?}");
    }

    #[test]
    fn discover_prefers_project_definitions_over_home_variants() {
        let tmp = tempfile::tempdir().unwrap();
        write_def(
            &tmp.path().join(".tcode/agents"),
            "dev.md",
            "---\ndescription: tcode one\n---\nA",
        );
        write_def(
            &tmp.path().join(".claude/agents"),
            "dev.md",
            "---\ndescription: claude one\n---\nB",
        );
        let (registry, _) = AgentRegistry::discover(tmp.path());
        assert_eq!(registry.get("dev").unwrap().description, "tcode one");
    }

    #[test]
    fn custom_listing_caps_and_overflows_to_names_only() {
        let mut registry = AgentRegistry::builtin();
        for i in 0..40 {
            let mut def = AgentDef::builtin(&format!("agent{i}"), &"d".repeat(150), "body", false);
            def.source = AgentSource::File(PathBuf::from("x"));
            registry.defs.push(def);
        }
        let listing = registry.custom_listing(None);
        assert!(listing.len() < LISTING_CAP + 1_000);
        assert!(listing.contains("names only"));
        assert!(listing.contains("agent39"));
        // Restricting to an allowlist restricts the listing.
        let allow = vec!["agent1".to_string()];
        let restricted = registry.custom_listing(Some(&allow));
        assert!(restricted.contains("agent1:"));
        assert!(!restricted.contains("agent2:"));
    }

    #[test]
    fn validate_tools_flags_unknown_names_only_for_custom_defs() {
        let tmp = tempfile::tempdir().unwrap();
        write_def(
            &tmp.path().join(".tcode/agents"),
            "dev.md",
            "---\ndescription: d\ntools: read, frobnicate\n---\nbody",
        );
        let (registry, _) = AgentRegistry::discover(tmp.path());
        let warnings = registry.validate_tools(&["read", "write"]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("frobnicate"));
    }
}
