//! Progress: one durable file per multi-phase task.
//!
//! A progress file is the same object a session used to split between an
//! in-memory `update_progress` list and a reviewed plan draft: an ordered work
//! breakdown. The only differences were durability and whether a human had
//! nodded, so both are properties of one object here — `state` says whether the
//! breakdown has been approved, and the file says it survives the session.
//!
//! Two invariants make this cheap rather than a verbose markdown edit loop, and
//! both are structural rather than prompt discipline:
//!
//! 1. **The tool is the file's only writer.** The model never `edit`s this
//!    markdown, so a phase flip costs one call instead of two round trips of
//!    full text. [`Progress::reconcile`] is what makes that safe when the user
//!    edits the file by hand.
//! 2. **Phase detail is delivered on demand.** A twelve-phase plan keeps
//!    exactly one phase's prose in context — the one just entered, handed back
//!    by [`Progress::set_phases`].
//!
//! A progress file is externally mutable state, not history. The ledger records
//! the tool calls the model made; this file records what is true now. Nothing
//! here touches the append-only invariant.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::store;

/// Nesting bound. Deeper than this means the plan wants splitting into two
/// progress files; without a hard cap models generate tree-shaped monsters.
pub const MAX_DEPTH: usize = 2;

/// An unfinished progress file older than this stops appearing in the opening
/// inventory. Nothing is deleted — a stale draft is still the user's.
const STALE_AFTER: Duration = Duration::from_secs(14 * 24 * 3600);

/// How many unfinished progress files the opening inventory lists.
pub const INVENTORY_LIMIT: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProgressState {
    /// Written, not yet approved. The model must not start executing it.
    #[default]
    Draft,
    /// Approved (or model-authored for work that needs no approval).
    Active,
    /// Every phase landed. Archived: kept on disk, dropped from the inventory.
    Done,
}

impl ProgressState {
    pub fn label(self) -> &'static str {
        match self {
            ProgressState::Draft => "draft",
            ProgressState::Active => "active",
            ProgressState::Done => "done",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "draft" => Some(ProgressState::Draft),
            "active" => Some(ProgressState::Active),
            "done" => Some(ProgressState::Done),
            _ => None,
        }
    }

    /// Legal transitions are the lifecycle's own arrows: a draft is approved
    /// into `active`, an active plan is either finished or sent back for
    /// revision. Everything else — reviving an archived plan, skipping the
    /// approval that `active` means — is rejected rather than silently applied.
    fn may_become(self, next: Self) -> bool {
        use ProgressState::*;
        self == next
            || matches!(
                (self, next),
                (Draft, Active) | (Active, Done) | (Active, Draft)
            )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PhaseStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
}

impl PhaseStatus {
    /// The box a human reads and can type by hand. Chosen over any richer
    /// encoding precisely because all three parties — user, model, parser —
    /// agree on it at a glance.
    fn box_mark(self) -> &'static str {
        match self {
            PhaseStatus::Pending => "[ ]",
            PhaseStatus::InProgress => "[>]",
            PhaseStatus::Completed => "[x]",
        }
    }

    fn from_box(mark: &str) -> Option<Self> {
        match mark {
            "[ ]" | "[]" => Some(PhaseStatus::Pending),
            "[>]" => Some(PhaseStatus::InProgress),
            "[x]" | "[X]" => Some(PhaseStatus::Completed),
            _ => None,
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "pending" => Some(PhaseStatus::Pending),
            "in_progress" => Some(PhaseStatus::InProgress),
            "completed" => Some(PhaseStatus::Completed),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PhaseStatus::Pending => "pending",
            PhaseStatus::InProgress => "in_progress",
            PhaseStatus::Completed => "completed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Phase {
    pub phase: String,
    pub status: PhaseStatus,
    /// Why, which files, what risk. The most valuable text in the file and the
    /// one that must not sit in context permanently.
    pub detail: String,
    pub phases: Vec<Phase>,
}

impl Phase {
    pub fn new(phase: impl Into<String>, status: PhaseStatus) -> Self {
        Self {
            phase: phase.into(),
            status,
            detail: String::new(),
            phases: Vec::new(),
        }
    }

    /// Read one phase out of model-authored tool input. `depth` is 1 for a
    /// top-level phase; the cap is enforced here so no caller can bypass it.
    fn from_json(value: &Value, depth: usize) -> Result<Self, String> {
        let phase = value["phase"]
            .as_str()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .ok_or_else(|| "every phase needs a non-empty `phase` title".to_string())?
            .to_string();
        let status = value["status"]
            .as_str()
            .map_or(Some(PhaseStatus::Pending), PhaseStatus::parse)
            .ok_or_else(|| {
                format!(
                    "phase '{phase}' has an unknown `status`; use pending, in_progress or completed"
                )
            })?;
        let detail = value["detail"].as_str().unwrap_or("").trim().to_string();
        let nested = value["phases"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        if !nested.is_empty() && depth >= MAX_DEPTH {
            return Err(format!(
                "phase '{phase}' nests deeper than {MAX_DEPTH} levels; a plan that needs a third level should be split into two progress files"
            ));
        }
        let phases = nested
            .iter()
            .map(|child| Phase::from_json(child, depth + 1))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            phase,
            status,
            detail,
            phases,
        })
    }

    fn walk<'a>(&'a self, out: &mut Vec<&'a Phase>) {
        out.push(self);
        for child in &self.phases {
            child.walk(out);
        }
    }
}

/// Read a whole `phases` array out of tool input.
pub fn phases_from_json(value: &Value) -> Result<Vec<Phase>, String> {
    let Some(items) = value.as_array() else {
        return Err("`phases` must be an array".to_string());
    };
    items.iter().map(|item| Phase::from_json(item, 1)).collect()
}

