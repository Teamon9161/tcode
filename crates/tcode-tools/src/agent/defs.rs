//! Sub-agent definitions: compiled defaults and user-authored
//! `.tcode/agents/*.md` personas share one registry and one capability model.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_yaml::{Mapping, Value};
use tcode_core::Tool;

use crate::frontmatter::{clip, strip_front_matter, yaml_front_matter};

/// One listed line per custom agent; longer descriptions are clipped.
const DESCRIPTION_CAP: usize = 200;
/// Budget for the whole agent catalogue inside the model-facing tool description.
const LISTING_CAP: usize = 2_000;
/// Nesting bound: an agent at this depth no longer receives an `agent` tool.
pub const MAX_TASK_DEPTH: usize = 3;
/// How deep `discover` descends under an `agents/` root. Skills ship their
/// agents in a subdirectory (often symlinked in, e.g. `agents/impeccable/`), so
/// the scan is recursive; the bound only exists to stop a symlink cycle from
/// looping forever, never to organize a real layout.
const MAX_AGENT_DIR_DEPTH: usize = 8;

// Rust cannot expand `include_str!` over a directory. The build script scans
// `src/agent/builtin/*.md` and emits this resource manifest; it owns no agent
// names, descriptions, prompts, or capability policies.
include!(concat!(env!("OUT_DIR"), "/builtin_agents.rs"));

#[derive(Debug, Clone)]
pub enum AgentSource {
    /// A compile-time resource path under `src/agent/builtin`.
    Builtin(&'static str),
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

/// A tool selector is deliberately definition-level, never an input-value
/// permission rule. Exact names and whole MCP-server groups are enough to
/// shape an agent's capability set without reimplementing the permission DSL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSelector {
    Exact(String),
    /// The platform command tools: `shell` and, where available, `bash`.
    Run,
    AllMcp,
    McpServer(String),
}

impl ToolSelector {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("tool selector cannot be empty".into());
        }
        if raw == "run" {
            return Ok(Self::Run);
        }
        if raw == "mcp__*" {
            return Ok(Self::AllMcp);
        }
        if let Some(server) = raw
            .strip_prefix("mcp__")
            .and_then(|rest| rest.strip_suffix("__*"))
        {
            if server.is_empty() || server.contains('*') {
                return Err(format!(
                    "invalid MCP selector '{raw}'; want mcp__<server>__*"
                ));
            }
            return Ok(Self::McpServer(server.to_string()));
        }
        if raw.contains('*') {
            return Err(format!(
                "unsupported tool wildcard '{raw}'; only mcp__* and mcp__<server>__* are supported"
            ));
        }
        Ok(Self::Exact(raw.to_string()))
    }

    pub fn matches(&self, tool_name: &str) -> bool {
        match self {
            Self::Exact(name) => name == tool_name,
            Self::Run => matches!(tool_name, "shell" | "bash"),
            Self::AllMcp => tool_name.starts_with("mcp__"),
            Self::McpServer(server) => tool_name
                .strip_prefix("mcp__")
                .and_then(|rest| rest.strip_prefix(server))
                .is_some_and(|suffix| suffix.starts_with("__")),
        }
    }

    pub fn display(&self) -> String {
        match self {
            Self::Exact(name) => name.clone(),
            Self::Run => "run".into(),
            Self::AllMcp => "mcp__*".into(),
            Self::McpServer(server) => format!("mcp__{server}__*"),
        }
    }
}

/// The two front-matter styles are deliberately exclusive. An allowlist says
/// "only these tools"; a denylist says "inherit everything except these".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolPolicy {
    #[default]
    Inherit,
    Allow(Vec<ToolSelector>),
    Deny(Vec<ToolSelector>),
}

impl ToolPolicy {
    pub fn keeps(&self, tool_name: &str) -> bool {
        match self {
            Self::Inherit => true,
            Self::Allow(selectors) => selectors.iter().any(|selector| selector.matches(tool_name)),
            Self::Deny(selectors) => !selectors.iter().any(|selector| selector.matches(tool_name)),
        }
    }

    pub fn selectors(&self) -> Option<&[ToolSelector]> {
        match self {
            Self::Inherit => None,
            Self::Allow(selectors) | Self::Deny(selectors) => Some(selectors),
        }
    }

    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QuestionPolicy {
    #[default]
    Disabled,
    User,
}

/// Which agent kinds a definition may spawn, mirroring `tools` /
/// `disallowedTools`. An allowlist names the spawnable kinds (empty = leaf);
/// a denylist spawns every registered kind except those listed and itself,
/// so a coordinator automatically covers kinds defined after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnPolicy {
    Allow(Vec<String>),
    Deny(Vec<String>),
}

