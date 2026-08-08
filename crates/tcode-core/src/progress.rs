//! Progress: one durable file per multi-phase task.
//!
//! A progress file is the same object a session used to split between an
//! in-memory `update_progress` list and a reviewed plan draft: an ordered work
//! breakdown. The only differences were durability and whether a human had
//! nodded, so both are properties of one object here — `state` says whether the
//! breakdown has been approved, and the file says it survives the session.
//!
//! **A plan is a document that contains a phase list, not a phase list.** The
//! file therefore has three tiers, each with its own delivery rule:
//!
//! - `description` — one line. What this plan is for. It is what the opening
//!   inventory and `/plan list` show, so relevance can be judged without
//!   opening anything.
//! - `background` — the part of the plan that belongs to no single phase: the
//!   decision and why, the facts investigation established, the shape of the
//!   data, the constraints that hold throughout, the approaches ruled out.
//!   Free markdown, deliberately unschematized. Without it this prose has
//!   nowhere to live but per-phase `detail`, where it is either duplicated
//!   across phases or dropped — and a model that finds no home for it writes
//!   its plan somewhere else entirely.
//! - `phases` — the index into the work, each with its own `detail`.
//!
//! Two invariants make this cheap rather than a verbose markdown edit loop, and
//! both are structural rather than prompt discipline:
//!
//! 1. **The tool is the file's only writer.** The model never `edit`s this
//!    markdown, so a phase flip costs one call instead of two round trips of
//!    full text. [`Progress::reconcile`] is what makes that safe when the user
//!    edits the file by hand.
//! 2. **Prose is written once and delivered on demand.** A twelve-phase plan
//!    keeps exactly one phase's prose in context — the one just entered,
//!    handed back by [`Progress::set_phases`], or the one already running when
//!    a session adopts the file ([`Progress::summary`]). `background` rides
//!    along at the same adoption points, because cross-cutting text has no
//!    narrower trigger than "a session is picking this file up". The resend
//!    that carries phase flips need not carry any of it again
//!    ([`carry_detail`], and the same omit-to-keep rule for `background`),
//!    which is what keeps "write the reasoning down" from being a recurring
//!    charge the model can see and will avoid.
//!
//! A progress file is externally mutable state, not history. The ledger records
//! the tool calls the model made; this file records what is true now. Nothing
//! here touches the append-only invariant.

use std::collections::{HashMap, HashSet};
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

/// How much `background` a summary carries verbatim. Past this the summary
/// carries its section headings and where to get the rest, because a summary is
/// injected at every adoption *and* every compact — a plan whose notes run to
/// twenty kilobytes would otherwise be re-bought each time. Nothing is lost:
/// the pointer names the no-argument call that serves the whole file.
const SUMMARY_BACKGROUND_BUDGET: usize = 8_000;

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

/// `Serialize` is for frontends, not for the model: a desktop review surface
/// edits this structure directly and needs it on the wire. The way back in is
/// [`phases_from_json`], never `Deserialize` — a breakdown arriving from a
/// frontend is data like any other and goes through the same validation the
/// model's own input does.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
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