/// The file rejected a write because the bytes on disk are no longer the ones
/// the harness last wrote. The user edited the plan; their version wins.
#[derive(Debug, Clone)]
pub struct Conflict {
    pub path: PathBuf,
    pub contents: String,
}

impl Conflict {
    /// A self-healing message: it carries the user's current text, so the model
    /// never has to spend a `read` to find out what changed.
    pub fn message(&self) -> String {
        format!(
            "The user edited this progress file since tcode last wrote it, so the update was not applied. Their version is authoritative — here it is in full. Re-send `phases` based on it.\n\n<tcode-progress-file path=\"{}\">\n{}\n</tcode-progress-file>",
            self.path.display(),
            self.contents.trim_end()
        )
    }
}

#[derive(Debug, Clone)]
pub struct Progress {
    path: PathBuf,
    pub title: String,
    state: ProgressState,
    created: String,
    phases: Vec<Phase>,
    /// Hash of the bytes last read from or written to `path`. A mismatch means
    /// a human edited the file.
    disk_hash: u64,
}

impl Progress {
    /// Start a new progress file for this project. The path is allocated now
    /// (timestamped, so files sort and never collide) but nothing is written
    /// until [`Progress::save`].
    pub fn create(cwd: &Path, title: &str) -> Result<Self, String> {
        let dir = store::progress_dir(cwd);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("could not create progress directory {}: {e}", dir.display()))?;
        let path = next_path(&dir, title)?;
        Ok(Self {
            path,
            title: title.trim().to_string(),
            state: ProgressState::Draft,
            created: timestamp_rfc3339(SystemTime::now()),
            phases: Vec::new(),
            // No file yet: any hash that cannot match an existing file's would
            // do, but 0 also means "never written", which `reconcile` needs.
            disk_hash: 0,
        })
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("could not read progress file {}: {e}", path.display()))?;
        Self::parse(path, &text)
    }

    pub fn parse(path: &Path, text: &str) -> Result<Self, String> {
        let (front, body) = split_front_matter(text);
        let front: FrontMatter = serde_yaml::from_str(front)
            .map_err(|e| format!("invalid progress front matter in {}: {e}", path.display()))?;
        let title = match front.title.trim() {
            "" => file_title(path),
            title => title.to_string(),
        };
        Ok(Self {
            path: path.to_path_buf(),
            title,
            state: ProgressState::parse(&front.state).unwrap_or_default(),
            created: front.created,
            phases: parse_phases(body),
            disk_hash: fnv1a(text.as_bytes()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn state(&self) -> ProgressState {
        self.state
    }

    pub fn phases(&self) -> &[Phase] {
        &self.phases
    }

    /// Move to another lifecycle state. Rejects the transitions the lifecycle
    /// has no arrow for rather than quietly reinterpreting them.
    pub fn set_state(&mut self, next: ProgressState) -> Result<(), String> {
        if !self.state.may_become(next) {
            return Err(format!(
                "a progress file cannot go from {} to {}",
                self.state.label(),
                next.label()
            ));
        }
        if next == ProgressState::Done && !self.all_complete() {
            return Err(
                "a progress file can become done only after every phase is completed".into(),
            );
        }
        self.state = next;
        Ok(())
    }

    /// Apply the parts of a `progress` call a review does not decide: the
    /// breakdown. The title chooses the progress file before this method runs;
    /// it is never mutable content of an existing tracker. Split out from
    /// [`Progress::apply`] because a draft submitted for approval is saved
    /// *before* the human answers — their decision is about `state`, and
    /// applying it up front would make declining silently promote the plan.
    pub fn apply_content(&mut self, input: &Value) -> Result<Option<String>, String> {
        match input.get("phases") {
            Some(phases) if !phases.is_null() => Ok(self.set_phases(phases_from_json(phases)?)),
            _ => Ok(None),
        }
    }

    /// Apply one whole `progress` tool input. Phases land before `state` so a
    /// `done` transition is judged against the breakdown the same call sent.
    pub fn apply(&mut self, input: &Value) -> Result<Option<String>, String> {
        let entered = self.apply_content(input)?;
        if let Some(raw) = input["state"].as_str() {
            let next = ProgressState::parse(raw)
                .ok_or_else(|| format!("unknown state '{raw}'; use draft, active or done"))?;
            self.set_state(next)?;
        }
        Ok(entered)
    }

    /// Replace the whole breakdown (the tool resends it in full — idempotent,
    /// no diffing). Returns the detail of the phase that just became
    /// `in_progress`, which is the one piece of prose worth spending context
    /// on right now.
    pub fn set_phases(&mut self, phases: Vec<Phase>) -> Option<String> {
        let was_running = self.running_titles();
        self.phases = phases;
        let entered = self
            .flatten()
            .into_iter()
            .find(|phase| {
                phase.status == PhaseStatus::InProgress && !was_running.contains(&phase.phase)
            })
            .filter(|phase| !phase.detail.is_empty())
            .map(|phase| format!("{}\n{}", phase.phase, phase.detail));
        entered
    }

    fn running_titles(&self) -> Vec<String> {
        self.flatten()
            .into_iter()
            .filter(|phase| phase.status == PhaseStatus::InProgress)
            .map(|phase| phase.phase.clone())
            .collect()
    }

    fn flatten(&self) -> Vec<&Phase> {
        let mut out = Vec::new();
        for phase in &self.phases {
            phase.walk(&mut out);
        }
        out
    }

    /// `(completed, total)` over every phase at every level.
    pub fn counts(&self) -> (usize, usize) {
        let all = self.flatten();
        let done = all
            .iter()
            .filter(|phase| phase.status == PhaseStatus::Completed)
            .count();
        (done, all.len())
    }

    /// Whether every phase landed, so the file can be archived.
    pub fn all_complete(&self) -> bool {
        let (done, total) = self.counts();
        total > 0 && done == total
    }

    /// Refuse to overwrite a file the user has edited since tcode wrote it.
    /// Called before every write; the returned conflict carries their text so
    /// the caller never has to read the file back.
    pub fn reconcile(&self) -> Result<(), Conflict> {
        // Never written: there is nothing of the user's to protect. A file that
        // appeared under this path in the meantime is still theirs, though.
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Ok(());
        };
        if fnv1a(text.as_bytes()) == self.disk_hash {
            return Ok(());
        }
        Err(Conflict {
            path: self.path.clone(),
            contents: text,
        })
    }

    pub fn save(&mut self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "could not create progress directory {}: {e}",
                    parent.display()
                )
            })?;
        }
        let text = self.render();
        std::fs::write(&self.path, &text)
            .map_err(|e| format!("could not write progress file {}: {e}", self.path.display()))?;
        self.disk_hash = fnv1a(text.as_bytes());
        Ok(())
    }

    pub fn render(&self) -> String {
        let mut out = format!(
            "---\ntitle: {}\nstate: {}\ncreated: {}\n---\n",
            yaml_scalar(&self.title),
            self.state.label(),
            self.created
        );
        out.push_str(&self.body());
        out
    }

    /// The phases as markdown, without the front matter. This is what a review
    /// pane shows and what `$EDITOR` round-trips: the lifecycle belongs to the
    /// approval buttons, so it is not put in front of the reviewer's cursor.
    pub fn body(&self) -> String {
        render_phases(&self.phases)
    }

    /// Adopt a human-authored body verbatim. The inverse of [`Progress::body`],
    /// used for the version a reviewer approved after rewriting it.
    pub fn set_body(&mut self, markdown: &str) {
        self.phases = parse_phases(markdown);
    }

    /// The model-facing summary: title lines and boxes, never the detail
    /// prose. Detail reaches the model one phase at a time, through the tool
    /// result that entered it.
    pub fn summary(&self) -> String {
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut out = format!(
            "<tcode-progress file=\"{}\" title=\"{}\" state=\"{}\">\n",
            name,
            escape_attr(&self.title),
            self.state.label()
        );
        for (i, phase) in self.phases.iter().enumerate() {
            summarize_phase(&mut out, phase, &format!("{}", i + 1), 0);
        }
        out.push_str("</tcode-progress>");
        out
    }
}