impl Default for SpawnPolicy {
    fn default() -> Self {
        Self::Allow(Vec::new())
    }
}

impl SpawnPolicy {
    fn list_mut(&mut self) -> &mut Vec<String> {
        match self {
            Self::Allow(names) | Self::Deny(names) => names,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// The sub-agent's system prompt (markdown body of the definition file).
    pub system: String,
    /// A hard capability ceiling: mutating tools are removed before the
    /// allowlist/denylist is considered. User questions are separately
    /// governed by `question_policy`.
    pub read_only: bool,
    /// Whether this agent may ask the human through its parent conversation.
    /// Defaults to disabled; the parent UI remains the only interaction surface.
    pub question_policy: QuestionPolicy,
    pub tool_policy: ToolPolicy,
    /// Agent kinds this one may spawn; the default (empty allowlist) is a
    /// leaf with no `agent` tool. Resolve through `AgentRegistry::spawn_list`.
    pub spawn: SpawnPolicy,
    pub model: Option<AgentModelHint>,
    /// Maximum model round-trips in one delegated turn.
    pub max_steps: Option<usize>,
    /// Whether the final report sent back to the parent should pass through
    /// the parent's blob gate. Internal tool output remains gated in the
    /// sub-agent's own session regardless of this setting.
    pub gates_output: bool,
    /// Follow-up turns a caller may send to one delegated run; 0 = one-shot.
    pub max_exchanges: u32,
    pub source: AgentSource,
    /// The subdirectory a custom agent was discovered under, when it was not
    /// placed directly in an `agents/` root — typically the name of the skill
    /// that installed it (`agents/impeccable/…` → `impeccable`). Surfaced in
    /// the model-facing catalogue as provenance so the model can connect a
    /// skill's "spawn my bundled agent" instruction to the agent it names, and
    /// so an installed definition reads as data from a package rather than a
    /// name the user authored. `None` for builtins and top-level files.
    pub source_label: Option<String>,
}

/// Should `tool` be in this agent's toolset? The same rule applies to builtin
/// and custom agents. `readonly` is a non-bypassable upper bound: no allowlist
/// can re-add a mutating tool.
pub fn keeps_tool(def: &AgentDef, tool: &dyn Tool) -> bool {
    (!def.read_only || !tool.is_mutating()) && def.tool_policy.keeps(tool.name())
}

#[derive(Debug, Clone)]
pub struct AgentRegistry {
    defs: Vec<AgentDef>,
}

impl AgentRegistry {
    /// The stable default kinds are compiled-in Markdown files. They are
    /// intentionally reserved: project agent files may add personas but cannot
    /// silently redefine the default safety and selection semantics the main
    /// prompt advertises.
    pub fn builtin() -> Self {
        let defs = BUILTIN_AGENT_FILES
            .iter()
            .map(|(path, text)| {
                let (def, warnings) = parse_def_text(text, None, AgentSource::Builtin(path), path)
                    .unwrap_or_else(|error| panic!("invalid builtin agent {path}: {error}"));
                assert!(warnings.is_empty(), "builtin agent {path}: {warnings:?}");
                def
            })
            .collect();
        Self { defs }
    }

    /// Builtin kinds plus project/user `.tcode/agents` definitions. Validation
    /// is warn-and-skip, never fatal: a broken definition cannot take the CLI
    /// down. Project definitions win over user definitions.
    pub fn discover(cwd: &Path) -> (Self, Vec<String>) {
        let mut registry = Self::builtin();
        let mut warnings = Vec::new();
        let mut roots = vec![cwd.join(".tcode/agents")];
        if let Some(home) = tcode_core::home_dir() {
            roots.push(home.join(".tcode/agents"));
        }
        for root in roots {
            let mut files = Vec::new();
            collect_agent_files(&root, 0, &mut files);
            files.sort();
            for file in files {
                match parse_def(&file) {
                    Ok((mut def, mut parse_warnings)) => {
                        warnings.append(&mut parse_warnings);
                        // The top-level subdirectory under this root, if any, is
                        // the installing group (usually a skill name).
                        def.source_label = file
                            .strip_prefix(&root)
                            .ok()
                            .and_then(|rel| rel.parent())
                            .and_then(|dir| dir.components().next())
                            .and_then(|first| first.as_os_str().to_str())
                            .map(str::to_string);
                        let taken_by_builtin = registry
                            .get(&def.name)
                            .map(|existing| matches!(existing.source, AgentSource::Builtin(_)));
                        match taken_by_builtin {
                            Some(true) => warnings.push(format!(
                                "agent '{}' ({}): name is reserved for a builtin kind, skipped",
                                def.name,
                                file.display()
                            )),
                            // First-wins: project beats user.
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
        let known: BTreeSet<String> = registry.defs.iter().map(|def| def.name.clone()).collect();
        for def in &mut registry.defs {
            let name = def.name.clone();
            let is_allow = matches!(def.spawn, SpawnPolicy::Allow(_));
            let key = if is_allow {
                "agents"
            } else {
                "disallowedAgents"
            };
            def.spawn.list_mut().retain(|target| {
                if *target == name {
                    if is_allow {
                        warnings.push(format!(
                            "agent '{name}': spawning itself is bounded only by agent depth; dropped from its agents list"
                        ));
                    }
                    // In a denylist self is implicitly excluded already.
                    return false;
                }
                let ok = known.contains(target);
                if !ok {
                    warnings.push(format!(
                        "agent '{name}': unknown agent '{target}' in {key} list, dropped"
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

    /// Kind names for the `agent` input schema, optionally restricted to a
    /// caller's spawn allowlist.
    pub fn names_for(&self, allow: Option<&[String]>) -> Vec<&str> {
        self.defs
            .iter()
            .map(|def| def.name.as_str())
            .filter(|name| allow.is_none_or(|allow| allow.iter().any(|a| a == name)))
            .collect()
    }

    pub fn visible_defs<'a>(
        &'a self,
        allow: Option<&'a [String]>,
    ) -> impl Iterator<Item = &'a AgentDef> + 'a {
        self.defs
            .iter()
            .filter(move |def| allow.is_none_or(|allow| allow.iter().any(|name| name == &def.name)))
    }

    /// User-authored definitions only (for model-hint merging and warnings).
    pub fn custom(&self) -> impl Iterator<Item = &AgentDef> {
        self.defs
            .iter()
            .filter(|def| matches!(def.source, AgentSource::File(_)))
    }

    /// One budgeted catalogue for the model-facing agent tool description.
    pub fn catalogue(&self, allow: Option<&[String]>) -> String {
        let mut out = String::from("Available agents:\n");
        let mut overflow = Vec::new();
        for def in self.visible_defs(allow) {
            let readonly = if def.read_only { " [read-only]" } else { "" };
            let from = def
                .source_label
                .as_deref()
                .map(|label| format!(" (installed by {label})"))
                .unwrap_or_default();
            let line = format!(
                "- {}{}{}: {}\n",
                def.name,
                readonly,
                from,
                clip(&def.description, DESCRIPTION_CAP)
            );
            if overflow.is_empty() && out.len() + line.len() <= LISTING_CAP {
                out.push_str(&line);
            } else {
                overflow.push(def.name.as_str());
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

    /// Evaluate user-authored policies against the actual base tool inventory
    /// delegated runs receive. Explicit empty allowlists are valid. An allowlist
    /// that selects nothing after the `readonly` ceiling is applied is not
    /// advertised; no model can successfully invoke a definition that has no
    /// requested capability.
    pub fn validate_for_tools(&mut self, tools: &[Arc<dyn Tool>]) -> Vec<String> {
        let mut warnings = Vec::new();
        let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
        let mut rejected = BTreeSet::new();

        for def in self.custom() {
            let selectors = def.tool_policy.selectors().unwrap_or_default();
            let matched: Vec<&dyn Tool> = tools
                .iter()
                .map(AsRef::as_ref)
                .filter(|tool| def.tool_policy.keeps(tool.name()))
                .collect();
            for selector in selectors {
                if !names.iter().any(|name| selector.matches(name)) {
                    let policy = if def.tool_policy.is_allow() {
                        "allow"
                    } else {
                        "deny"
                    };
                    warnings.push(format!(
                        "agent '{}': {policy} selector '{}' matches no tool available to sub-agents in this environment",
                        def.name,
                        selector.display()
                    ));
                }
            }

            if !def.tool_policy.is_allow() || selectors.is_empty() {
                continue;
            }
            if matched.is_empty() {
                warnings.push(format!(
                    "agent '{}': its allowlist resolves to no tools available to sub-agents, skipped",
                    def.name
                ));
                rejected.insert(def.name.clone());
                continue;
            }
            if !def.read_only {
                continue;
            }
            let selected_before_readonly: Vec<&dyn Tool> = matched;
            let selected_after_readonly: Vec<&dyn Tool> = selected_before_readonly
                .iter()
                .copied()
                .filter(|tool| !tool.is_mutating())
                .collect();
            let stripped: Vec<&str> = selected_before_readonly
                .iter()
                .copied()
                .filter(|tool| tool.is_mutating())
                .map(Tool::name)
                .collect();
            if !stripped.is_empty() {
                warnings.push(format!(
                    "agent '{}': readonly removed mutating allowlisted tools: {}",
                    def.name,
                    stripped.join(", ")
                ));
            }
            if !selectors.is_empty() && selected_after_readonly.is_empty() {
                warnings.push(format!(
                    "agent '{}': readonly leaves its allowlist with no usable tools, skipped",
                    def.name
                ));
                rejected.insert(def.name.clone());
            }
        }

        if !rejected.is_empty() {
            self.defs.retain(|def| {
                !matches!(def.source, AgentSource::File(_)) || !rejected.contains(&def.name)
            });
            self.drop_unknown_spawn_targets(&mut warnings);
        }
        warnings
    }

    fn drop_unknown_spawn_targets(&mut self, warnings: &mut Vec<String>) {
        let known: BTreeSet<String> = self.defs.iter().map(|def| def.name.clone()).collect();
        for def in &mut self.defs {
            let name = def.name.clone();
            let is_allow = matches!(def.spawn, SpawnPolicy::Allow(_));
            def.spawn.list_mut().retain(|target| {
                let ok = known.contains(target);
                if !ok && is_allow {
                    // A skipped kind vanishing from a denylist changes nothing.
                    warnings.push(format!(
                        "agent '{name}': agent '{target}' was skipped and was dropped from its agents list"
                    ));
                }
                ok
            });
        }
    }

    /// The concrete kinds `def` may spawn. A denylist resolves against the
    /// current registry, so it covers custom kinds discovered after the
    /// definition was written. An empty result means a leaf: no `agent` tool.
    pub fn spawn_list(&self, def: &AgentDef) -> Vec<String> {
        match &def.spawn {
            SpawnPolicy::Allow(names) => names.clone(),
            SpawnPolicy::Deny(denied) => self
                .defs
                .iter()
                .map(|other| other.name.clone())
                .filter(|name| *name != def.name && !denied.contains(name))
                .collect(),
        }
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

fn value<'a>(meta: &'a Mapping, key: &str) -> Option<&'a Value> {
    meta.get(Value::String(key.to_string()))
}

fn string(meta: &Mapping, key: &str) -> Result<Option<String>, String> {
    match value(meta, key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

fn question_policy(meta: &Mapping) -> Result<QuestionPolicy, String> {
    match string(meta, "questionPolicy")?.as_deref() {
        None | Some("disabled") => Ok(QuestionPolicy::Disabled),
        Some("user") => Ok(QuestionPolicy::User),
        Some(_) => Err("`questionPolicy` must be `disabled` or `user`".into()),
    }
}

fn bool(meta: &Mapping, key: &str) -> Result<bool, String> {
    match value(meta, key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(format!("`{key}` must be true or false")),
    }
}

fn bool_or(meta: &Mapping, key: &str, default: bool) -> Result<bool, String> {
    match value(meta, key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(format!("`{key}` must be true or false")),
    }
}

fn usize(meta: &Mapping, key: &str) -> Result<Option<usize>, String> {
    match value(meta, key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value > 0)
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a positive integer")),
        Some(_) => Err(format!("`{key}` must be a positive integer")),
    }
}

fn u32(meta: &Mapping, key: &str) -> Result<Option<u32>, String> {
    match value(meta, key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a non-negative integer")),
        Some(_) => Err(format!("`{key}` must be a non-negative integer")),
    }
}

fn string_list(meta: &Mapping, key: &str) -> Result<Option<Vec<String>>, String> {
    let Some(value) = value(meta, key) else {
        return Ok(None);
    };
    let values = match value {
        Value::Null => Vec::new(),
        Value::String(value) => value
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(String::from)
            .collect(),
        Value::Sequence(values) => values
            .iter()
            .map(|value| match value {
                Value::String(value) if !value.trim().is_empty() => Ok(value.trim().to_string()),
                _ => Err(format!("`{key}` must contain only non-empty strings")),
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => {
            return Err(format!(
                "`{key}` must be a comma-separated string or YAML list"
            ))
        }
    };
    Ok(Some(values))
}

fn selectors(meta: &Mapping, key: &str) -> Result<Option<Vec<ToolSelector>>, String> {
    string_list(meta, key)?
        .map(|values| {
            values
                .iter()
                .map(|value| {
                    ToolSelector::parse(value).map_err(|error| format!("`{key}`: {error}"))
                })
                .collect()
        })
        .transpose()
}

/// Gather every `.md`/`.toml` agent file under `root`, descending into
/// subdirectories and following directory symlinks (`metadata` resolves them),
/// so a skill's symlinked `agents/<skill>/` folder is discovered like a
/// hand-placed file. Unreadable entries and paths past the depth bound are
/// silently skipped: discovery is best-effort and must never fail the CLI.
fn collect_agent_files(root: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_AGENT_DIR_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // `metadata` follows symlinks, so a symlinked agents directory counts
        // as a directory and is walked.
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => collect_agent_files(&path, depth + 1, out),
            Ok(_)
                if path
                    .extension()
                    .is_some_and(|ext| ext == "md" || ext == "toml") =>
            {
                out.push(path)
            }
            _ => {}
        }
    }
}

fn parse_def(file: &Path) -> Result<(AgentDef, Vec<String>), String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("cannot read: {e}"))?;
    let fallback_name = file.file_stem().and_then(|stem| stem.to_str());
    let source = AgentSource::File(file.to_path_buf());
    let origin = file.display().to_string();
    if file.extension().is_some_and(|ext| ext == "toml") {
        parse_def_toml(&text, fallback_name, source, &origin)
    } else {
        parse_def_text(&text, fallback_name, source, &origin)
    }
}

/// Markdown parser for user files and compiled-in resources: YAML front matter
/// carries the fields, the body is the system prompt. `source` controls
/// provenance only; every agent's fields and validation rules come from the
/// shared `build_def`.
fn parse_def_text(
    text: &str,
    fallback_name: Option<&str>,
    source: AgentSource,
    origin: &str,
) -> Result<(AgentDef, Vec<String>), String> {
    let meta = yaml_front_matter(text)?;
    let system = strip_front_matter(text).trim().to_string();
    build_def(meta, system, fallback_name, source, origin)
}

/// TOML parser for Codex/Impeccable-style agent files: the whole file is the
/// metadata table and the body is a field. The field set is normalized onto the
/// same `Mapping` the Markdown path produces, so both formats share `build_def`
/// and every validation rule. Field aliases:
/// - `developer_instructions` / `instructions` → the system prompt body.
/// - `model_reasoning_effort` → `effort` (a native `effort` key still wins).
///
/// Keys with no tcode meaning (e.g. `nickname_candidates`) are ignored, exactly
/// as unknown YAML front-matter keys are.
fn parse_def_toml(
    text: &str,
    fallback_name: Option<&str>,
    source: AgentSource,
    origin: &str,
) -> Result<(AgentDef, Vec<String>), String> {
    let table: toml::Table = toml::from_str(text).map_err(|e| format!("invalid TOML: {e}"))?;
    let value =
        serde_yaml::to_value(&table).map_err(|e| format!("cannot read TOML fields: {e}"))?;
    let Value::Mapping(mut meta) = value else {
        return Err("agent definition must be a table of fields".into());
    };
    // The body is a field here rather than the file's trailing text.
    let system = match meta
        .remove("developer_instructions")
        .or_else(|| meta.remove("instructions"))
    {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(body)) => body.trim().to_string(),
        Some(_) => {
            return Err("`developer_instructions` must be a string".into());
        }
    };
    // Effort alias: only fill `effort` when the native key is absent.
    if !meta.contains_key("effort") {
        if let Some(effort) = meta.remove("model_reasoning_effort") {
            meta.insert(Value::String("effort".to_string()), effort);
        }
    }
    build_def(meta, system, fallback_name, source, origin)
}

/// The format-agnostic core: given the parsed field map and system prompt body,
/// validate and assemble one `AgentDef`. Both the Markdown and TOML parsers feed
/// this, so a field means the same thing whichever file format declared it.
fn build_def(
    meta: Mapping,
    system: String,
    fallback_name: Option<&str>,
    source: AgentSource,
    origin: &str,
) -> Result<(AgentDef, Vec<String>), String> {
    let name = string(&meta, "name")?
        .or_else(|| fallback_name.map(ToOwned::to_owned))
        .unwrap_or_default();
    if !valid_name(&name) {
        return Err(format!(
            "invalid name '{name}' (want ^[a-z0-9][a-z0-9_-]{{0,47}}$)"
        ));
    }
    let description = string(&meta, "description")?.unwrap_or_default();
    if description.trim().is_empty() {
        return Err("missing description".into());
    }
    if system.is_empty() {
        return Err("empty body (the body is the agent's system prompt)".into());
    }
    let allow = selectors(&meta, "tools")?;
    let deny = selectors(&meta, "disallowedTools")?;
    if allow.is_some() && deny.is_some() {
        return Err("`tools` and `disallowedTools` are mutually exclusive; choose an allowlist or a denylist".into());
    }
    let tool_policy = match (allow, deny) {
        (Some(allow), None) => ToolPolicy::Allow(allow),
        (None, Some(deny)) => ToolPolicy::Deny(deny),
        (None, None) => ToolPolicy::Inherit,
        (Some(_), Some(_)) => unreachable!("checked above"),
    };
    let allow_agents = string_list(&meta, "agents")?;
    let deny_agents = string_list(&meta, "disallowedAgents")?;
    let spawn = match (allow_agents, deny_agents) {
        (Some(_), Some(_)) => {
            return Err(
                "`agents` and `disallowedAgents` are mutually exclusive; choose an allowlist or a denylist"
                    .into(),
            )
        }
        (Some(names), None) => SpawnPolicy::Allow(names),
        (None, Some(names)) => SpawnPolicy::Deny(names),
        (None, None) => SpawnPolicy::default(),
    };
    let max_turns = usize(&meta, "maxTurns")?;
    let legacy_max_steps = usize(&meta, "max_steps")?;
    if max_turns.is_some() && legacy_max_steps.is_some() {
        return Err("`maxTurns` and legacy `max_steps` cannot both be set".into());
    }
    let mut warnings = Vec::new();
    if legacy_max_steps.is_some() {
        warnings.push(format!(
            "agent '{name}' ({origin}): `max_steps` is deprecated; use `maxTurns`"
        ));
    }
    let model = AgentModelHint {
        profile: string(&meta, "profile")?,
        model: string(&meta, "model")?,
        effort: string(&meta, "effort")?,
    };
    Ok((
        AgentDef {
            name,
            description,
            system,
            read_only: bool(&meta, "readonly")?,
            question_policy: question_policy(&meta)?,
            tool_policy,
            spawn,
            model: (!model.is_empty()).then_some(model),
            max_steps: max_turns.or(legacy_max_steps),
            gates_output: bool_or(&meta, "gatesOutput", true)?,
            max_exchanges: u32(&meta, "max_exchanges")?.unwrap_or(0),
            source,
            // Discovery fills this in for nested files; a builtin or a
            // top-level definition has no installing subdirectory.
            source_label: None,
        },
        warnings,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_def(dir: &Path, file: &str, contents: &str) {
        // `discover` also scans `home_dir().join(".tcode/agents")`; without an
        // isolated home it reads the developer's real `~/.tcode/agents` and any
        // agents installed there leak into exact-list assertions.
        tcode_core::home::testing::temp_home();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(file), contents).unwrap();
    }

    #[test]
    fn builtin_registry_is_derived_from_embedded_markdown() {
        let registry = AgentRegistry::builtin();
        assert_eq!(registry.defs.len(), BUILTIN_AGENT_FILES.len());
        for (path, text) in BUILTIN_AGENT_FILES {
            assert!(path.starts_with("src/agent/builtin/"));
            let (parsed, warnings) =
                parse_def_text(text, None, AgentSource::Builtin(path), path).unwrap();
            assert!(warnings.is_empty(), "{path}: {warnings:?}");
            let registered = registry
                .get(&parsed.name)
                .expect("parsed builtin is registered");
            assert_eq!(registered.description, parsed.description);
            assert_eq!(registered.system, parsed.system);
            assert_eq!(registered.read_only, parsed.read_only);
            assert_eq!(registered.question_policy, parsed.question_policy);
            assert_eq!(registered.tool_policy, parsed.tool_policy);
            assert_eq!(registered.spawn, parsed.spawn);
        }
    }

    #[test]
    fn discover_parses_toml_codex_agents() {
        let tmp = tempfile::tempdir().unwrap();
        write_def(
            &tmp.path().join(".tcode/agents"),
            "asset-producer.toml",
            "name = \"asset-producer\"\n\
             description = \"Produces clean reusable raster assets.\"\n\
             model_reasoning_effort = \"medium\"\n\
             nickname_candidates = [\"Clean Plate\", \"Crop Cutter\"]\n\
             developer_instructions = '''\n\
             You are the asset production agent.\n\
             Work only from the approved mock.\n\
             '''\n",
        );
        let (registry, warnings) = AgentRegistry::discover(tmp.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let def = registry.get("asset-producer").unwrap();
        assert_eq!(def.description, "Produces clean reusable raster assets.");
        // model_reasoning_effort maps onto the native effort pin.
        assert_eq!(
            def.model.as_ref().and_then(|m| m.effort.as_deref()),
            Some("medium")
        );
        // developer_instructions is the system prompt; unknown keys are ignored.
        assert!(def
            .system
            .starts_with("You are the asset production agent."));
        assert!(def.system.contains("approved mock"));
    }

    #[test]
    fn discover_descends_into_subdirectories() {
        let tmp = tempfile::tempdir().unwrap();
        // A skill drops its agents in a nested folder under the agents root.
        write_def(
            &tmp.path().join(".tcode/agents/impeccable"),
            "producer.toml",
            "description = \"nested agent\"\ninstructions = \"Do the nested work.\"\n",
        );
        let (registry, warnings) = AgentRegistry::discover(tmp.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let def = registry.get("producer").unwrap();
        assert!(def.source_label.is_some());
        // The installing subdirectory becomes model-facing provenance.
        assert_eq!(def.source_label.as_deref(), Some("impeccable"));
        assert!(registry
            .catalogue(None)
            .contains("producer (installed by impeccable):"));
    }

    #[test]
    fn top_level_custom_agents_carry_no_source_label() {
        let tmp = tempfile::tempdir().unwrap();
        write_def(
            &tmp.path().join(".tcode/agents"),
            "reviewer.md",
            "---\ndescription: d\n---\nbody",
        );
        let (registry, warnings) = AgentRegistry::discover(tmp.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(registry.get("reviewer").unwrap().source_label.is_none());
    }

    #[test]
    fn toml_agents_share_every_native_field_and_alias_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        write_def(
            &tmp.path().join(".tcode/agents"),
            "reviewer.toml",
            "description = \"review a change\"\n\
             readonly = true\n\
             tools = [\"read\", \"grep\"]\n\
             effort = \"high\"\n\
             model_reasoning_effort = \"low\"\n\
             instructions = \"Review the change and cite evidence.\"\n",
        );
        let (mut registry, warnings) = AgentRegistry::discover(tmp.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let tools = crate::builtin_tools(tmp.path());
        let _ = registry.validate_for_tools(&tools);
        let def = registry.get("reviewer").unwrap();
        // Filename supplies the name; native TOML fields parse like YAML ones.
        assert!(def.read_only);
        assert!(matches!(def.tool_policy, ToolPolicy::Allow(_)));
        // A native effort key wins over the model_reasoning_effort alias.
        assert_eq!(
            def.model.as_ref().and_then(|m| m.effort.as_deref()),
            Some("high")
        );
        assert_eq!(def.system, "Review the change and cite evidence.");
    }

    #[test]
    fn selector_matches_exact_groups_and_mcp_groups() {
        assert!(ToolSelector::parse("read").unwrap().matches("read"));
        let run = ToolSelector::parse("run").unwrap();
        assert!(run.matches("shell"));
        assert!(run.matches("bash"));
        assert!(!run.matches("read"));
        assert!(ToolSelector::parse("mcp__*")
            .unwrap()
            .matches("mcp__github__issue"));
        assert!(ToolSelector::parse("mcp__github__*")
            .unwrap()
            .matches("mcp__github__issue"));
        assert!(!ToolSelector::parse("mcp__github__*")
            .unwrap()
            .matches("mcp__gitlab__issue"));
        assert!(ToolSelector::parse("shell(*)").is_err());
    }

    #[test]
    fn readonly_wins_over_allowlist() {
        let (def, warnings) = parse_def_text(
            "---\nname: t\ndescription: d\nreadonly: true\ntools: [write]\n---\nbody",
            None,
            AgentSource::Builtin("test"),
            "test",
        )
        .unwrap();
        assert!(warnings.is_empty());
        let tools = crate::builtin_tools(&std::env::temp_dir());
        assert!(tools
            .iter()
            .find(|tool| keeps_tool(&def, tool.as_ref()))
            .is_none());
    }

    #[test]
    fn discover_parses_yaml_lists_and_max_turns() {
        let tmp = tempfile::tempdir().unwrap();
        write_def(
            &tmp.path().join(".tcode/agents"),
            "quant-dev.md",
            "---\ndescription: backtests strategies\ntools: [read, shell, edit]\nagents:\n  - helper\nmodel: gpt-5.2\nmaxTurns: 80\ngatesOutput: false\nmax_exchanges: 5\n---\nYou are a quant developer.",
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
        assert_eq!(def.max_steps, Some(80));
        assert!(!def.gates_output);
        assert!(registry.get("helper").unwrap().gates_output);
        assert!(matches!(def.tool_policy, ToolPolicy::Allow(_)));
        assert_eq!(registry.spawn_list(def), ["helper"]);
        assert_eq!(def.question_policy, QuestionPolicy::Disabled);
    }

    #[test]
    fn a_denylist_spawn_policy_covers_later_defined_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".tcode/agents");
        write_def(
            &dir,
            "coordinator.md",
            "---\ndescription: d\ndisallowedAgents: [plan]\n---\nbody",
        );
        write_def(&dir, "worker.md", "---\ndescription: d\n---\nbody");
        let (registry, warnings) = AgentRegistry::discover(tmp.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let spawn = registry.spawn_list(registry.get("coordinator").unwrap());
        assert!(spawn.contains(&"worker".to_string()));
        assert!(spawn.contains(&"explore".to_string()));
        assert!(!spawn.contains(&"plan".to_string()));
        assert!(!spawn.contains(&"coordinator".to_string()));
        // A definition without either key stays a leaf.
        assert!(registry
            .spawn_list(registry.get("worker").unwrap())
            .is_empty());
    }

    #[test]
    fn spawn_allow_and_deny_forms_are_mutually_exclusive() {
        let tmp = tempfile::tempdir().unwrap();
        write_def(
            &tmp.path().join(".tcode/agents"),
            "both.md",
            "---\ndescription: d\nagents: [explore]\ndisallowedAgents: [plan]\n---\nbody",
        );
        let (registry, warnings) = AgentRegistry::discover(tmp.path());
        assert!(registry.get("both").is_none());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    #[test]
    fn discover_rejects_conflicting_tool_forms_and_bad_values() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".tcode/agents");
        write_def(
            &dir,
            "conflict.md",
            "---\ndescription: d\ntools: [read]\ndisallowedTools: [write]\n---\nbody",
        );
        write_def(
            &dir,
            "bad-readonly.md",
            "---\ndescription: d\nreadonly: yes\n---\nbody",
        );
        let (registry, warnings) = AgentRegistry::discover(tmp.path());
        assert!(registry.get("conflict").is_none());
        assert!(registry.get("bad-readonly").is_none());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }

    #[test]
    fn project_tcode_definition_wins_over_user_style_second_root() {
        let tmp = tempfile::tempdir().unwrap();
        write_def(
            &tmp.path().join(".tcode/agents"),
            "dev.md",
            "---\ndescription: project\n---\nA",
        );
        let (registry, _) = AgentRegistry::discover(tmp.path());
        assert_eq!(registry.get("dev").unwrap().description, "project");
    }

    #[test]
    fn builtin_names_remain_reserved() {
        let builtin_name = AgentRegistry::builtin()
            .visible_defs(None)
            .next()
            .expect("at least one embedded builtin")
            .name
            .clone();
        let tmp = tempfile::tempdir().unwrap();
        write_def(
            &tmp.path().join(".tcode/agents"),
            "override.md",
            &format!(
                "---\nname: {}\ndescription: hijack\nreadonly: false\n---\nbody",
                builtin_name
            ),
        );
        let (registry, warnings) = AgentRegistry::discover(tmp.path());
        assert!(matches!(
            registry.get(&builtin_name).unwrap().source,
            AgentSource::Builtin(_)
        ));
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn validation_skips_empty_allowlists_but_keeps_explicit_no_tool_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".tcode/agents");
        write_def(
            &dir,
            "missing.md",
            "---\ndescription: missing\ntools: [mcp__github__*]\n---\nbody",
        );
        write_def(
            &dir,
            "analysis.md",
            "---\ndescription: pure analysis\ntools: []\n---\nbody",
        );
        let (mut registry, warnings) = AgentRegistry::discover(tmp.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let tools = crate::builtin_tools(tmp.path());
        let warnings = registry.validate_for_tools(&tools);
        assert!(registry.get("missing").is_none());
        assert!(registry.get("analysis").is_some());
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("missing") && warning.contains("skipped")));
    }

    #[test]
    fn validation_applies_readonly_after_allowlist_and_warns_about_stripped_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".tcode/agents");
        write_def(
            &dir,
            "mixed.md",
            "---\ndescription: mixed\nreadonly: true\ntools: [read, write]\n---\nbody",
        );
        write_def(
            &dir,
            "write-only.md",
            "---\ndescription: write only\nreadonly: true\ntools: [write]\n---\nbody",
        );
        let (mut registry, warnings) = AgentRegistry::discover(tmp.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let tools = crate::builtin_tools(tmp.path());
        let warnings = registry.validate_for_tools(&tools);
        assert!(registry.get("mixed").is_some());
        assert!(registry.get("write-only").is_none());
        assert!(warnings.iter().any(|warning| warning.contains("mixed")
            && warning.contains("readonly removed")
            && warning.contains("write")));
        assert!(warnings.iter().any(|warning| warning.contains("write-only")
            && warning.contains("no usable tools")
            && warning.contains("skipped")));
    }

    #[test]
    fn validation_keeps_portable_denylists_that_match_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        write_def(
            &tmp.path().join(".tcode/agents"),
            "portable.md",
            "---\ndescription: portable\ndisallowedTools: [mcp__github__*]\n---\nbody",
        );
        let (mut registry, warnings) = AgentRegistry::discover(tmp.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let tools = crate::builtin_tools(tmp.path());
        let warnings = registry.validate_for_tools(&tools);
        assert!(registry.get("portable").is_some());
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("deny selector")
                    && warning.contains("mcp__github__*"))
        );
    }

    #[test]
    fn catalogue_uses_effective_definitions_including_builtins() {
        let registry = AgentRegistry::builtin();
        let catalogue = registry.catalogue(None);
        for def in registry.visible_defs(None) {
            assert!(catalogue.contains(&format!(
                "{}{}:",
                def.name,
                if def.read_only { " [read-only]" } else { "" }
            )));
        }
    }
}