/// Every phase in a breakdown, parents before children.
fn walk_phases(phases: &[Phase]) -> Vec<&Phase> {
    let mut out = Vec::new();
    for phase in phases {
        phase.walk(&mut out);
    }
    out
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
    /// One line: what this plan is for. The only part of the file the inventory
    /// shows, which is what makes "you need not open it" a real offer.
    pub description: String,
    state: ProgressState,
    created: String,
    /// The plan's own prose — everything that belongs to no single phase.
    /// Rendered above the phases, and everything above the first phase heading
    /// parses back into it, so a hand-written preamble survives.
    background: String,
    phases: Vec<Phase>,
    /// Hash of the bytes last read from or written to `path`. A mismatch means
    /// a human edited the file.
    disk_hash: u64,
    /// Phase titles whose stored `detail` this conversation has actually been
    /// shown. Deliberately not persisted: it is what *this* model knows, not
    /// what the file holds, so a new session starts knowing nothing.
    seen: HashSet<String>,
    /// The same fact about `background`. Separate flag rather than a sentinel
    /// key in `seen`, because a phase title is user-supplied text and any
    /// sentinel is one a phase could be named.
    background_seen: bool,
    /// Hash of the body at the last [`Progress::full_view`], so asking again
    /// for a plan that has not moved costs a pointer instead of a copy. Same
    /// contract as `read`'s freshness check, for the same reason.
    viewed: Option<u64>,
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
            description: String::new(),
            state: ProgressState::Draft,
            created: timestamp_rfc3339(SystemTime::now()),
            background: String::new(),
            phases: Vec::new(),
            // No file yet: any hash that cannot match an existing file's would
            // do, but 0 also means "never written", which `reconcile` needs.
            disk_hash: 0,
            seen: HashSet::new(),
            background_seen: false,
            viewed: None,
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
        let (background, phases) = parse_body(body);
        Ok(Self {
            path: path.to_path_buf(),
            title,
            description: front.description.trim().to_string(),
            state: ProgressState::parse(&front.state).unwrap_or_default(),
            created: front.created,
            background,
            phases,
            disk_hash: fnv1a(text.as_bytes()),
            // Parsing a file is not reading it: the text went to disk, not to
            // the model. Whoever hands some of it over marks that much seen.
            seen: HashSet::new(),
            background_seen: false,
            viewed: None,
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

    /// The plan's prose: the part that belongs to no single phase.
    pub fn background(&self) -> &str {
        &self.background
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
    /// description, the prose and the breakdown. The title chooses the progress
    /// file before this method runs; it is never mutable content of an existing
    /// tracker. Split out from [`Progress::apply`] because a draft submitted for
    /// approval is saved *before* the human answers — their decision is about
    /// `state`, and applying it up front would make declining silently promote
    /// the plan.
    ///
    /// `background` follows the same omit-to-keep rule as a phase's `detail`,
    /// and for the same reason: the breakdown is resent in full on every phase
    /// flip, so prose that had to ride along would be paid for on every call
    /// and the model would learn to stop writing it. Everything is validated
    /// before anything is applied, so one refusal reports every blind rewrite.
    pub fn apply_content(&mut self, input: &Value) -> Result<Option<String>, String> {
        let background = input["background"].as_str().map(str::trim);
        let phases = match input.get("phases") {
            Some(phases) if !phases.is_null() => Some(phases_from_json(phases)?),
            _ => None,
        };
        let stored = self.stored_detail();
        self.refuse_blind_rewrites(phases.as_deref().unwrap_or(&[]), &stored, background)?;
        if let Some(description) = input["description"]
            .as_str()
            .map(str::trim)
            .filter(|description| !description.is_empty())
        {
            self.description = description.to_string();
        }
        if let Some(background) = background.filter(|text| !text.is_empty()) {
            self.background = background.to_string();
            self.background_seen = true;
        }
        match phases {
            Some(phases) => self.set_phases(phases),
            None => Ok(None),
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
    ///
    /// Detail is the one field a resend does *not* have to carry: a phase sent
    /// without it keeps what the file already holds. See [`carry_detail`] —
    /// that rule is what makes writing detail worth doing at all.
    ///
    /// Rejects a rewrite of detail this conversation was never shown, the same
    /// bargain the file tools strike over `edit`: overwriting prose you have
    /// not read is not an edit, it is a guess. The error hands the text over,
    /// which is also what makes the retry legal.
    pub fn set_phases(&mut self, mut phases: Vec<Phase>) -> Result<Option<String>, String> {
        let stored = self.stored_detail();
        self.refuse_blind_rewrites(&phases, &stored, None)?;
        let was_running = self.running_titles();
        carry_detail(&mut phases, &stored);
        self.phases = phases;
        // Every phase whose prose the model just wrote out, it has by
        // definition seen; the one it is handed below joins them.
        let written: Vec<String> = self
            .flatten()
            .iter()
            .filter(|phase| !phase.detail.is_empty())
            .map(|phase| phase.phase.clone())
            .collect();
        self.seen.extend(written);
        let entered = self
            .flatten()
            .into_iter()
            .find(|phase| {
                phase.status == PhaseStatus::InProgress && !was_running.contains(&phase.phase)
            })
            .filter(|phase| !phase.detail.is_empty())
            .map(|phase| format!("{}\n{}", phase.phase, phase.detail));
        Ok(entered)
    }

    /// Refuse to replace stored prose this conversation has not been shown, and
    /// hand it over in the error so the next attempt is an informed one.
    /// Reporting every offending passage at once — `background` included — is
    /// why this runs before anything is applied: one round trip beats N.
    fn refuse_blind_rewrites(
        &mut self,
        incoming: &[Phase],
        stored: &HashMap<String, String>,
        background: Option<&str>,
    ) -> Result<(), String> {
        let blind_background = background
            .filter(|text| !text.is_empty())
            .filter(|text| {
                !self.background_seen && !self.background.is_empty() && **text != self.background
            })
            .map(|_| self.background.clone());
        let blind: Vec<(String, String)> = walk_phases(incoming)
            .into_iter()
            .filter(|phase| !phase.detail.is_empty() && !self.seen.contains(&phase.phase))
            .filter_map(|phase| {
                let held = stored.get(&phase.phase)?;
                (*held != phase.detail).then(|| (phase.phase.clone(), held.clone()))
            })
            .collect();
        if blind.is_empty() && blind_background.is_none() {
            return Ok(());
        }
        let mut error = String::from(
            "Nothing was applied: this plan already holds text that was written outside this conversation, and you have not been shown it. Here it is. Re-send with those fields left out to keep them, or with your replacement if you still mean to replace them.\n",
        );
        if let Some(held) = blind_background {
            error.push_str(&format!(
                "\n<tcode-progress-background>\n{}\n</tcode-progress-background>\n",
                held.trim_end()
            ));
            self.background_seen = true;
        }
        for (phase, detail) in blind {
            error.push_str(&format!(
                "\n<tcode-progress-detail phase=\"{}\">\n{}\n</tcode-progress-detail>\n",
                escape_attr(&phase),
                detail.trim_end()
            ));
            self.seen.insert(phase);
        }
        Err(error)
    }

    /// The detail this file already holds, keyed by phase title. First writer
    /// wins for a duplicated title: the alternative is picking a loser, and no
    /// choice there is better than the earlier one.
    fn stored_detail(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for phase in self.flatten() {
            if !phase.detail.is_empty() {
                out.entry(phase.phase.clone())
                    .or_insert_with(|| phase.detail.clone());
            }
        }
        out
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
    ///
    /// Rejecting is only half of "their version wins" — the other half is
    /// adopting it here, before returning. Without that the handle keeps
    /// comparing against the bytes it wrote long ago, so every later call
    /// re-reports the same conflict and the tool is wedged for the rest of the
    /// session, while the error tells the model to do the one thing that
    /// cannot work. Their text is also handed to the model in the same breath,
    /// so every detail in it counts as seen.
    pub fn reconcile(&mut self) -> Result<(), Conflict> {
        // Never written: there is nothing of the user's to protect. A file that
        // appeared under this path in the meantime is still theirs, though.
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Ok(());
        };
        if fnv1a(text.as_bytes()) == self.disk_hash {
            return Ok(());
        }
        if let Ok(theirs) = Self::parse(&self.path, &text) {
            let seen = theirs.flatten().iter().map(|p| p.phase.clone()).collect();
            *self = theirs;
            self.seen = seen;
            self.background_seen = true;
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
        let mut out = format!("---\ntitle: {}\n", yaml_scalar(&self.title));
        // Written only when there is one, so a file that predates descriptions
        // still round-trips byte for byte.
        if !self.description.is_empty() {
            out.push_str(&format!(
                "description: {}\n",
                yaml_scalar(&self.description)
            ));
        }
        out.push_str(&format!(
            "state: {}\ncreated: {}\n---\n",
            self.state.label(),
            self.created
        ));
        out.push_str(&self.body());
        out
    }

    /// The plan as markdown, without the front matter: the prose, then the
    /// phases. This is what a review pane shows and what `$EDITOR` round-trips
    /// — the lifecycle belongs to the approval buttons, so it is not put in
    /// front of the reviewer's cursor, but everything they are reviewing is.
    pub fn body(&self) -> String {
        render_document(&self.background, &self.phases)
    }

    /// Adopt a human-authored body verbatim. The inverse of [`Progress::body`],
    /// used for the version a reviewer approved after rewriting it. Prose they
    /// added above the first phase comes back as `background` rather than being
    /// dropped on the next write.
    pub fn set_body(&mut self, markdown: &str) {
        let (background, phases) = parse_body(markdown);
        self.background = background;
        self.phases = phases;
    }

    /// The model-facing summary: the plan's prose, title lines and boxes, plus
    /// the detail of the phase currently `[>]` — and no other phase's.
    ///
    /// `background` rides along because every caller of this is a session
    /// picking the file up, and the prose that belongs to no phase has no
    /// narrower moment to arrive at than that. Past
    /// [`SUMMARY_BACKGROUND_BUDGET`] it degrades to its section headings and a
    /// pointer at the no-argument call, so a very long plan costs a pointer per
    /// compact rather than its full weight.
    ///
    /// The one-phase-at-a-time budget is the point, not the omission. This is
    /// injected when a session takes the file over (new session, resume,
    /// compact, a user edit), and the phase already running at that moment is
    /// the one case where the hand-back on entry never comes: it was entered by
    /// someone else, in a conversation this model cannot see. Withholding it
    /// here does not save the prose for later — it drops it.
    ///
    /// Takes `&mut self` because handing prose to the model is a fact about
    /// what this conversation now knows, and that fact is what later lets it
    /// rewrite the phase it is standing on.
    /// This also *resets* what the conversation is taken to know, because every
    /// caller of it is a moment the prose it was handed before is gone or was
    /// never theirs: a new session, a resume, a compact that dropped the text,
    /// a user rewrite. What it knows afterwards is what this summary carries.
    pub fn summary(&mut self) -> String {
        let running: Vec<String> = self
            .flatten()
            .into_iter()
            .filter(|phase| phase.status == PhaseStatus::InProgress && !phase.detail.is_empty())
            .map(|phase| phase.phase.clone())
            .collect();
        self.seen = running.into_iter().collect();
        self.viewed = None;
        let (background, whole) = self.background_for_summary();
        self.background_seen = whole;
        let mut out = String::new();
        if !background.is_empty() {
            out.push_str(&background);
            out.push_str("\n\n");
        }
        out.push_str(&self.summary_body());
        self.envelope(&out)
    }

    /// The prose a summary carries, and whether that is all of it. Over budget,
    /// the section headings are the useful shape — they say what is in the file
    /// so the model can decide to go and read it; with no headings to list, a
    /// prefix is the only thing left to offer.
    fn background_for_summary(&self) -> (String, bool) {
        if self.background.len() <= SUMMARY_BACKGROUND_BUDGET {
            return (self.background.clone(), !self.background.is_empty());
        }
        let headings: Vec<String> = self
            .background
            .lines()
            .filter(|line| line.starts_with('#'))
            .map(|line| line.trim_start_matches('#').trim().to_string())
            .filter(|title| !title.is_empty())
            .collect();
        let mut out = String::new();
        if headings.is_empty() {
            let head: String = self.background.chars().take(600).collect();
            out.push_str(head.trim_end());
            out.push('\n');
        } else {
            out.push_str("This plan's notes, by section:\n");
            for heading in headings {
                out.push_str(&format!("- {heading}\n"));
            }
        }
        out.push_str("(Shortened. Call `progress` with no arguments to read the plan in full.)");
        (out, false)
    }

    /// The whole file as the model reads it: every phase, every detail. The
    /// deliberately expensive one — nothing injects this, the model asks for it
    /// when it wants the plan rather than the checklist.
    ///
    /// Asking twice for a plan that has not moved gets a pointer to the copy
    /// already in context, exactly as a repeated `read` does. The stale-copy
    /// worry that would argue against it is handled at the other end:
    /// [`Progress::summary`] runs at precisely the moments the earlier copy
    /// stops being reachable, and clears this.
    pub fn full_view(&mut self) -> String {
        let body = self.body();
        let hash = fnv1a(body.as_bytes());
        if self.viewed == Some(hash) {
            return format!(
                "unchanged: {} has not changed since you read it in full; the plan is already in your context above.",
                self.path.display()
            );
        }
        let all: Vec<String> = self
            .flatten()
            .into_iter()
            .map(|phase| phase.phase.clone())
            .collect();
        self.seen.extend(all);
        self.background_seen = !self.background.is_empty();
        self.viewed = Some(hash);
        self.envelope(body.trim_start_matches('\n'))
    }

    /// One tag for both views, so the model never has to work out which shape
    /// of progress text it is looking at. `description` rides in the attributes
    /// rather than the body: the body is the reviewable markdown that
    /// [`Progress::set_body`] parses back, and a line that is not part of the
    /// plan's prose has no business in it.
    fn envelope(&self, contents: &str) -> String {
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let description = match self.description.trim() {
            "" => String::new(),
            text => format!(" description=\"{}\"", escape_attr(text)),
        };
        format!(
            "<tcode-progress file=\"{}\" title=\"{}\"{} state=\"{}\">\n{}\n</tcode-progress>",
            name,
            escape_attr(&self.title),
            description,
            self.state.label(),
            contents.trim_end()
        )
    }

    fn summary_body(&self) -> String {
        let mut out = String::new();
        for (i, phase) in self.phases.iter().enumerate() {
            summarize_phase(&mut out, phase, &format!("{}", i + 1), 0);
        }
        out
    }
}

/// Carry stored detail onto a resent breakdown, matching by phase title. A
/// phase resent without `detail` means "unchanged", never "erased".
///
/// Without this rule the tool's own economics argue against ever writing
/// detail: the breakdown is resent in full on every phase flip, so prose paid
/// for once would be paid for again on every call, while its only payoff is
/// the single hand-back when that phase starts. Worse, a fresh session is
/// handed titles and boxes alone — it would resend the plan it can see and
/// silently wipe the reasoning it never saw, which is exactly the reader this
/// detail was written for.
fn carry_detail(phases: &mut [Phase], stored: &HashMap<String, String>) {
    for phase in phases {
        if phase.detail.is_empty() {
            if let Some(detail) = stored.get(&phase.phase) {
                phase.detail = detail.clone();
            }
        }
        carry_detail(&mut phase.phases, stored);
    }
}

/// A whole plan as the markdown a reader sees: the prose, then the breakdown.
/// Free-standing so a frontend can render a plan straight out of a tool call,
/// before any file exists for it.
///
/// Prose first and phases last is the machine's order and the natural one at
/// once: [`parse_body`] reads everything above the first boxed heading as the
/// prose, so a section appended below the checklist would be read as the last
/// phase's detail. Rendering this way keeps a round trip through `$EDITOR`
/// meaning what it looks like it means.
pub fn render_document(background: &str, phases: &[Phase]) -> String {
    let mut out = String::new();
    let background = background.trim();
    if !background.is_empty() {
        out.push('\n');
        out.push_str(background);
        out.push('\n');
    }
    out.push_str(&render_phases(phases));
    out
}

/// A breakdown as the markdown a reader sees, without the prose above it.
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
    if phase.status == PhaseStatus::InProgress {
        for line in phase.detail.lines() {
            out.push_str(&"  ".repeat(indent + 1));
            out.push_str(line);
            out.push('\n');
        }
    }
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

/// The plan body this call carries for a human to read, if it carries one.
///
/// The review copy's saved body wins over the call's own phases because it is
/// the text the reviewer actually saw — including a rewrite they made. A
/// retired `exit_plan` call in a resumed session carries the same field, which
/// is why this is checked before the submission test rather than after it.
pub fn plan_document(input: &Value) -> Option<String> {
    if let Some(body) = input[REVIEW_BODY_FIELD].as_str() {
        if !body.trim().is_empty() {
            return Some(body.to_string());
        }
    }
    if !is_submission(input) {
        return None;
    }
    let phases = phases_from_json(&input["phases"]).ok()?;
    let background = input["background"].as_str().unwrap_or("");
    Some(render_document(background, &phases)).filter(|body| !body.trim().is_empty())
}

/// Whether this call is a plan for a human to read rather than a phase flip.
/// The two are the same tool, and only the input says which happened, so every
/// surface that has to tell them apart asks here.
pub fn is_plan_document(input: &Value) -> bool {
    plan_document(input).is_some()
}

/// Turn a reviewer's edited breakdown into the plan body to execute, keeping the
/// detail they left alone.
///
/// A review surface that edits phases as structure — the desktop's does — sends
/// back what the reviewer changed, and a phase they did not open carries no
/// `detail`. Rendering that directly would erase the reasoning behind every
/// phase they never looked at, which is the single most valuable text in the
/// file. So the same rule the tool gives the model applies to the human:
/// omitting detail keeps what is stored, and only text they actually wrote
/// replaces it.
///
/// `body` is the plan as it stands (the review copy's own body), because that,
/// not the ledger, is what the stored detail is being carried from.
///
/// The plan's prose is carried the same way and for a stronger reason: a review
/// surface that edits phases as structure has no way to send it back at all, so
/// re-rendering from the phases alone would delete it outright.
pub fn revise_plan_body(body: &str, edited: &[Phase]) -> String {
    let (background, existing) = parse_body(body);
    let stored: HashMap<String, String> = walk_phases(&existing)
        .into_iter()
        .filter(|phase| !phase.detail.is_empty())
        .map(|phase| (phase.phase.clone(), phase.detail.clone()))
        .fold(HashMap::new(), |mut out, (title, detail)| {
            out.entry(title).or_insert(detail);
            out
        });
    let mut edited = edited.to_vec();
    carry_detail(&mut edited, &stored);
    render_document(&background, &edited)
}

/// One reviewer comment, anchored to the passage it is about.
///
/// The quote travels with the comment because it is the anchor the *model*
/// reads: a character offset into a plan the model is about to rewrite tells it
/// nothing, while the passage commented on stays unambiguous even after the
/// text around it moves. Both review surfaces produce exactly this — the TUI
/// from block navigation or a dragged passage, the desktop from a text
/// selection.
#[derive(Debug, Clone)]
pub struct PlanNote {
    /// The passage this is about. `None` comments on the plan as a whole.
    pub quote: Option<String>,
    pub text: String,
}

/// Reviewer comments as the model reads them: each quoted passage followed by
/// what the human said about it, then any free-form remark.
///
/// This lives here, and not in either frontend, because two of them produce it.
/// The model has learned this format from whichever surface it saw first; the
/// other one arriving with a different shape would be two definitions of one
/// contract. Empty comments are dropped rather than sent as blank quotes.
pub fn plan_notes(notes: &[PlanNote], free: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for note in notes {
        let text = note.text.trim();
        if text.is_empty() {
            continue;
        }
        match note
            .quote
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
        {
            Some(quote) => parts.push(format!("{}\n\n{text}", quote_lines(quote))),
            None => parts.push(text.to_string()),
        }
    }
    let free = free.trim();
    if !free.is_empty() {
        parts.push(free.to_string());
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// The approval note for a plan the reviewer accepted. A rewrite leads, because
/// the model is about to execute a plan it did not write and this note is the
/// only place it learns which text won; the comments follow.
pub fn approved_plan_note(revised: Option<&str>, notes: &[PlanNote], free: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(revised) = revised.map(str::trim).filter(|body| !body.is_empty()) {
        parts.push(format!(
            "The user edited the plan before approving. Use this revised plan as the source of truth for execution, not the earlier draft:\n\n{revised}"
        ));
    }
    if let Some(notes) = plan_notes(notes, free) {
        parts.push(notes);
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// The note a declined ("keep planning") review sends back. An edit travels as
/// a diff rather than a whole body: the plan on disk is still the model's own
/// draft, so what the model needs is what the human would change about it.
pub fn declined_plan_note(diff: Option<&str>, notes: &[PlanNote], free: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(diff) = diff.map(str::trim).filter(|diff| !diff.is_empty()) {
        parts.push(format!("The user edited the plan:\n\n{diff}"));
    }
    if let Some(notes) = plan_notes(notes, free) {
        parts.push(notes);
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// A unified diff from the plan the model submitted to the one the reviewer
/// wrote. `None` when nothing meaningful changed — an empty diff is not
/// feedback, and sending one would tell the model to look for a change it
/// cannot find.
pub fn plan_revision_diff(original: &str, revised: &str) -> Option<String> {
    let (original, revised) = (original.trim(), revised.trim());
    if original == revised {
        return None;
    }
    let diff = similar::TextDiff::from_lines(original, revised);
    Some(
        diff.unified_diff()
            .context_radius(3)
            .header("plan", "revised")
            .to_string(),
    )
}

fn quote_lines(source: &str) -> String {
    source
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether this call only asks to see the file. A call that carries neither a
/// breakdown nor a lifecycle change has nothing to apply, so the honest reading
/// of it is "show me", not "rewrite the file with what it already says".
fn is_view(input: &Value) -> bool {
    let absent = |field: &str| input.get(field).is_none_or(Value::is_null);
    absent("phases") && absent("state") && absent("background") && absent("description")
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
    if let Some(error) = submission_gaps(progress) {
        return Err(error);
    }
    let mut review = input.clone();
    review[REVIEW_BODY_FIELD] = Value::String(progress.body());
    review[REVIEW_PATH_FIELD] = Value::String(progress.path().display().to_string());
    Ok(review)
}

/// Whether a `state: "active"` call re-submits the plan this session already
/// holds as active. The approval dialog was answered (or never needed) when
/// that file became active, so asking again would be a second approval of one
/// document — the common shape is a phase flip that re-carries the previous
/// call's `state: "active"` together with the resent breakdown. A genuinely
/// revised plan goes back through `draft` first (the lifecycle's own arrow,
/// `Active -> Draft -> Active`), so a submission of a *draft* still asks.
pub fn is_redundant_submission(ctx: &crate::tool::ToolCtx, input: &Value) -> bool {
    is_submission(input)
        && ctx
            .progress
            .lock()
            .expect("progress lock")
            .as_ref()
            .is_some_and(|progress| progress.state() == ProgressState::Active)
}

/// What a submitted plan is missing, as the error refusing it.
///
/// A draft is the one shape of progress file whose reader is someone other than
/// the conversation that wrote it: the person about to approve it, and often a
/// session handed the file and nothing else. That reader is the reason all
/// three of these fields exist, so submission is the one moment where their
/// absence is a defect rather than a budget decision — everywhere else a bare
/// phase list is a model tracking work it can still see. Enforced here rather
/// than asked for in the tool description because the description is what the
/// model economizes on: prose costs output tokens now and pays a reader it will
/// never meet.
///
/// Every gap in one message: the model would otherwise pay a round trip per
/// field to discover a checklist we already hold.
///
/// Detail is demanded of top-level phases only. A sub-phase is read inside its
/// parent's detail, and demanding prose under each of them buys repetition, not
/// context.
fn submission_gaps(progress: &Progress) -> Option<String> {
    let mut gaps: Vec<String> = Vec::new();
    if progress.description.trim().is_empty() {
        gaps.push(
            "`description` is empty. One line saying what this plan is for — it is all the opening inventory and `/plan list` show, so it is what decides whether anyone opens the file at all."
                .to_string(),
        );
    }
    if progress.background().trim().is_empty() {
        gaps.push(
            "`background` is empty. A phase list is an index, not a plan: what you decided and why, the facts your investigation established, the shape of the data, the constraints that hold throughout, and the approaches you ruled out belong to no single phase, and without them the executor re-derives all of it."
                .to_string(),
        );
    }
    let missing: Vec<&str> = progress
        .phases()
        .iter()
        .filter(|phase| phase.detail.trim().is_empty())
        .map(|phase| phase.phase.as_str())
        .collect();
    if !missing.is_empty() {
        gaps.push(format!(
            "these phases have no `detail` — {}. Each needs the files and symbols it touches, what you found in them that decides the approach, why it comes at this point, and what could break.",
            missing.join(", ")
        ));
    }
    if gaps.is_empty() {
        return None;
    }
    Some(format!(
        "Not submitted, and the user was not asked. A plan under review is read by someone who was not in this conversation, and may be executed by a session that has this file and nothing else, so:\n\n- {}\n\nRe-send with that written out. Fields that are already filled in can be left out to keep what they hold.",
        gaps.join("\n- ")
    ))
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
    // Nothing to apply is a request to look: the model has no other way to read
    // a file this tool owns, and reading it should not cost a guessed path or a
    // second tool. Every other call answers with counts, so the plan itself has
    // to be askable for.
    if is_view(input) {
        return Ok(progress.full_view());
    }
    let was_active = progress.state() == ProgressState::Active;
    let submitted = is_submission(input).then(|| progress.body());
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
    // An approval is a go-ahead, and this result is the only thing the model
    // reads after it. Left as a bare phase count it reads like the end of the
    // task it was asked to do — draft a plan, submit it — so the model stops
    // exactly where the user expected it to start. (The other approval route,
    // handing the plan to a fresh session, says this in that session's opening
    // instruction; this is the same sentence for staying put.) A redundant
    // re-submission of an already-active plan is not a fresh approval, so it
    // does not get the nudge either.
    if submitted.is_some() && !was_active {
        result.push_str(
            "\n\nApproved — you are cleared to execute this plan now. Start with its first phase: mark it in_progress in your next `progress` call and begin the work.",
        );
    }
    if let Some(entered) = entered {
        result.push_str("\n\nNow starting — ");
        result.push_str(&entered);
    }
    // An approval the reviewer rewrote is the one moment the file says
    // something the model never wrote and would otherwise never read. It is
    // about to execute this; handing it back here costs one echo, and skipping
    // it means executing the plan it proposed rather than the one approved.
    if submitted.is_some_and(|before| before != progress.body()) {
        result.push_str(
            "\n\nThe user rewrote this plan before approving it. Theirs is the one to execute:\n\n",
        );
        result.push_str(&progress.full_view());
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
        .as_mut()
        .map(Progress::summary)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FrontMatter {
    title: String,
    description: String,
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

/// Body → `(background, phases)`. Phase headings carry the box and an optional
/// `1.` / `1.2` number (rendered by us, ignored on the way back in so a user may
/// renumber freely); everything between headings is the previous phase's detail.
///
/// Everything *above* the first phase heading is the plan's own prose. It used
/// to be discarded, which meant a preamble a user or a reviewer wrote was
/// silently deleted by the next phase flip — the one thing `reconcile`'s "their
/// version wins" is supposed to prevent.
fn parse_body(body: &str) -> (String, Vec<Phase>) {
    let mut background = String::new();
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
        let target = match detail_of {
            Some((parent, Some(child))) => &mut top[parent].phases[child].detail,
            Some((parent, None)) => &mut top[parent].detail,
            None => &mut background,
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
    (background.trim().to_string(), top)
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
    /// The plan's own one-liner. The whole point of the listing tier: a title
    /// says which task, this says whether it is the one you are looking for.
    pub description: String,
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
                description: progress.description,
                state: progress.state,
                done,
                total,
                modified,
            })
        })
        .collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.modified));
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
        // The one line that lets this be a listing rather than a set of file
        // names: whether a plan is worth opening is decided here, not by
        // reading it.
        if !entry.description.is_empty() {
            out.push_str(&format!("  {}\n", entry.description));
        }
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
            description: "让 rewind 跨过 compact 边界仍然成立".into(),
            state: ProgressState::Active,
            created: "2026-07-29T10:12:00Z".into(),
            background: "## 决策\n值得做：rewind 目前在 Summary 边界上静默截断。".into(),
            phases: Vec::new(),
            disk_hash: 0,
            seen: HashSet::new(),
            background_seen: false,
            viewed: None,
        };
        let _ = progress.set_phases(vec![
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
        assert_eq!(parsed.description, original.description);
        assert_eq!(parsed.state(), original.state());
        assert_eq!(parsed.background(), original.background());
        assert_eq!(parsed.phases(), original.phases());
        assert_eq!(parsed.render(), text);
    }

    /// A file written before descriptions existed still round-trips byte for
    /// byte, which is what keeps `reconcile` from reading our own rewrite as a
    /// user edit.
    #[test]
    fn a_file_without_a_description_round_trips_unchanged() {
        let text = "---\ntitle: \"t\"\nstate: active\ncreated: c\n---\n\n## [ ] 1. one\n";
        let parsed = Progress::parse(Path::new("/tmp/p.md"), text).unwrap();
        assert!(parsed.description.is_empty());
        assert!(parsed.background().is_empty());
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

    /// The prose above the checklist is the plan, not litter: it used to be
    /// discarded on parse, so a preamble a user or a reviewer wrote was deleted
    /// by the next phase flip — the one thing "their version wins" forbids.
    #[test]
    fn a_heading_without_a_box_is_prose_not_a_phase() {
        let text = "---\ntitle: \"t\"\ndescription: \"what it is for\"\nstate: draft\ncreated: c\n---\n\n## Background\nnot a phase\n\n## [ ] 1. Real\nwhy\n";
        let parsed = Progress::parse(Path::new("/tmp/p.md"), text).unwrap();
        assert_eq!(parsed.description, "what it is for");
        assert_eq!(parsed.phases().len(), 1);
        assert_eq!(parsed.phases()[0].phase, "Real");
        assert_eq!(parsed.background(), "## Background\nnot a phase");
        assert_eq!(parsed.render(), text, "and it survives being written back");
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
        assert!(progress.set_phases(phases).unwrap().is_none());

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
            .unwrap()
            .expect("newly entered phase carries its detail");
        assert!(entered.contains("迁移调用方"));
        assert!(entered.contains("ledger.rs"));
    }

    #[test]
    fn summary_carries_boxes_and_only_the_running_phase_detail() {
        let summary = sample().summary();
        assert!(summary.contains("[x] 1. 勘查调用面"));
        assert!(summary.contains("← current"));
        assert!(summary.contains("state=\"active\""));
        assert!(summary.contains("description=\"让 rewind"), "{summary}");
        assert!(
            summary.contains("Summary 边界上静默截断"),
            "the prose that belongs to no phase arrives when a session picks the file up: {summary}"
        );
        assert!(
            summary.contains("风险：compact"),
            "the running phase's detail is the one a resuming session cannot get any other way: {summary}"
        );
        assert!(
            !summary.contains("aux 事件"),
            "every other phase's detail must stay out of the summary: {summary}"
        );
    }

    /// The resume path in one test: a fresh session is handed the summary,
    /// which names the phases but carries no detail for the ones it is not
    /// running. Resending that — the only breakdown it can see — must not cost
    /// the file its reasoning.
    #[test]
    fn resending_titles_alone_keeps_the_stored_detail() {
        let mut progress = sample();
        let titles: Vec<Phase> = progress
            .phases()
            .iter()
            .map(|phase| Phase {
                phase: phase.phase.clone(),
                status: phase.status,
                detail: String::new(),
                phases: phase
                    .phases
                    .iter()
                    .map(|child| Phase::new(child.phase.clone(), child.status))
                    .collect(),
            })
            .collect();
        progress.set_phases(titles).unwrap();
        assert_eq!(
            progress.phases()[0].detail,
            "只读。确认 rewind 之后 aux 事件的重放顺序。"
        );
        assert!(progress.phases()[1].detail.starts_with("风险："));
    }

    #[test]
    fn a_resent_phase_may_still_rewrite_its_own_detail() {
        let mut progress = sample();
        progress
            .set_phases(vec![Phase {
                phase: "勘查调用面".into(),
                status: PhaseStatus::Completed,
                detail: "改主意了".into(),
                phases: Vec::new(),
            }])
            .unwrap();
        assert_eq!(progress.phases()[0].detail, "改主意了");
    }

    /// Detail carried forward is detail the model never re-read, so entering a
    /// phase a previous session wrote still hands back that session's prose.
    #[test]
    fn entering_a_phase_hands_back_detail_written_by_someone_else() {
        let mut progress = sample();
        let entered = progress
            .set_phases(vec![
                Phase::new("勘查调用面", PhaseStatus::Completed),
                Phase::new("改 truncate_tail 的归档语义", PhaseStatus::Completed),
                Phase::new("迁移调用方", PhaseStatus::InProgress),
            ])
            .unwrap()
            .is_none();
        assert!(entered, "a phase with no stored detail hands back nothing");

        let mut progress = sample();
        progress
            .set_phases(vec![
                Phase::new("勘查调用面", PhaseStatus::Completed),
                Phase::new("改 truncate_tail 的归档语义", PhaseStatus::Pending),
            ])
            .unwrap();
        let entered = progress
            .set_phases(vec![
                Phase::new("勘查调用面", PhaseStatus::Completed),
                Phase::new("改 truncate_tail 的归档语义", PhaseStatus::InProgress),
            ])
            .unwrap()
            .expect("stored detail is handed back on entry");
        assert!(entered.contains("跨越 Summary 边界"), "{entered}");
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
        progress
            .set_phases(
                progress
                    .phases()
                    .iter()
                    .cloned()
                    .map(complete_phase)
                    .collect(),
            )
            .unwrap();
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
        progress
            .set_phases(vec![Phase::new("one", PhaseStatus::InProgress)])
            .unwrap();
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

    /// A `state: "active"` call for a plan this session already holds as active
    /// is not a new submission — the approval was answered (or never needed)
    /// when that file became active, so re-asking would be a second approval of
    /// one document. The agent loop uses this to skip the dialog; a draft, a
    /// session with no file open, and a call without `state` all still ask or
    /// never were submissions.
    #[test]
    fn a_resubmission_of_an_active_plan_is_redundant() {
        // A tracker the model opened without `state` is active from birth;
        // re-submitting it as active is redundant.
        let ctx = test_ctx();
        apply_call(
            &ctx,
            &json!({ "title": "Track it", "phases": [{ "phase": "one", "status": "in_progress" }] }),
        )
        .unwrap();
        assert!(is_redundant_submission(
            &ctx,
            &json!({ "state": "active", "phases": [] })
        ));

        // A draft has not been approved yet; its submission must still ask.
        let drafted = test_ctx();
        apply_call(
            &drafted,
            &json!({ "title": "Plan it", "state": "draft", "phases": [] }),
        )
        .unwrap();
        assert!(!is_redundant_submission(
            &drafted,
            &json!({ "state": "active", "phases": [] })
        ));

        // Nothing is open yet: there is no approved file to be redundant about.
        let fresh = test_ctx();
        assert!(!is_redundant_submission(
            &fresh,
            &json!({ "title": "Other", "state": "active" })
        ));

        // A call that never names `state: "active"` is not a submission at all.
        let tracking = test_ctx();
        apply_call(&tracking, &json!({ "title": "Track it", "phases": [] })).unwrap();
        assert!(!is_redundant_submission(
            &tracking,
            &json!({ "phases": [] })
        ));
    }

    /// A submitted draft is written before the human answers, so declining
    /// keeps the file and the next revision replaces it in place. The state
    /// transition is the one thing withheld — it is the question being asked.
    #[test]
    fn review_saves_the_draft_without_promoting_it() {
        let ctx = test_ctx();
        let submit = json!({
            "title": "Rewrite the resume path",
            "description": "make resume replay aux events in order",
            "background": "## 决策\nWorth doing.",
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
                     "phases": [{ "phase": "survey the callers again", "status": "pending",
                                  "detail": "read only" }] }),
            // description and background are left out on purpose: the file keeps
            // what it holds, exactly as `detail` does.
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

    /// The two tiers of detail, which are a function of `state` and not of the
    /// model's read on the task: a plan submitted for review is read by a human
    /// and possibly executed by a session with nothing but the file, so a phase
    /// with nothing written under it bounces before the reviewer is asked
    /// anything; a file tracking this conversation's own work never is.
    #[test]
    fn a_submission_needs_detail_under_every_phase() {
        let ctx = test_ctx();
        let submit = |migrate: Value| {
            json!({
                "title": "Rewrite the resume path",
                "description": "make resume replay aux events in order",
                "background": "## 决策\nWorth doing: resume replays out of order.",
                "state": "active",
                "phases": [
                    { "phase": "survey the callers", "status": "pending", "detail": "read only" },
                    migrate
                ]
            })
        };
        let error = review_copy(
            &ctx,
            &submit(json!({ "phase": "migrate them", "status": "pending" })),
        )
        .unwrap_err();
        assert!(error.contains("migrate them"), "{error}");
        assert!(!error.contains("survey the callers"), "{error}");

        // The retry is not a blind rewrite: nothing was ever stored under it.
        review_copy(
            &ctx,
            &submit(json!({ "phase": "migrate them", "status": "pending",
                            "detail": "callers live in session.rs" })),
        )
        .unwrap();

        // Tracking your own work is not a submission, and is not gated.
        apply_call(
            &ctx,
            &json!({ "title": "Ship it",
                     "phases": [{ "phase": "do it", "status": "in_progress" }] }),
        )
        .unwrap();
    }

    /// The other two tiers are gated at the same moment and for the same
    /// reader, and every gap is reported at once rather than one per round trip.
    #[test]
    fn a_submission_needs_a_description_and_the_prose_no_phase_holds() {
        let ctx = test_ctx();
        let error = review_copy(
            &ctx,
            &json!({
                "title": "Rewrite the resume path",
                "state": "active",
                "phases": [{ "phase": "survey the callers", "status": "pending",
                             "detail": "read only" }]
            }),
        )
        .unwrap_err();
        assert!(error.contains("`description` is empty"), "{error}");
        assert!(error.contains("`background` is empty"), "{error}");

        review_copy(
            &ctx,
            &json!({
                "title": "Rewrite the resume path",
                "description": "make resume replay aux events in order",
                "background": "## 决策\nWorth doing: resume replays out of order.",
                "state": "active",
                "phases": [{ "phase": "survey the callers", "status": "pending" }]
            }),
        )
        .unwrap();
        let slot = ctx.progress.lock().unwrap();
        let plan = slot.as_ref().unwrap();
        assert_eq!(plan.description, "make resume replay aux events in order");
        assert!(plan.background().contains("replays out of order"));
        assert!(
            plan.body().find("## 决策").unwrap() < plan.body().find("[ ] 1.").unwrap(),
            "the prose leads the document the reviewer reads: {}",
            plan.body()
        );
    }

    /// The same omit-to-keep bargain `detail` strikes: a phase flip resends the
    /// breakdown, and prose that had to ride along would be paid for every time.
    #[test]
    fn a_resend_without_background_keeps_the_prose_and_cannot_rewrite_it_blind() {
        let ctx = test_ctx();
        apply_call(
            &ctx,
            &json!({ "title": "Ship it", "background": "why we are doing this at all",
                     "phases": [{ "phase": "one", "status": "in_progress" }] }),
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

        apply_call(
            &ctx,
            &json!({ "phases": [{ "phase": "one", "status": "completed" }] }),
        )
        .unwrap();
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("why we are doing this at all"));

        // A fresh session holding only the file may not overwrite prose it was
        // never shown — and the refusal hands it over, so the retry is informed.
        *ctx.progress.lock().unwrap() = Some(Progress::load(&path).unwrap());
        let rewrite = json!({ "background": "my version" });
        let error = apply_call(&ctx, &rewrite).unwrap_err();
        assert!(error.contains("why we are doing this at all"), "{error}");
        apply_call(&ctx, &rewrite).unwrap();
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("my version"));
    }

    /// A plan whose notes run long costs a pointer per compact, not their
    /// weight — and prose the model was only pointed at is prose it has not
    /// seen, so it still cannot rewrite it blind.
    #[test]
    fn an_oversized_background_degrades_to_its_sections() {
        let mut progress = sample();
        let section = "x".repeat(SUMMARY_BACKGROUND_BUDGET);
        progress.background = format!("## 决策\n{section}\n\n## 数据结构\nmore");
        let summary = progress.summary();
        assert!(summary.contains("- 决策"), "{summary}");
        assert!(summary.contains("- 数据结构"), "{summary}");
        assert!(
            !summary.contains(&section),
            "the weight stays out of context"
        );
        assert!(summary.contains("no arguments"), "{summary}");
        assert!(!progress.background_seen);
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
                "description": "check the suite still passes end to end",
                "background": "## 决策\nWorth doing.",
                "state": "active",
                "phases": [{ "phase": "run tests", "status": "pending", "detail": "cargo test" }]
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
                "description": "stop a declined draft from being overwritten",
                "background": "## 决策\nWorth doing.",
                "state": "active",
                "phases": [{ "phase": "write regression", "status": "pending",
                             "detail": "cover the declined path" }]
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

    /// The whole resume path, end to end: one session writes the reasoning, a
    /// later one reloads the file from disk with only titles and boxes in hand,
    /// resends that, and must both keep the prose on disk and be handed the
    /// phase it just entered.
    #[test]
    fn a_later_session_reloading_the_file_keeps_and_receives_the_detail() {
        let ctx = test_ctx();
        apply_call(
            &ctx,
            &json!({ "title": "Ship it", "phases": [
                { "phase": "survey", "status": "in_progress", "detail": "read ledger.rs first" },
                { "phase": "change it", "status": "pending", "detail": "touch truncate_tail" }
            ] }),
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

        // A fresh session: nothing in context but the file on disk.
        *ctx.progress.lock().unwrap() = Some(Progress::load(&path).unwrap());
        let summary = current_summary(&ctx).unwrap();
        assert!(summary.contains("read ledger.rs first"), "{summary}");
        assert!(!summary.contains("truncate_tail"), "{summary}");

        // Resending what that summary showed — titles and boxes.
        let result = apply_call(
            &ctx,
            &json!({ "phases": [
                { "phase": "survey", "status": "completed" },
                { "phase": "change it", "status": "in_progress" }
            ] }),
        )
        .unwrap();
        assert!(result.contains("touch truncate_tail"), "{result}");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("read ledger.rs first"),
            "a resend must not erase detail it was never shown"
        );
    }

    /// The model's only way to read a file this tool owns.
    #[test]
    fn a_call_with_nothing_to_apply_shows_the_whole_file() {
        let ctx = test_ctx();
        apply_call(
            &ctx,
            &json!({ "title": "Ship it", "phases": [
                { "phase": "survey", "status": "completed", "detail": "old news" },
                { "phase": "change it", "status": "pending", "detail": "touch ledger.rs" }
            ] }),
        )
        .unwrap();

        let view = apply_call(&ctx, &json!({})).unwrap();
        assert!(view.contains("old news"), "{view}");
        assert!(view.contains("touch ledger.rs"), "{view}");
        assert!(view.contains("title=\"Ship it\""), "{view}");

        // Same contract as a repeated `read`: a plan that has not moved costs
        // a pointer, not a second copy.
        let again = apply_call(&ctx, &json!({})).unwrap();
        assert!(again.starts_with("unchanged:"), "{again}");
        assert!(!again.contains("old news"), "{again}");

        // Moved, so worth serving again.
        apply_call(
            &ctx,
            &json!({ "phases": [
                { "phase": "survey", "status": "completed" },
                { "phase": "change it", "status": "in_progress" }
            ] }),
        )
        .unwrap();
        let moved = apply_call(&ctx, &json!({})).unwrap();
        assert!(moved.contains("old news"), "{moved}");
    }

    /// A summary injection is the harness saying "here is the file, because
    /// what you had is gone or was never yours". Anything the model was shown
    /// before that no longer counts as shown.
    #[test]
    fn re_describing_the_file_resets_what_the_conversation_knows() {
        let ctx = test_ctx();
        apply_call(
            &ctx,
            &json!({ "title": "Ship it", "phases": [
                { "phase": "survey", "status": "pending", "detail": "the reasoning" }
            ] }),
        )
        .unwrap();
        assert!(apply_call(&ctx, &json!({})).unwrap().contains("reasoning"));

        // Compact, resume, a user edit: same call, same meaning.
        current_summary(&ctx).unwrap();

        let rewrite =
            json!({ "phases": [{ "phase": "survey", "status": "pending", "detail": "mine" }] });
        let error = apply_call(&ctx, &rewrite).unwrap_err();
        assert!(
            error.contains("the reasoning"),
            "prose the model can no longer see is prose it has not seen: {error}"
        );
        // And the copy it was handed before is no longer reachable either, so
        // asking for the file again must serve it rather than point at it.
        assert!(apply_call(&ctx, &json!({})).unwrap().contains("reasoning"));
        apply_call(&ctx, &rewrite).unwrap();
    }

    /// Overwriting prose you have never read is a guess, not an edit — the same
    /// bargain `edit` strikes over file contents.
    #[test]
    fn detail_this_session_never_saw_cannot_be_rewritten_blind() {
        let ctx = test_ctx();
        apply_call(
            &ctx,
            &json!({ "title": "Ship it", "phases": [
                { "phase": "survey", "status": "pending", "detail": "the reasoning nobody read" }
            ] }),
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
        *ctx.progress.lock().unwrap() = Some(Progress::load(&path).unwrap());

        let rewrite = json!({ "phases": [{ "phase": "survey", "status": "pending", "detail": "my version" }] });
        let error = apply_call(&ctx, &rewrite).unwrap_err();
        assert!(error.contains("the reasoning nobody read"), "{error}");
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("the reasoning nobody read"));

        // Refusing once and handing the text over is the whole mechanism: the
        // informed retry goes through, or the tool would be wedged.
        apply_call(&ctx, &rewrite).unwrap();
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("my version"));
    }

    /// Detail delivered by any route counts as read.
    #[test]
    fn seeing_a_phase_detail_is_what_unlocks_rewriting_it() {
        let ctx = test_ctx();
        apply_call(
            &ctx,
            &json!({ "title": "Ship it", "phases": [
                { "phase": "survey", "status": "in_progress", "detail": "the reasoning" }
            ] }),
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

        // A fresh session that was handed the summary has seen the running
        // phase, and only that one.
        *ctx.progress.lock().unwrap() = Some(Progress::load(&path).unwrap());
        current_summary(&ctx).unwrap();
        apply_call(
            &ctx,
            &json!({ "phases": [{ "phase": "survey", "status": "in_progress", "detail": "revised" }] }),
        )
        .unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("revised"));
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

        // "Their version wins" has to mean the handle adopts it too. Reporting
        // the same conflict forever would wedge the tool for the rest of the
        // session, and the error asks for a resend that could never land.
        apply_call(
            &ctx,
            &json!({ "phases": [{ "phase": "I disagree", "status": "completed" }] }),
        )
        .unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("[x] 1. I disagree"), "{after}");
    }

    /// The reviewer's rewrite is the plan that gets executed, so the model has
    /// to be told when it is not the one it submitted.
    #[test]
    fn an_approved_rewrite_comes_back_to_the_model() {
        let ctx = test_ctx();
        let draft = json!({ "title": "Ship it", "state": "active",
            "description": "ship the thing", "background": "## 决策\nWorth doing.",
            "phases": [
            { "phase": "mine", "status": "pending", "detail": "my reasoning" }
        ] });
        review_copy(&ctx, &draft).unwrap();

        let mut approved = draft.clone();
        approved[REVIEW_BODY_FIELD] =
            Value::String("\n## [ ] 1. theirs\ntheir reasoning\n".to_string());
        let result = apply_call(&ctx, &approved).unwrap();
        assert!(result.contains("their reasoning"), "{result}");
        assert!(result.contains("rewrote"), "{result}");

        // Unchanged approvals stay quiet — the model already has that text.
        let ctx = test_ctx();
        review_copy(&ctx, &draft).unwrap();
        let mut approved = draft.clone();
        approved[REVIEW_BODY_FIELD] =
            Value::String(ctx.progress.lock().unwrap().as_ref().unwrap().body());
        let result = apply_call(&ctx, &approved).unwrap();
        assert!(!result.contains("rewrote"), "{result}");
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

    /// The question every review surface asks before deciding where a call
    /// belongs. A phase flip is not a document; a submission and a review copy
    /// both are.
    #[test]
    fn a_plan_document_is_a_submission_or_a_saved_review_body() {
        let update = json!({ "phases": [{ "phase": "one", "status": "in_progress" }] });
        assert!(!is_plan_document(&update));

        let submission = json!({
            "state": "active",
            "phases": [{ "phase": "one", "status": "pending" }]
        });
        let document = plan_document(&submission).expect("a submission renders its own phases");
        assert!(document.contains("[ ] 1. one"), "{document}");

        // The saved body wins: it is the text the reviewer actually saw, which
        // may be their own rewrite rather than the phases the model sent.
        let mut reviewed = submission.clone();
        reviewed[REVIEW_BODY_FIELD] = Value::String("\n## [ ] 1. theirs\n".into());
        assert_eq!(
            plan_document(&reviewed).unwrap().trim(),
            "## [ ] 1. theirs",
            "a rewritten body is the document"
        );
        // A submission with nothing in it is not a document to read.
        assert!(!is_plan_document(&json!({ "state": "active" })));
    }

    /// The reviewer's counterpart to `resending_titles_alone_keeps_the_stored_
    /// detail`: a structural review edit must not cost the file the reasoning
    /// behind phases the reviewer never opened.
    #[test]
    fn a_reviewed_edit_keeps_detail_the_reviewer_left_alone() {
        let body =
            "\n## Decision\nworth doing\n\n## [ ] 1. one\nwhy one\n\n## [ ] 2. two\nwhy two\n";
        let revised = revise_plan_body(
            body,
            &[
                Phase::new("two", PhaseStatus::Pending),
                Phase {
                    phase: "one".into(),
                    status: PhaseStatus::Pending,
                    detail: "rewritten by the reviewer".into(),
                    phases: Vec::new(),
                },
            ],
        );
        assert!(
            revised.contains("why two"),
            "untouched prose survives: {revised}"
        );
        assert!(
            revised.contains("## Decision\nworth doing"),
            "a structural editor cannot send the plan's prose back, so re-rendering must not drop it: {revised}"
        );
        assert!(revised.contains("rewritten by the reviewer"), "{revised}");
        assert!(
            !revised.contains("why one"),
            "and theirs replaces it: {revised}"
        );
        // Their order is the plan's order.
        assert!(
            revised.find("2. one").unwrap() > revised.find("1. two").unwrap(),
            "{revised}"
        );
    }

    #[test]
    fn notes_quote_their_passage_and_free_feedback_comes_last() {
        let notes = vec![
            PlanNote {
                quote: Some("risk: rewind crosses\nthe Summary boundary".into()),
                text: "write the regression test first".into(),
            },
            PlanNote {
                quote: None,
                text: "no anchor, still feedback".into(),
            },
            // An empty comment is not feedback; sending it would be a bare quote.
            PlanNote {
                quote: Some("something".into()),
                text: "   ".into(),
            },
        ];
        let note = plan_notes(&notes, "  and rename the phase  ").unwrap();
        assert_eq!(
            note,
            "> risk: rewind crosses\n> the Summary boundary\n\nwrite the regression test first\n\nno anchor, still feedback\n\nand rename the phase"
        );
        assert!(
            plan_notes(&[], "").is_none(),
            "nothing to say sends nothing"
        );
    }

    #[test]
    fn an_approved_rewrite_leads_the_note_and_a_declined_one_travels_as_a_diff() {
        let notes = vec![PlanNote {
            quote: Some("one".into()),
            text: "start here".into(),
        }];
        let approved = approved_plan_note(Some("## [ ] 1. theirs"), &notes, "").unwrap();
        assert!(
            approved.starts_with("The user edited the plan before approving."),
            "the model is about to execute a plan it did not write: {approved}"
        );
        assert!(approved.contains("## [ ] 1. theirs"));
        assert!(approved.ends_with("start here"), "{approved}");

        let diff = plan_revision_diff("## [ ] 1. mine\n", "## [ ] 1. theirs\n").unwrap();
        let declined = declined_plan_note(Some(&diff), &notes, "").unwrap();
        assert!(
            declined.starts_with("The user edited the plan:"),
            "{declined}"
        );
        assert!(declined.contains("-## [ ] 1. mine"), "{declined}");
        assert!(declined.contains("+## [ ] 1. theirs"), "{declined}");

        // An unedited plan says nothing about edits, in either direction.
        assert!(plan_revision_diff("same\n", "  same  ").is_none());
        assert_eq!(
            approved_plan_note(None, &notes, "").unwrap(),
            "> one\n\nstart here"
        );
        assert!(declined_plan_note(None, &[], "  ").is_none());
    }
}