/// A breakdown as the markdown a reader sees. Free-standing so a frontend can
/// render a plan straight out of a tool call, before any file exists for it.
pub fn render_phases(phases: &[Phase]) -> String {
    let mut out = String::new();
    for (i, phase) in phases.iter().enumerate() {
        render_phase(&mut out, phase, &format!("{}", i + 1), 2);
    }
    out
}

fn render_phase(out: &mut String, phase: &Phase, number: &str, level: usize) {
    out.push('\n');
    out.push_str(&"#".repeat(level));
    out.push_str(&format!(
        " {} {number}. {}\n",
        phase.status.box_mark(),
        phase.phase
    ));
    if !phase.detail.is_empty() {
        out.push_str(phase.detail.trim_end());
        out.push('\n');
    }
    for (i, child) in phase.phases.iter().enumerate() {
        render_phase(out, child, &format!("{number}.{}", i + 1), level + 1);
    }
}

fn summarize_phase(out: &mut String, phase: &Phase, number: &str, indent: usize) {
    out.push_str(&"  ".repeat(indent));
    out.push_str(&format!(
        "{} {number}. {}",
        phase.status.box_mark(),
        phase.phase
    ));
    if phase.status == PhaseStatus::InProgress {
        out.push_str("  ← current");
    }
    out.push('\n');
    for (i, child) in phase.phases.iter().enumerate() {
        summarize_phase(out, child, &format!("{number}.{}", i + 1), indent + 1);
    }
}

/// The rendered body attached to the *review* copy of a `progress` call, and
/// the field an approved rewrite comes back through. The model never sends it:
/// its schema has no `plan` property, and [`ProgressTool`-side handling] only
/// honours it on the one call the human just approved, which by construction
/// went through the dialog.
///
/// [`ProgressTool`-side handling]: crate::progress::is_submission
pub const REVIEW_BODY_FIELD: &str = "plan";

/// The draft's path, attached to the review copy so the pane can show it, copy
/// it, and open it in `$EDITOR`. Display only: nothing writes through it — the
/// tool reaches its file through the session's own handle — so unlike the
/// mechanism it replaces it needs no anti-escalation check, because it grants
/// no capability to check.
pub const REVIEW_PATH_FIELD: &str = "_progress_path";

/// Whether this call asks the user to approve a plan. `state: "active"` is the
/// approval request itself: a progress file the model opens for its own
/// tracking is active from birth and says nothing about `state`, so the two
/// intents never collide.
pub fn is_submission(input: &Value) -> bool {
    input["state"].as_str() == Some(ProgressState::Active.label())
}

/// The session's selected progress file, opening a new file when this call
/// names a different task. A title identifies a tracker rather than mutable
/// metadata: a reused session may keep its current file only when the incoming
/// title is absent or exactly matches. That lets a declined draft remain on
/// disk without making the next task overwrite its title or phases.
fn current_or_create<'a>(
    slot: &'a mut Option<Progress>,
    cwd: &Path,
    input: &Value,
    fresh: ProgressState,
) -> Result<&'a mut Progress, String> {
    let title = input["title"]
        .as_str()
        .map(str::trim)
        .filter(|title| !title.is_empty());
    let needs_new = match slot.as_ref() {
        Some(progress) => title.is_some_and(|title| title != progress.title),
        None => true,
    };
    if needs_new {
        let title = title.ok_or_else(|| {
            "there is no progress file open yet, so this call needs a `title`".to_string()
        })?;
        let mut created = Progress::create(cwd, title)?;
        created.state = fresh;
        *slot = Some(created);
    }
    Ok(slot.as_mut().expect("progress just created"))
}

/// Persist a submitted plan and return the review copy of its input.
///
/// The draft is written before the review reaches the user, so a declined plan
/// keeps its file and the next revision replaces it in place rather than
/// leaving a trail of near-duplicates. Only `state` is withheld — that is the
/// question being asked.
pub fn review_copy(ctx: &crate::tool::ToolCtx, input: &Value) -> Result<Value, String> {
    let mut slot = ctx.progress.lock().expect("progress lock");
    // What is under review is by definition a draft, even the first time.
    let progress = current_or_create(&mut slot, &ctx.cwd, input, ProgressState::Draft)?;
    progress
        .reconcile()
        .map_err(|conflict| conflict.message())?;
    progress.apply_content(input)?;
    progress.save()?;
    let mut review = input.clone();
    review[REVIEW_BODY_FIELD] = Value::String(progress.body());
    review[REVIEW_PATH_FIELD] = Value::String(progress.path().display().to_string());
    Ok(review)
}

/// Apply an ordinary (or approved) `progress` call to the session's file.
/// Returns the tool's own result text.
pub fn apply_call(ctx: &crate::tool::ToolCtx, input: &Value) -> Result<String, String> {
    let mut slot = ctx.progress.lock().expect("progress lock");
    // `draft` is the only thing that opens a draft. A file the model opens to
    // track its own work needs nobody's approval, so it is active from birth —
    // which is what lets `state: "active"` mean one thing only: submit the
    // draft I already have.
    let fresh = match input["state"].as_str() == Some(ProgressState::Draft.label()) {
        true => ProgressState::Draft,
        false => ProgressState::Active,
    };
    let progress = current_or_create(&mut slot, &ctx.cwd, input, fresh)?;
    let entered = if is_submission(input) {
        // The approved call: whatever the reviewer left in the pane is the
        // authority, including a rewrite, so `reconcile` is deliberately not
        // consulted — this write *is* the human's answer about this file.
        match input[REVIEW_BODY_FIELD].as_str() {
            Some(body) => progress.set_body(body),
            // No review copy reached the tool, so nothing overrode the model's
            // own breakdown — take it as sent.
            None => {
                progress.apply_content(input)?;
            }
        }
        progress.set_state(ProgressState::Active)?;
        None
    } else {
        progress
            .reconcile()
            .map_err(|conflict| conflict.message())?;
        progress.apply(input)?
    };
    progress.save()?;
    let (done, total) = progress.counts();
    let state = progress.state();
    let mut result = format!("{} · {done}/{total} phases done", state.label());
    if let Some(entered) = entered {
        result.push_str("\n\nNow starting — ");
        result.push_str(&entered);
    }
    // A finished plan stops being this conversation's current one, so the next
    // task opens its own file instead of appending to a completed one.
    if state == ProgressState::Done {
        *slot = None;
    }
    Ok(result)
}

/// This conversation's current progress summary, if it has one.
pub fn current_summary(ctx: &crate::tool::ToolCtx) -> Option<String> {
    ctx.progress
        .lock()
        .expect("progress lock")
        .as_ref()
        .map(Progress::summary)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FrontMatter {
    title: String,
    state: String,
    created: String,
}

/// Split `---\n…\n---\n` off the front. A file without front matter is still
/// readable: a user can hand-write one and the defaults apply.
fn split_front_matter(text: &str) -> (&str, &str) {
    let rest = match text.strip_prefix("---\n") {
        Some(rest) => rest,
        None => match text.strip_prefix("---\r\n") {
            Some(rest) => rest,
            None => return ("", text),
        },
    };
    for marker in ["\n---\n", "\n---\r\n"] {
        if let Some(end) = rest.find(marker) {
            return (&rest[..end], &rest[end + marker.len()..]);
        }
    }
    // A closing `---` on the last line, with no trailing newline.
    match rest
        .strip_suffix("\n---")
        .or_else(|| rest.strip_suffix("\n---\n"))
    {
        Some(front) => (front, ""),
        None => ("", text),
    }
}

/// Body → phases. Headings carry the box and an optional `1.` / `1.2` number
/// (rendered by us, ignored on the way back in so a user may renumber freely);
/// everything between headings is the previous phase's detail.
fn parse_phases(body: &str) -> Vec<Phase> {
    let mut top: Vec<Phase> = Vec::new();
    // Which phase the detail lines currently belong to.
    let mut detail_of: Option<(usize, Option<usize>)> = None;
    for line in body.lines() {
        if let Some((level, status, title)) = parse_heading(line) {
            let phase = Phase {
                phase: title,
                status,
                detail: String::new(),
                phases: Vec::new(),
            };
            match level {
                2 => {
                    top.push(phase);
                    detail_of = Some((top.len() - 1, None));
                    continue;
                }
                3 if !top.is_empty() => {
                    let parent = top.len() - 1;
                    top[parent].phases.push(phase);
                    let child = top[parent].phases.len() - 1;
                    detail_of = Some((parent, Some(child)));
                    continue;
                }
                // A marked heading beyond the supported two levels remains
                // ordinary prose. Silently flattening it into a child would
                // make a hand-edited plan mean something different.
                _ => {}
            }
        }
        let Some((parent, child)) = detail_of else {
            continue;
        };
        let target = match child {
            Some(child) => &mut top[parent].phases[child].detail,
            None => &mut top[parent].detail,
        };
        target.push_str(line);
        target.push('\n');
    }
    for phase in &mut top {
        phase.detail = phase.detail.trim().to_string();
        for child in &mut phase.phases {
            child.detail = child.detail.trim().to_string();
        }
    }
    top
}

/// `## [>] 2. Title` → `(2, InProgress, "Title")`. A heading without a box is
/// not a phase; it is prose the user wrote.
fn parse_heading(line: &str) -> Option<(usize, PhaseStatus, String)> {
    let level = line.chars().take_while(|c| *c == '#').count();
    if level < 2 || !line[level..].starts_with(' ') {
        return None;
    }
    let rest = line[level..].trim_start();
    let (mark, rest) = rest.split_at(rest.find(']')? + 1);
    let status = PhaseStatus::from_box(mark)?;
    let rest = rest.trim_start();
    let title = match rest.split_once(' ') {
        Some((head, tail)) if is_numbering(head) => tail,
        _ => rest,
    };
    let title = title.trim();
    (!title.is_empty()).then(|| (level, status, title.to_string()))
}

/// A leading `2.` / `2.1` / `2.1.` — the numbering we render, or whatever the
/// user renumbered it to — which is dropped on the way back in so a renumbered
/// plan does not grow its numbers into its phase titles. A dot is required, so
/// a phase that genuinely opens with a number ("2024 migration") keeps it.
fn is_numbering(head: &str) -> bool {
    head.contains('.')
        && !head.trim_end_matches('.').is_empty()
        && head
            .trim_end_matches('.')
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.')
}

/// One line per unfinished progress file, newest first.
#[derive(Debug, Clone)]
pub struct InventoryEntry {
    pub path: PathBuf,
    pub title: String,
    pub state: ProgressState,
    pub done: usize,
    pub total: usize,
    pub modified: SystemTime,
}

impl InventoryEntry {
    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    }
}

/// Unfinished progress files for this project, most recently touched first.
/// `done` files and files nobody has touched in two weeks are left on disk but
/// dropped here — an inventory that lists every abandoned draft lists nothing.
///
/// **This is a listing, not an instruction.** A draft found here was written by
/// whoever wrote it, possibly a different task three days ago; seeing it is not
/// a request to continue it.
pub fn inventory(cwd: &Path, limit: usize) -> Vec<InventoryEntry> {
    let mut entries = inventory_at(&store::progress_dir(cwd), SystemTime::now());
    entries.truncate(limit);
    entries
}

/// `now` is a parameter so staleness is testable without touching file mtimes.
fn inventory_at(dir: &Path, now: SystemTime) -> Vec<InventoryEntry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<InventoryEntry> = read
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .filter_map(|path| {
            let modified = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .unwrap_or(UNIX_EPOCH);
            let progress = Progress::load(&path).ok()?;
            if progress.state() == ProgressState::Done {
                return None;
            }
            if now
                .duration_since(modified)
                .is_ok_and(|age| age > STALE_AFTER)
            {
                return None;
            }
            let (done, total) = progress.counts();
            Some(InventoryEntry {
                path,
                title: progress.title,
                state: progress.state,
                done,
                total,
                modified,
            })
        })
        .collect();
    entries.sort_by(|a, b| b.modified.cmp(&a.modified));
    entries
}

/// The opening-context listing. Stable across a session (the model's own
/// updates are in the ledger), so it is safe in the cached prefix.
pub fn inventory_note(entries: &[InventoryEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let mut out = String::from("<tcode-progress-inventory>\nUnfinished progress files on disk. This is a listing of past work, not a request: do not resume one unless the user asks.\n");
    for entry in entries {
        out.push_str(&format!(
            "- {} [{}] {}/{} phases · {}\n",
            entry.title,
            entry.state.label(),
            entry.done,
            entry.total,
            entry.file_name()
        ));
    }
    out.push_str("</tcode-progress-inventory>");
    Some(out)
}

fn next_path(dir: &Path, title: &str) -> Result<PathBuf, String> {
    let stem = format!("{}-{}", timestamp_stamp(), slug(title));
    for suffix in 0..10_000 {
        let name = match suffix {
            0 => format!("{stem}.md"),
            n => format!("{stem}-{n}.md"),
        };
        let path = dir.join(name);
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(format!(
        "could not allocate a progress filename in {}",
        dir.display()
    ))
}

/// A title for a file whose front matter has none: the slug, minus the
/// timestamp we prefixed it with.
fn file_title(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    stem.split_once('-')
        .and_then(|(_, rest)| rest.split_once('-'))
        .map(|(_, rest)| rest.replace('-', " "))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(stem)
}

fn yaml_scalar(value: &str) -> String {
    // A title is free text; quoting it keeps `:` and `#` from turning the front
    // matter into something else.
    format!("{:?}", value)
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

/// `yyyymmdd-HHMMSS` in UTC, for filename ordering.
fn timestamp_stamp() -> String {
    let (y, mo, d, h, m, s) = civil_now(SystemTime::now());
    format!("{y:04}{mo:02}{d:02}-{h:02}{m:02}{s:02}")
}

fn timestamp_rfc3339(at: SystemTime) -> String {
    let (y, mo, d, h, m, s) = civil_now(at);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// civil-from-days, so neither the filename nor the front matter pulls in a
/// date crate.
fn civil_now(at: SystemTime) -> (i64, i64, i64, u64, u64, u64) {
    let secs = at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86400;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d, h, m, s)
}

/// Filesystem-safe short slug from the title.
///
/// Alphanumeric is judged by Unicode, not ASCII: a title written in Chinese,
/// Japanese or Cyrillic must still name its own file. Restricting this to ASCII
/// folds every such title to nothing and lands them all on the `progress`
/// fallback — which is exactly the directory of interchangeable names the
/// timestamped-title scheme exists to avoid. Everything `is_alphanumeric`
/// rejects is punctuation or whitespace, so the path separators and the Windows
/// reserved set are all still excluded.
fn slug(title: &str) -> String {
    let mut slug: String = title
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                '-'
            }
        })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug.trim_matches('-');
    let slug: String = slug.chars().take(40).collect();
    if slug.is_empty() {
        "progress".to_string()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Progress {
        let mut progress = Progress {
            path: PathBuf::from("/tmp/x/20260729-101200-demo.md"),
            title: "重写 ledger rewind 路径".into(),
            state: ProgressState::Active,
            created: "2026-07-29T10:12:00Z".into(),
            phases: Vec::new(),
            disk_hash: 0,
        };
        progress.set_phases(vec![
            Phase {
                phase: "勘查调用面".into(),
                status: PhaseStatus::Completed,
                detail: "只读。确认 rewind 之后 aux 事件的重放顺序。".into(),
                phases: Vec::new(),
            },
            Phase {
                phase: "改 truncate_tail 的归档语义".into(),
                status: PhaseStatus::InProgress,
                detail: "风险：compact 之后再 rewind 会跨越 Summary 边界。".into(),
                phases: vec![
                    Phase::new("先补一个跨 Summary 的回归测试", PhaseStatus::Completed),
                    Phase::new("再改实现", PhaseStatus::InProgress),
                ],
            },
            Phase::new("迁移调用方", PhaseStatus::Pending),
        ]);
        progress
    }

    #[test]
    fn render_and_parse_round_trip() {
        let original = sample();
        let text = original.render();
        let parsed = Progress::parse(original.path(), &text).unwrap();
        assert_eq!(parsed.title, original.title);
        assert_eq!(parsed.state(), original.state());
        assert_eq!(parsed.phases(), original.phases());
        assert_eq!(parsed.render(), text);
    }

    #[test]
    fn headings_carry_the_box_and_drop_our_numbering() {
        let text = "---\ntitle: t\nstate: active\ncreated: c\n---\n\n## [x] 1. First\nwhy\n\n### [>] 1.1 Nested\n\n## [ ] 2. Second\n";
        let parsed = Progress::parse(Path::new("/tmp/p.md"), text).unwrap();
        assert_eq!(parsed.phases().len(), 2);
        assert_eq!(parsed.phases()[0].phase, "First");
        assert_eq!(parsed.phases()[0].detail, "why");
        assert_eq!(parsed.phases()[0].phases[0].phase, "Nested");
        assert_eq!(parsed.phases()[0].phases[0].status, PhaseStatus::InProgress);
        assert_eq!(parsed.phases()[1].phase, "Second");
    }

    #[test]
    fn a_heading_without_a_box_is_prose_not_a_phase() {
        let text = "---\ntitle: t\nstate: draft\ncreated: c\n---\n\n## Background\nnot a phase\n\n## [ ] 1. Real\n";
        let parsed = Progress::parse(Path::new("/tmp/p.md"), text).unwrap();
        assert_eq!(parsed.phases().len(), 1);
        assert_eq!(parsed.phases()[0].phase, "Real");
    }

    #[test]
    fn deeply_nested_handwritten_heading_is_not_flattened_into_a_phase() {
        let text = "---\ntitle: t\nstate: draft\ncreated: c\n---\n\n## [ ] 1. Top\n\n### [ ] 1.1 Child\n\n#### [ ] 1.1.1 Too deep\n";
        let parsed = Progress::parse(Path::new("/tmp/p.md"), text).unwrap();
        assert_eq!(parsed.phases().len(), 1);
        assert_eq!(parsed.phases()[0].phases.len(), 1);
        assert!(parsed.phases()[0].phases[0].detail.contains("Too deep"));
    }

    #[test]
    fn nesting_is_capped_at_two_levels() {
        let deep = json!([{ "phase": "a", "status": "pending",
            "phases": [{ "phase": "b", "status": "pending",
                "phases": [{ "phase": "c", "status": "pending" }] }] }]);
        let error = phases_from_json(&deep).unwrap_err();
        assert!(error.contains("split into two progress files"), "{error}");

        let ok = json!([{ "phase": "a", "status": "pending",
            "phases": [{ "phase": "b", "status": "in_progress" }] }]);
        assert_eq!(phases_from_json(&ok).unwrap().len(), 1);
    }

    #[test]
    fn entering_a_phase_returns_its_detail_once() {
        let mut progress = sample();
        // Re-sending the same list enters nothing new.
        let phases = progress.phases().to_vec();
        assert!(progress.set_phases(phases).is_none());

        let entered = progress
            .set_phases(vec![
                Phase::new("勘查调用面", PhaseStatus::Completed),
                Phase {
                    phase: "迁移调用方".into(),
                    status: PhaseStatus::InProgress,
                    detail: "动 crates/tcode-core/src/ledger.rs 附近。".into(),
                    phases: Vec::new(),
                },
            ])
            .expect("newly entered phase carries its detail");
        assert!(entered.contains("迁移调用方"));
        assert!(entered.contains("ledger.rs"));
    }

    #[test]
    fn summary_carries_boxes_but_never_detail() {
        let summary = sample().summary();
        assert!(summary.contains("[x] 1. 勘查调用面"));
        assert!(summary.contains("← current"));
        assert!(summary.contains("state=\"active\""));
        assert!(
            !summary.contains("aux 事件"),
            "detail prose must stay out of the summary: {summary}"
        );
    }

    #[test]
    fn state_transitions_follow_the_lifecycle() {
        let mut progress = sample();
        progress.state = ProgressState::Draft;
        assert!(progress.set_state(ProgressState::Done).is_err());
        progress.set_state(ProgressState::Active).unwrap();
        assert!(progress.set_state(ProgressState::Done).is_err());
        progress.set_state(ProgressState::Draft).unwrap();
        progress.set_state(ProgressState::Active).unwrap();
        progress.set_phases(
            progress
                .phases()
                .iter()
                .cloned()
                .map(complete_phase)
                .collect(),
        );
        progress.set_state(ProgressState::Done).unwrap();
        assert!(progress.set_state(ProgressState::Active).is_err());
    }

    fn complete_phase(mut phase: Phase) -> Phase {
        phase.status = PhaseStatus::Completed;
        phase.phases = phase.phases.into_iter().map(complete_phase).collect();
        phase
    }

    #[test]
    fn a_user_edit_blocks_the_next_write_and_hands_back_their_text() {
        crate::home::testing::temp_home();
        let cwd = tempfile::tempdir().unwrap();
        let mut progress = Progress::create(cwd.path(), "Rewrite the resume path").unwrap();
        progress.set_phases(vec![Phase::new("one", PhaseStatus::InProgress)]);
        progress.save().unwrap();
        assert!(progress.reconcile().is_ok());

        std::fs::write(
            progress.path(),
            "---\ntitle: mine\nstate: draft\ncreated: c\n---\n\n## [ ] 1. I disagree\n",
        )
        .unwrap();
        let conflict = progress.reconcile().unwrap_err();
        assert!(conflict.message().contains("I disagree"));
        assert!(conflict.message().contains("authoritative"));
    }

    /// A title nobody wrote in ASCII must still name its own file. Folding the
    /// whole thing away lands every such plan on the `progress` fallback, which
    /// is the directory of interchangeable names this scheme exists to avoid.
    #[test]
    fn a_non_ascii_title_still_names_its_file() {
        assert_eq!(slug("修复工作区测试"), "修复工作区测试");
        assert_eq!(slug("Rewrite the resume path"), "rewrite-the-resume-path");
        assert_eq!(slug("修复 ledger rewind"), "修复-ledger-rewind");
        // Separators and the Windows reserved set are punctuation, so they are
        // still folded out.
        assert_eq!(slug("a/b\\c:d*e?f\"g<h>i|j"), "a-b-c-d-e-f-g-h-i-j");
        assert_eq!(slug("!!!"), "progress");
    }

    #[test]
    fn create_uses_a_timestamped_slug_under_the_project_directory() {
        crate::home::testing::temp_home();
        let cwd = tempfile::tempdir().unwrap();
        let progress = Progress::create(cwd.path(), "Rewrite the resume path").unwrap();
        let name = progress.path().file_name().unwrap().to_string_lossy();
        assert!(name.contains("rewrite-the-resume-path"), "{name}");
        assert!(progress.path().starts_with(store::progress_dir(cwd.path())));
    }

    fn test_ctx() -> crate::tool::ToolCtx {
        crate::home::testing::temp_home();
        let cwd = tempfile::tempdir().unwrap().keep();
        crate::tool::ToolCtx::for_test(cwd, 2_000)
    }

    /// The model tracking its own work needs nobody's approval, so a file it
    /// opens without mentioning `state` is active from birth; only naming
    /// `state` puts the lifecycle up for decision.
    #[test]
    fn an_unstated_state_opens_an_active_file_and_draft_opens_a_draft() {
        let ctx = test_ctx();
        apply_call(
            &ctx,
            &json!({ "title": "Track it", "phases": [{ "phase": "one", "status": "in_progress" }] }),
        )
        .unwrap();
        assert_eq!(
            ctx.progress.lock().unwrap().as_ref().unwrap().state(),
            ProgressState::Active
        );

        let drafted = test_ctx();
        apply_call(
            &drafted,
            &json!({ "title": "Plan it", "state": "draft", "phases": [] }),
        )
        .unwrap();
        assert_eq!(
            drafted.progress.lock().unwrap().as_ref().unwrap().state(),
            ProgressState::Draft
        );
    }

    #[test]
    fn the_first_call_must_name_the_task() {
        let ctx = test_ctx();
        let error = apply_call(&ctx, &json!({ "phases": [] })).unwrap_err();
        assert!(error.contains("`title`"), "{error}");
    }

    /// `state: "active"` is the approval request itself, which is what keeps
    /// `permission()` a pure function of the input.
    #[test]
    fn only_an_active_transition_asks_for_review() {
        assert!(is_submission(&json!({ "state": "active" })));
        assert!(!is_submission(&json!({ "state": "draft" })));
        assert!(!is_submission(&json!({ "phases": [] })));
    }

    /// A submitted draft is written before the human answers, so declining
    /// keeps the file and the next revision replaces it in place. The state
    /// transition is the one thing withheld — it is the question being asked.
    #[test]
    fn review_saves_the_draft_without_promoting_it() {
        let ctx = test_ctx();
        let submit = json!({
            "title": "Rewrite the resume path",
            "state": "active",
            "phases": [{ "phase": "survey the callers", "status": "in_progress",
                         "detail": "read only" }]
        });
        let review = review_copy(&ctx, &submit).unwrap();
        assert!(review[REVIEW_BODY_FIELD]
            .as_str()
            .unwrap()
            .contains("survey the callers"));

        let path = {
            let slot = ctx.progress.lock().unwrap();
            let progress = slot.as_ref().unwrap();
            assert_eq!(progress.state(), ProgressState::Draft, "not yet approved");
            progress.path().to_path_buf()
        };
        assert!(path.exists(), "the draft is durable before it is reviewed");
        let first = path.clone();

        // A revision while still in review replaces the same file.
        review_copy(
            &ctx,
            &json!({ "title": "Rewrite the resume path", "state": "active",
                     "phases": [{ "phase": "survey the callers again", "status": "pending" }] }),
        )
        .unwrap();
        assert_eq!(ctx.progress.lock().unwrap().as_ref().unwrap().path(), first);

        // Approval carries the body the human accepted, rewritten or not.
        let mut approved = review;
        approved[REVIEW_BODY_FIELD] = Value::String("## [ ] 1. what the user wants\n".into());
        apply_call(&ctx, &approved).unwrap();
        let slot = ctx.progress.lock().unwrap();
        let progress = slot.as_ref().unwrap();
        assert_eq!(progress.state(), ProgressState::Active);
        assert_eq!(progress.phases()[0].phase, "what the user wants");
    }

    /// A session can retain a declined draft, but the next task's title must
    /// select a new tracker rather than mutating the abandoned file in place.
    #[test]
    fn a_new_title_replaces_a_declined_draft_without_rewriting_its_file() {
        let ctx = test_ctx();
        let first = review_copy(
            &ctx,
            &json!({
                "title": "Functional test plan",
                "state": "active",
                "phases": [{ "phase": "run tests", "status": "pending" }]
            }),
        )
        .unwrap();
        let first_path = PathBuf::from(first[REVIEW_PATH_FIELD].as_str().unwrap());
        assert!(first_path.exists());

        // The reviewer declined the first draft. It remains selected by the
        // session, just as it does after the ledger is truncated for a fresh
        // conversation, until the following title identifies a new task.
        let second = review_copy(
            &ctx,
            &json!({
                "title": "Fix plan review flow",
                "state": "active",
                "phases": [{ "phase": "write regression", "status": "pending" }]
            }),
        )
        .unwrap();
        let second_path = PathBuf::from(second[REVIEW_PATH_FIELD].as_str().unwrap());

        assert_ne!(second_path, first_path);
        assert!(second_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("fix-plan-review-flow"));
        assert_eq!(
            Progress::load(&first_path).unwrap().title,
            "Functional test plan"
        );
        assert_eq!(
            Progress::load(&second_path).unwrap().title,
            "Fix plan review flow"
        );
        assert_eq!(
            ctx.progress.lock().unwrap().as_ref().unwrap().path(),
            second_path
        );
    }

    /// Rule (2) of the design: a twelve-phase plan keeps one phase's prose in
    /// context, handed over exactly when that phase starts.
    #[test]
    fn the_result_carries_the_entered_phase_detail_and_nothing_else() {
        let ctx = test_ctx();
        let result = apply_call(
            &ctx,
            &json!({ "title": "Ship it", "phases": [
                { "phase": "survey", "status": "completed", "detail": "old news" },
                { "phase": "change it", "status": "in_progress", "detail": "touch ledger.rs" }
            ] }),
        )
        .unwrap();
        assert!(result.contains("touch ledger.rs"), "{result}");
        assert!(!result.contains("old news"), "{result}");
        assert!(result.contains("1/2"), "{result}");
    }

    /// A finished plan stops being the conversation's current one, so the next
    /// task opens its own file instead of appending to a completed one.
    #[test]
    fn done_closes_the_file_and_needs_every_phase_landed() {
        let ctx = test_ctx();
        let unfinished = json!({ "title": "Ship it", "state": "done",
            "phases": [{ "phase": "one", "status": "in_progress" }] });
        assert!(apply_call(&ctx, &unfinished)
            .unwrap_err()
            .contains("every phase is completed"));

        apply_call(
            &ctx,
            &json!({ "state": "done", "phases": [{ "phase": "one", "status": "completed" }] }),
        )
        .unwrap();
        assert!(ctx.progress.lock().unwrap().is_none());
    }

    /// The user's edit wins, and the error hands their text straight back so
    /// the model never spends a `read` finding out what changed.
    #[test]
    fn a_hand_edited_file_blocks_the_next_update_with_its_own_contents() {
        let ctx = test_ctx();
        apply_call(
            &ctx,
            &json!({ "title": "Ship it", "phases": [{ "phase": "one", "status": "pending" }] }),
        )
        .unwrap();
        let path = ctx
            .progress
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .path()
            .to_path_buf();
        std::fs::write(
            &path,
            "---\ntitle: mine\nstate: active\ncreated: c\n---\n\n## [ ] 1. I disagree\n",
        )
        .unwrap();

        let error = apply_call(
            &ctx,
            &json!({ "phases": [{ "phase": "one", "status": "completed" }] }),
        )
        .unwrap_err();
        assert!(error.contains("I disagree"), "{error}");
        assert!(error.contains("authoritative"), "{error}");
    }

    #[test]
    fn inventory_skips_done_and_stale_files_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, state: &str, body: &str| {
            let path = dir.path().join(name);
            std::fs::write(
                &path,
                format!("---\ntitle: {name}\nstate: {state}\ncreated: c\n---\n{body}"),
            )
            .unwrap();
            path
        };
        write("a.md", "active", "\n## [x] 1. one\n\n## [ ] 2. two\n");
        write("b.md", "draft", "\n## [ ] 1. one\n");
        let done = write("c.md", "done", "\n## [x] 1. one\n");
        // Everything is stale seen from far enough ahead; re-dating the files
        // themselves would need a filetime dependency for no extra coverage.
        let entries = inventory_at(dir.path(), SystemTime::now() + STALE_AFTER * 2);
        assert!(entries.is_empty(), "two weeks on, nothing is listed");

        let entries = inventory_at(dir.path(), SystemTime::now());
        let names: Vec<String> = entries.iter().map(|e| e.file_name()).collect();
        assert_eq!(names.len(), 2, "only the unfinished ones: {names:?}");
        assert!(!names.contains(&"c.md".to_string()), "done is archived");
        assert!(done.exists(), "archiving never deletes");
        let a = entries.iter().find(|e| e.file_name() == "a.md").unwrap();
        assert_eq!((a.done, a.total), (1, 2));
    }
}
