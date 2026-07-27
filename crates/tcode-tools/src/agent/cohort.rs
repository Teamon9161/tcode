//! Cohorts: the `cohort` tool convenes a group of sub-agents that each explore
//! in their own private context but share one append-only channel, so they can
//! debate a question and then each return its own independent report. Only the
//! channel is shared — every member's private tool calls and reasoning stay in
//! its own session, exactly like a normal delegated run.
//!
//! Implemented through P2 (see `COHORT-DESIGN.md` §16): a sequential round-robin
//! scheduler drives one full `user_turn` per member per round; members speak
//! through an injected `channel` tool; the channel delta is fenced into each
//! activation like an attached report; a final round collects one report per
//! member (remembered in the shared report store so the parent can `attach`
//! them). The tool is a resumable delegation, not a blocking call: it yields to
//! the parent when a member addresses it (`to: "parent"`) or fails, parks the
//! cohort, and continues on `cohort(resume, answer?)`; the parent can read the
//! transcript on demand (`action: "channel"`).
//!
//! Persistence spans two files per cohort in the session's trace directory: the
//! channel log appends line-by-line as JSONL, and a small `meta.json` is
//! rewritten whole at each pause. A `resume`/`channel` after a restart, when the
//! in-memory cohort is gone, rebuilds it from those files plus each member's
//! trace chain — the cohort analogue of `restore_run`. An oversized single post
//! spills to `scratchpad/cohort/<id>/` and keeps only a preview+pointer in the
//! channel (§6a), so the shared context stays bounded.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use tcode_core::blobs::BlobStore;
use tcode_core::{
    Agent, CohortChannelMessage, CohortMember as CohortMemberView, CohortMemberRun,
    CohortMemberStatus, CohortUpdate, DelegateEvent, PermissionRequest, Session, Tool, ToolCtx,
    ToolOutput,
};

use super::AgentTool;

/// Rounds of debate before the cohort finalizes. A trial value; `max_rounds`
/// overrides it per call. Kept small: each round costs one turn per member.
const DEFAULT_MAX_ROUNDS: usize = 5;
/// Absolute ceiling on `max_rounds` so a caller cannot spend an unbounded
/// number of turns per member.
const MAX_ROUNDS_CAP: usize = 12;
/// A cohort needs at least two members to debate; the cap keeps one call from
/// spawning an unreasonable fan-out of concurrent sessions.
const MIN_MEMBERS: usize = 2;
const MAX_MEMBERS: usize = 6;
/// Paused cohorts kept resumable per tool instance, oldest evicted beyond this.
/// Each holds its members' whole sessions in memory, so the cap is modest.
const MAX_LIVE_COHORTS: usize = 8;

/// The closing tag members' messages are fenced with. Neutralized in the body
/// at the emitter (`fence_channel`), like `web.rs::fence_page` and
/// `attach_reports`: only-wrapping without escaping is not wrapping at all.
const CHANNEL_FENCE_END: &str = "</channel-message>";

/// Discipline every member is told once, on its first activation.
const MEMBER_PREAMBLE: &str = include_str!("../../prompts/cohort-member.md");
/// The finalize instruction, sent as the last activation.
const FINALIZE_PROMPT: &str = include_str!("../../prompts/cohort-finalize.md");

/// One message on a cohort's shared channel. Append-only; `seq` starts at 1.
/// `Deserialize` too so the log reloads from its JSONL on crash recovery (§11).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChannelMsg {
    seq: usize,
    /// The member id that spoke (`m1`, `m2`, …), or `parent`.
    from: String,
    /// Addressee when directed: a member id or `parent`. Everyone still sees
    /// the message; `to` is only a hint to the intended reader. `None` =
    /// broadcast.
    to: Option<String>,
    body: String,
    round: usize,
}

/// A cohort's append-only discussion log, shared (behind a mutex) between every
/// member's `channel` tool instance. Persisted line-by-line as JSONL when the
/// session has a bound trace root; in-memory otherwise.
struct Channel {
    log: Vec<ChannelMsg>,
    /// The round posts are currently stamped with; the scheduler sets it before
    /// activating that round's members.
    round: usize,
    writer: Option<BufWriter<File>>,
    /// Spills an oversized single post to `scratchpad/cohort/<id>/` and keeps
    /// only a head+tail preview + pointer in the channel (§6a): the shared
    /// context has an upper bound, and a member that wants the whole artifact
    /// pages the file with its own `read`/`grep`.
    blobs: BlobStore,
}

impl Channel {
    /// Open a channel: its JSONL writer (append), its overflow blob store, and,
    /// when `load_existing`, the messages already on disk (crash recovery).
    fn open(path: Option<PathBuf>, blob_dir: PathBuf, budget: usize, load_existing: bool) -> Self {
        let log = if load_existing {
            path.as_deref().map(load_channel_log).unwrap_or_default()
        } else {
            Vec::new()
        };
        let round = log.last().map_or(0, |msg| msg.round);
        // The channel opens before any member drives — i.e. before the first
        // `TaskTraces::begin` creates the session's trace dir — so create the
        // parent here rather than silently degrading to an in-memory channel.
        let writer = path
            .and_then(|path| {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                OpenOptions::new().create(true).append(true).open(path).ok()
            })
            .map(BufWriter::new);
        Self {
            log,
            round,
            writer,
            blobs: BlobStore::new(blob_dir, budget),
        }
    }

    fn new(path: Option<PathBuf>, blob_dir: PathBuf, budget: usize) -> Self {
        Self::open(path, blob_dir, budget, false)
    }

    /// Reload a persisted channel from its JSONL, reopening the writer so
    /// further posts append past the recovered history.
    fn restore(path: Option<PathBuf>, blob_dir: PathBuf, budget: usize) -> Self {
        Self::open(path, blob_dir, budget, true)
    }

    /// Append one message and persist it. Best-effort persistence, like the
    /// session log: a write failure degrades to an in-memory-only channel,
    /// never a failed post. An oversized body is spilled to a file first (§6a),
    /// so the stored, fenced and persisted body is always the bounded preview.
    fn post(&mut self, from: String, to: Option<String>, body: String) -> usize {
        let body = self.blobs.gate("channel", body, false);
        let seq = self.log.len() + 1;
        let msg = ChannelMsg {
            seq,
            from,
            to,
            body,
            round: self.round,
        };
        if let Some(writer) = &mut self.writer {
            if let Ok(line) = serde_json::to_string(&msg) {
                let _ = writeln!(writer, "{line}");
                let _ = writer.flush();
            }
        }
        self.log.push(msg);
        seq
    }

    /// Messages `member` has not seen yet, cloned for fencing without holding
    /// the lock across the member's turn.
    fn delta(&self, cursor: usize) -> Vec<ChannelMsg> {
        self.log
            .get(cursor..)
            .map(<[_]>::to_vec)
            .unwrap_or_default()
    }

    /// A plain, readable transcript of the whole channel for the parent to read
    /// on demand (`action: "channel"`). This is the parent observing the
    /// members' output — returned as tool output like a report, so it is not
    /// re-fenced here; the caller blob-gates it.
    fn transcript(&self) -> String {
        render_transcript(&self.log)
    }
}

/// Render a channel log as a plain readable transcript. Free-standing so the
/// parent can read a cohort's discussion straight off disk (§11) without first
/// rebuilding all its member sessions.
fn render_transcript(log: &[ChannelMsg]) -> String {
    if log.is_empty() {
        return "(the channel is empty)".to_string();
    }
    let mut out = String::new();
    for msg in log {
        let to = msg.to.as_deref().unwrap_or("all");
        out.push_str(&format!(
            "[seq {} · round {} · {} → {}]\n{}\n\n",
            msg.seq, msg.round, msg.from, to, msg.body
        ));
    }
    out
}

/// Read a persisted channel's JSONL back into memory, one `ChannelMsg` per
/// line. Best-effort, mirroring the write side: an unreadable or partly written
/// file yields the messages that did parse, never an error.
fn load_channel_log(path: &Path) -> Vec<ChannelMsg> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<ChannelMsg>(&line).ok())
        .collect()
}

/// Fence a run of channel messages so they enter a member's context as data,
/// not instructions — the same discipline as `attach_reports`. The closing tag
/// is neutralized in each body here at the single emitter; `seq`/`from`/`to`
/// are scheduler-issued (`m<n>`/`parent`/`all`) and carry no injection surface.
fn fence_channel(msgs: &[ChannelMsg]) -> String {
    let mut out = String::new();
    for msg in msgs {
        let body = msg.body.replace(CHANNEL_FENCE_END, "<\\/channel-message>");
        let to = msg.to.as_deref().unwrap_or("all");
        out.push_str(&format!(
            "<channel-message seq=\"{}\" from=\"{}\" to=\"{}\">\n{body}\n{CHANNEL_FENCE_END}\n",
            msg.seq, msg.from, to
        ));
    }
    out
}

/// One member of a cohort: a private session plus its channel bookkeeping.
struct Member {
    /// Channel identity, `m1`, `m2`, … — stable and independent of the trace
    /// run id, which is not known until the first activation.
    id: String,
    kind: String,
    task: String,
    summary: String,
    agent: Agent,
    session: Session,
    model_name: String,
    /// The trace run id of the member's *first* activation; later activations
    /// chain to it via `resume_of` so the whole session can be rebuilt from
    /// disk. `None` until it has taken its first turn.
    run_id: Option<String>,
    /// How far into the channel this member has read.
    cursor: usize,
    /// The roster snapshot this member has already received. The complete
    /// roster is sent once; later turns receive a new snapshot only when
    /// someone left, so roster awareness does not turn into repeated prompt
    /// overhead.
    seen_roster_revision: usize,
    /// Still in the round-robin. Cleared by `channel_leave` or a failed turn.
    active: bool,
    /// Set by this member's `channel` tool when it calls `channel_leave`.
    left: Arc<AtomicBool>,
}

/// The member-facing `channel` tool. One instance per member, holding a handle
/// to the shared channel and that member's identity. Injected into the member's
/// toolset by the scheduler (`build_run_with`), so it bypasses the readonly
/// ceiling and tool policy on purpose — the cohort grants it, not a definition.
struct ChannelTool {
    channel: Arc<Mutex<Channel>>,
    cohort_id: String,
    from: String,
    left: Arc<AtomicBool>,
    description: String,
}

impl ChannelTool {
    fn new(
        channel: Arc<Mutex<Channel>>,
        cohort_id: String,
        from: String,
        left: Arc<AtomicBool>,
    ) -> Self {
        let description = format!(
            "Speak on your cohort's shared channel. Other members see what you post here; your \
             private tool calls and reasoning stay yours alone. IMPORTANT: the text at the end of \
             your turn is NOT shared with anyone — only `channel_post` messages are read by other \
             members. You are `{from}`.\n\
             - action \"post\": send a message. Optional `to` addresses a member id or \"parent\" \
             (everyone still sees it). Post as many times as you like in one turn.\n\
             - action \"leave\": bow out of further rounds. You still write a final report at the \
             end and your earlier messages remain visible.",
        );
        Self {
            channel,
            cohort_id,
            from,
            left,
            description,
        }
    }
}

#[async_trait]
impl Tool for ChannelTool {
    fn name(&self) -> &str {
        "channel"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["post", "leave"] },
                "to": {
                    "type": "string",
                    "description": "Optional addressee for a post: a member id (e.g. \"m2\") or \"parent\". Everyone still sees the message."
                },
                "body": { "type": "string", "description": "The message text, for action \"post\"." }
            },
            "required": ["action"]
        })
    }

    fn permission(&self, _input: &Value) -> PermissionRequest {
        // Posting to the cohort's own channel is not an externally visible side
        // effect; it never leaves the session.
        PermissionRequest::None
    }

    fn auto_safety(&self, _input: &Value) -> tcode_core::AutoSafety {
        tcode_core::AutoSafety::Allow
    }

    fn gates_output(&self) -> bool {
        false
    }

    async fn run(&self, input: Value, ctx: &ToolCtx, _cancel: &CancellationToken) -> ToolOutput {
        match input["action"].as_str() {
            Some("leave") => {
                self.left.store(true, Ordering::SeqCst);
                ToolOutput::ok("left the cohort's rounds; you will still write a final report")
            }
            Some("post") => {
                let Some(body) = input["body"]
                    .as_str()
                    .filter(|body| !body.trim().is_empty())
                else {
                    return ToolOutput::err("`post` requires a non-empty `body`");
                };
                let to = input["to"]
                    .as_str()
                    .map(str::trim)
                    .filter(|to| !to.is_empty())
                    .map(ToOwned::to_owned);
                let message = {
                    let mut channel = self.channel.lock().expect("cohort channel lock");
                    let seq = channel.post(self.from.clone(), to, body.to_string());
                    let posted = channel.log.last().expect("post just appended");
                    CohortChannelMessage {
                        cohort_id: self.cohort_id.clone(),
                        seq,
                        from: posted.from.clone(),
                        to: posted.to.clone(),
                        body: posted.body.clone(),
                        round: posted.round,
                    }
                };
                let seq = message.seq;
                if let Some(delegate) = ctx.delegate_reporter() {
                    let _ = delegate.send(DelegateEvent::CohortChannelMessage(message));
                }
                ToolOutput::ok(format!("posted to the channel (seq {seq})"))
            }
            _ => ToolOutput::err("`action` must be \"post\" or \"leave\""),
        }
    }
}

/// A convened cohort, held between yields. Parked in `CohortTool::cohorts` when
/// the scheduler stops to let the parent answer a question or acknowledge a
/// failed member, and taken back out on `resume`. Members' whole sessions live
/// here (not in the shared `live` map) so a debate is never evicted by ordinary
/// parked runs mid-flight (COHORT-DESIGN.md §4).
struct Cohort {
    id: String,
    /// The conversation that convened this cohort; see `LiveTask::scope`. An id
    /// only means something inside the conversation that issued it.
    scope: PathBuf,
    members: Vec<Member>,
    channel: Arc<Mutex<Channel>>,
    /// Round the round-robin is currently on.
    round: usize,
    max_rounds: usize,
    /// Next member index to activate within `round`; preserved across a yield so
    /// the rest of the round runs before the next round on resume (§9).
    next_index: usize,
    /// Increments whenever a member leaves the active roster. Members receive
    /// an updated overview at most once per revision.
    roster_revision: usize,
    /// Whether completion returns full reports or only their attachable run ids.
    detached_reports: bool,
    /// Parent `cohort` call, so member traces tie back to the spawning entry.
    call_id: String,
    /// Park order for oldest-first eviction.
    seq: u64,
}

/// Presentation-only scheduler phase for a complete cohort roster snapshot.
/// The scheduler keeps the authoritative mechanics on `Member`; this enum
/// only gives frontends a truthful state without parsing prompt summaries.
enum CohortViewPhase<'a> {
    Waiting,
    Running(&'a str),
    Finalizing,
    Done,
}

/// The small resumable state persisted at each scheduler exit (§11), rewritten
/// whole into `cohort-<id>.meta.json`. The channel itself lives in its own
/// append-only JSONL; the members' sessions rebuild from their trace chains.
/// `scope` is deliberately *not* stored: a recovered cohort takes the resuming
/// conversation's scope, and the meta file is only reachable from that same
/// conversation's session directory, so the scope invariant holds by path.
#[derive(Serialize, Deserialize)]
struct CohortMeta {
    id: String,
    call_id: String,
    round: usize,
    max_rounds: usize,
    next_index: usize,
    #[serde(default)]
    roster_revision: usize,
    #[serde(default)]
    detached_reports: bool,
    members: Vec<MemberMeta>,
}

/// One member's recoverable bookkeeping. The session behind `run_id` rebuilds
/// from its trace chain (`TaskTraces::restore`); everything here is what the
/// scheduler needs to place that session back in the round-robin.
#[derive(Serialize, Deserialize)]
struct MemberMeta {
    id: String,
    kind: String,
    task: String,
    #[serde(default)]
    summary: String,
    run_id: Option<String>,
    cursor: usize,
    #[serde(default)]
    seen_roster_revision: usize,
    active: bool,
    left: bool,
}

/// Where a cohort spills oversized channel messages: a dedicated directory per
/// cohort under the conversation's scratch, kept apart from the shared
/// `tool-output/` so recovery and cross-member reads can find them (§6a/§11).
fn cohort_blob_dir(scratch: &Path, id: &str) -> PathBuf {
    scratch.join("cohort").join(id)
}

/// Why the scheduler stopped. Three yields plus a hard cancel (§5).
enum CohortOutcome {
    /// Debate finished: one report per member, run ids paired for `attach`.
    Done(Vec<MemberReport>),
    /// A member addressed the parent; the debate is paused for a reply.
    AskParent {
        member: String,
        questions: Vec<String>,
    },
    /// A member's turn failed; it left the debate and the parent is told.
    Stalled { member: String, reason: String },
    /// The user cancelled mid-debate.
    Cancelled,
}

/// One finalized member's output: its channel id, kind, resumable run id (for
/// `attach`), and report text.
struct MemberReport {
    member: String,
    kind: String,
    run_id: Option<String>,
    report: String,
}

/// Cohort ids reach `resume`/`channel` from the model, so — like a trace id —
/// only the issued shape `c<digits>` is honored before it is used to key the
/// map or (elsewhere) join a path.
fn valid_cohort_id(id: &str) -> bool {
    id.strip_prefix('c')
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

fn next_cohort_seq() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// The parent-facing `cohort` tool. Installed only on the depth-0 main agent
/// (see composition root). Reuses an embedded [`AgentTool`] for member session
/// construction and turn driving, so no core machinery is duplicated.
pub struct CohortTool {
    inner: AgentTool,
    counter: AtomicU64,
    /// Paused cohorts, scope-isolated and capped like the agent tool's `live`.
    cohorts: Arc<Mutex<HashMap<String, Cohort>>>,
    description: String,
}

impl AgentTool {
    /// Build the sibling `cohort` tool that shares this agent tool's model,
    /// pins, watchdog, filters and agent registry. It shares the *same* `live`
    /// and `reports` maps as the main agent tool: a finalized member's report is
    /// remembered there, so the parent can `attach` it to a later `agent` call
    /// by run id exactly as it would any other run's report.
    pub fn cohort_tool(&self) -> CohortTool {
        let inner = AgentTool {
            model: self.model.clone(),
            pinned: self.pinned.clone(),
            watchdog: self.watchdog.clone(),
            output_budget: self.output_budget,
            auto_policy: self.auto_policy.clone(),
            auto_classifier_config: self.auto_classifier_config,
            auto_compact: self.auto_compact,
            auto_compact_percent: self.auto_compact_percent,
            trusted_read_hosts: self.trusted_read_hosts.clone(),
            shell_filters: self.shell_filters.clone(),
            extension_tools: self.extension_tools.clone(),
            description: self.description.clone(),
            defs: self.defs.clone(),
            allowed: None,
            depth: 0,
            live: self.live.clone(),
            reports: self.reports.clone(),
            resolver: self.resolver.clone(),
        };
        CohortTool::new(inner)
    }
}

impl CohortTool {
    fn new(inner: AgentTool) -> Self {
        let description = format!(
            "Convene a cohort of sub-agents that work in private contexts while sharing one \
             append-only channel, then each returns its own independent report (disagreements \
             preserved, not synthesized). Give `members` (agent kinds, {MIN_MEMBERS}–{MAX_MEMBERS}), `tasks` \
             (one complete task per member) and `summaries` (one short UI label per member); all three arrays \
             must have the same length. Members run read-only for up \
             to `max_rounds` rounds (default {DEFAULT_MAX_ROUNDS}); each round every member takes one turn and \
             may speak on the channel. This is a resumable delegation, not a blocking call: if a \
             member addresses you (`to: \"parent\"`) or a member fails, it returns paused with the \
             cohort id — reply or continue with `cohort(resume: \"<id>\", answer?: \"…\")`. Read the \
             discussion so far at any pause with `cohort(action: \"channel\", id: \"<id>\")`. Use for \
             review, debate, or multi-angle exploration where several perspectives matter more \
             than one merged answer."
        );
        Self {
            inner,
            counter: AtomicU64::new(0),
            cohorts: Arc::default(),
            description,
        }
    }

    /// Parse and validate `members`/`tasks`/`summaries`/`max_rounds`.
    fn parse(&self, input: &Value) -> Result<(Vec<(String, String, String)>, usize, bool), String> {
        let members = input["members"]
            .as_array()
            .ok_or("`members` must be an array of agent-kind strings")?;
        let tasks = input["tasks"]
            .as_array()
            .ok_or("`tasks` must be an array of task strings")?;
        let summaries = input["summaries"]
            .as_array()
            .ok_or("`summaries` must be an array of short task summaries")?;
        if members.len() != tasks.len() || tasks.len() != summaries.len() {
            return Err(format!(
                "`members` ({}), `tasks` ({}) and `summaries` ({}) must be the same length",
                members.len(),
                tasks.len(),
                summaries.len()
            ));
        }
        if !(MIN_MEMBERS..=MAX_MEMBERS).contains(&members.len()) {
            return Err(format!(
                "a cohort needs {MIN_MEMBERS}–{MAX_MEMBERS} members, got {}",
                members.len()
            ));
        }
        let mut pairs = Vec::with_capacity(members.len());
        for ((member, task), summary) in members.iter().zip(tasks).zip(summaries) {
            let kind = member
                .as_str()
                .map(str::trim)
                .filter(|kind| !kind.is_empty())
                .ok_or("every `members` entry must be a non-empty agent-kind string")?;
            let task = task
                .as_str()
                .map(str::trim)
                .filter(|task| !task.is_empty())
                .ok_or("every `tasks` entry must be a non-empty string")?;
            let summary = summary
                .as_str()
                .map(str::trim)
                .filter(|summary| !summary.is_empty())
                .ok_or("every `summaries` entry must be a non-empty short task label")?;
            pairs.push((kind.to_string(), task.to_string(), summary.to_string()));
        }
        let max_rounds = input["max_rounds"]
            .as_u64()
            .map(|rounds| (rounds as usize).clamp(1, MAX_ROUNDS_CAP))
            .unwrap_or(DEFAULT_MAX_ROUNDS);
        let detached_reports = match input.get("detached") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err("`detached` must be true or false".to_string()),
        };
        Ok((pairs, max_rounds, detached_reports))
    }

    /// A concise roster snapshot. Members receive it on their first turn and
    /// once after each departure; ordinary turns carry only channel deltas.
    fn overview(members: &[Member], me: &str) -> String {
        let active = members.iter().filter(|member| member.active).count();
        let finished = members.len() - active;
        let mut out = format!(
            "Cohort roster: {active} active, {finished} finished. You are `{me}`.\nActive members:\n"
        );
        for member in members.iter().filter(|member| member.active) {
            out.push_str(&format!(
                "- {} ({}): {}\n",
                member.id, member.kind, member.task
            ));
        }
        if finished > 0 {
            out.push_str("Finished members (they only return for final reports):\n");
            for member in members.iter().filter(|member| !member.active) {
                out.push_str(&format!("- {} ({})\n", member.id, member.kind));
            }
        }
        out
    }

    /// Forward a channel post through the same frontend-only delegate stream
    /// whether it came from a member tool or from the parent resuming a pause.
    fn announce_channel_message(message: &CohortChannelMessage, ctx: &ToolCtx) {
        if let Some(delegate) = ctx.delegate_reporter() {
            let _ = delegate.send(DelegateEvent::CohortChannelMessage(message.clone()));
        }
    }

    /// Publish one complete scheduler snapshot for frontend transcript cards.
    /// This uses the delegate stream just like normal task progress, but carries
    /// typed roster state instead of asking renderers to reverse-engineer it.
    fn announce(&self, cohort: &Cohort, phase: CohortViewPhase<'_>, ctx: &ToolCtx) {
        let Some(delegate) = ctx.delegate_reporter() else {
            return;
        };
        let members = cohort
            .members
            .iter()
            .map(|member| {
                let status = match phase {
                    CohortViewPhase::Running(id) if member.id == id => CohortMemberStatus::Running,
                    CohortViewPhase::Finalizing => CohortMemberStatus::Finalizing,
                    CohortViewPhase::Done => CohortMemberStatus::Done,
                    _ if member.active => CohortMemberStatus::Waiting,
                    _ if member.left.load(Ordering::SeqCst) => CohortMemberStatus::Left,
                    _ => CohortMemberStatus::Failed,
                };
                CohortMemberView {
                    id: member.id.clone(),
                    kind: member.kind.clone(),
                    task: member.task.clone(),
                    summary: member.summary.clone(),
                    model: member.model_name.clone(),
                    run: member.run_id.clone(),
                    status,
                }
            })
            .collect();
        let _ = delegate.send(DelegateEvent::CohortUpdated(CohortUpdate {
            id: cohort.id.clone(),
            parent_call: cohort.call_id.clone(),
            round: cohort.round,
            max_rounds: cohort.max_rounds,
            members,
        }));
    }
}

#[async_trait]
impl Tool for CohortTool {
    fn name(&self) -> &str {
        "cohort"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "members": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Convene: agent kinds, one per member. Members run read-only."
                },
                "tasks": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Convene: one complete self-contained task per member, same length as `members`. May be the same task for all or different tasks."
                },
                "summaries": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Convene: one concise task label per member, same length as `members` and `tasks`. Required so cohort cards stay readable."
                },
                "max_rounds": {
                    "type": "integer",
                    "description": "Convene: debate rounds before finalizing (default 5). Each round is one turn per member."
                },
                "detached": {
                    "type": "boolean",
                    "description": "Convene: keep final reports only in the attach store and return their run ids instead of placing report text in this context (default false)."
                },
                "resume": {
                    "type": "string",
                    "description": "Resume a paused cohort by its id (from the pause message). Continues to the next pause or to the final reports."
                },
                "answer": {
                    "type": "string",
                    "description": "Optional reply, injected into the channel as a message from `parent`, when resuming a cohort that asked you something. Omit to just continue."
                },
                "action": {
                    "type": "string",
                    "enum": ["channel"],
                    "description": "Set to \"channel\" (with `id`) to read a paused cohort's whole discussion transcript into your context."
                },
                "id": {
                    "type": "string",
                    "description": "Cohort id for `action: \"channel\"`."
                }
            }
        })
    }

    fn permission(&self, _input: &Value) -> PermissionRequest {
        // Like `agent`, convening only creates isolated sessions; each member's
        // side-effecting calls reach the same approval boundary (read-only here).
        PermissionRequest::None
    }

    async fn run(&self, input: Value, ctx: &ToolCtx, cancel: &CancellationToken) -> ToolOutput {
        self.run_with_call("", input, ctx, cancel).await
    }

    async fn run_with_call(
        &self,
        call_id: &str,
        input: Value,
        ctx: &ToolCtx,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        // Read a paused cohort's whole transcript into the parent context.
        if input["action"].as_str() == Some("channel") {
            return self.channel_view(&input, ctx);
        }
        // Resume a paused cohort, optionally injecting the parent's reply.
        if input["resume"]
            .as_str()
            .map(str::trim)
            .is_some_and(|id| !id.is_empty())
        {
            return self.resume(call_id, &input, ctx, cancel).await;
        }
        // Otherwise convene a fresh cohort.
        self.convene(&input, call_id, ctx, cancel).await
    }
}

impl CohortTool {
    /// Convene a fresh cohort and drive it to its first exit.
    async fn convene(
        &self,
        input: &Value,
        call_id: &str,
        ctx: &ToolCtx,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let (pairs, max_rounds, detached_reports) = match self.parse(input) {
            Ok(parsed) => parsed,
            Err(reason) => return ToolOutput::err(reason),
        };
        let mut cohort = match self.build_cohort(pairs, max_rounds, detached_reports, call_id, ctx)
        {
            Ok(cohort) => cohort,
            Err(out) => return out,
        };
        self.announce(&cohort, CohortViewPhase::Waiting, ctx);
        let outcome = self.drive_to_exit(&mut cohort, ctx, cancel).await;
        self.handle_outcome(cohort, outcome, ctx)
    }

    /// Resume a paused cohort, injecting the parent's `answer` (if any) as a
    /// `from: parent` channel message before driving on to the next exit.
    async fn resume(
        &self,
        call_id: &str,
        input: &Value,
        ctx: &ToolCtx,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let id = input["resume"].as_str().map(str::trim).unwrap_or_default();
        if !valid_cohort_id(id) {
            return ToolOutput::err(format!("'{id}' is not a valid cohort id (want c<number>)"));
        }
        // In memory when paused this session; otherwise rebuilt from disk after
        // a restart or `/resume` (§11), exactly as `restore_run` recovers a lone
        // sub-agent — its meta file is the only thing that makes it resumable.
        let taken = self.take_cohort(id, &ctx.scratch_dir);
        let Some(mut cohort) = taken.or_else(|| self.restore_cohort(id, ctx)) else {
            let mut ids: Vec<String> = {
                let map = self.cohorts.lock().expect("cohorts lock");
                map.values()
                    .filter(|cohort| cohort.scope == ctx.scratch_dir)
                    .map(|cohort| cohort.id.clone())
                    .collect()
            };
            ids.sort();
            return ToolOutput::err(format!(
                "no paused cohort '{id}' in this conversation. Paused now: [{}]. A finished cohort \
                 is not resumable — its reports were already returned.",
                ids.join(", ")
            ));
        };
        // A resumed cohort now belongs to this tool record for transcript
        // updates and any member activations it schedules from here onward.
        cohort.call_id = call_id.to_string();
        self.announce(&cohort, CohortViewPhase::Waiting, ctx);
        // The reply is another agent-or-human message: still data (§10), it
        // enters the channel like any post and is fenced when members read it.
        if let Some(answer) = input["answer"]
            .as_str()
            .map(str::trim)
            .filter(|answer| !answer.is_empty())
        {
            let message = {
                let mut channel = cohort.channel.lock().expect("cohort channel lock");
                channel.round = cohort.round;
                let seq = channel.post("parent".to_string(), None, answer.to_string());
                let posted = channel.log.last().expect("parent post just appended");
                CohortChannelMessage {
                    cohort_id: cohort.id.clone(),
                    seq,
                    from: posted.from.clone(),
                    to: posted.to.clone(),
                    body: posted.body.clone(),
                    round: posted.round,
                }
            };
            Self::announce_channel_message(&message, ctx);
        }
        let outcome = self.drive_to_exit(&mut cohort, ctx, cancel).await;
        self.handle_outcome(cohort, outcome, ctx)
    }

    /// Read a paused cohort's transcript into the parent's context, blob-gated
    /// like any other large tool output (§8).
    fn channel_view(&self, input: &Value, ctx: &ToolCtx) -> ToolOutput {
        let id = input["id"].as_str().map(str::trim).unwrap_or_default();
        if !valid_cohort_id(id) {
            return ToolOutput::err(
                "`action: \"channel\"` needs `id` set to a valid cohort id (want c<number>)",
            );
        }
        let transcript = {
            let map = self.cohorts.lock().expect("cohorts lock");
            map.get(id)
                .filter(|cohort| cohort.scope == ctx.scratch_dir)
                .map(|cohort| {
                    cohort
                        .channel
                        .lock()
                        .expect("cohort channel lock")
                        .transcript()
                })
        };
        // Not paused in memory: read the persisted channel straight off disk if
        // this conversation still has a resumable cohort by that id (§11). Its
        // meta file is what marks it resumable — a finished cohort has none.
        let transcript = transcript.or_else(|| self.disk_transcript(id, ctx));
        let Some(transcript) = transcript else {
            return ToolOutput::err(format!(
                "no paused cohort '{id}' to read — only a cohort currently paused for you \
                 has a readable channel."
            ));
        };
        let gated = ctx
            .blobs
            .lock()
            .expect("blobs lock")
            .gate("cohort", transcript, false);
        ToolOutput::ok(gated)
    }

    /// Build every member: a read-only-forced clone of its definition, its own
    /// session, and an injected `channel` tool bound to its identity.
    fn build_cohort(
        &self,
        pairs: Vec<(String, String, String)>,
        max_rounds: usize,
        detached_reports: bool,
        call_id: &str,
        ctx: &ToolCtx,
    ) -> Result<Cohort, ToolOutput> {
        // Allocate a cohort id and open its persisted channel log (if this
        // session persists at all). The id is validated to `c<digits>` by
        // `cohort_channel_path` before it ever reaches the filesystem.
        let id = format!("c{}", self.counter.fetch_add(1, Ordering::Relaxed) + 1);
        let channel_path = ctx
            .task_traces
            .lock()
            .expect("task traces lock")
            .cohort_channel_path(&id);
        let channel = Arc::new(Mutex::new(Channel::new(
            channel_path,
            cohort_blob_dir(&ctx.scratch_dir, &id),
            self.inner.output_budget,
        )));

        let mut members: Vec<Member> = Vec::with_capacity(pairs.len());
        for (index, (kind, task, summary)) in pairs.into_iter().enumerate() {
            let Some(def) = self.inner.def_for(&kind) else {
                return Err(ToolOutput::err(format!(
                    "unknown agent '{kind}'; available: {}",
                    self.inner
                        .defs
                        .names_for(self.inner.allowed.as_deref())
                        .join(", ")
                )));
            };
            // Members are analysis-only and share a cwd; a readonly ceiling
            // keeps cross-round writes from stepping on each other (§12).
            let mut def = def.clone();
            def.read_only = true;

            let member_id = format!("m{}", index + 1);
            let left = Arc::new(AtomicBool::new(false));
            let channel_tool: Arc<dyn Tool> = Arc::new(ChannelTool::new(
                channel.clone(),
                id.clone(),
                member_id.clone(),
                left.clone(),
            ));
            let (agent, session, model_name) =
                self.inner
                    .build_run_with(&def, ctx, None, std::slice::from_ref(&channel_tool));
            members.push(Member {
                id: member_id,
                kind,
                task,
                summary,
                agent,
                session,
                model_name,
                run_id: None,
                cursor: 0,
                seen_roster_revision: 0,
                active: true,
                left,
            });
        }
        Ok(Cohort {
            id,
            scope: ctx.scratch_dir.clone(),
            members,
            channel,
            round: 0,
            max_rounds,
            next_index: 0,
            roster_revision: 1,
            detached_reports,
            call_id: call_id.to_string(),
            seq: next_cohort_seq(),
        })
    }

    /// Rebuild a paused cohort from disk (§11): its meta file (round, budget,
    /// per-member bookkeeping), its channel JSONL, and each member's session
    /// replayed from its trace chain — the cohort analogue of `restore_run`.
    /// A member whose channel tool must be re-bound to the reloaded channel is
    /// rebuilt with `build_run_with`, then has its restored ledger swapped in.
    /// `None` when this conversation records no such cohort (no meta file), the
    /// meta is unparsable, or it names an agent this instance may not spawn.
    fn restore_cohort(&self, id: &str, ctx: &ToolCtx) -> Option<Cohort> {
        let (meta_path, channel_path) = {
            let traces = ctx.task_traces.lock().expect("task traces lock");
            (traces.cohort_meta_path(id)?, traces.cohort_channel_path(id))
        };
        let meta: CohortMeta = serde_json::from_slice(&std::fs::read(meta_path).ok()?).ok()?;

        let budget = self.inner.output_budget;
        let blob_dir = cohort_blob_dir(&ctx.scratch_dir, id);
        let channel = Arc::new(Mutex::new(Channel::restore(channel_path, blob_dir, budget)));

        let mut members = Vec::with_capacity(meta.members.len());
        for meta in &meta.members {
            let def = self.inner.def_for(&meta.kind)?;
            let mut def = def.clone();
            def.read_only = true;

            let left = Arc::new(AtomicBool::new(meta.left));
            let channel_tool: Arc<dyn Tool> = Arc::new(ChannelTool::new(
                channel.clone(),
                id.to_string(),
                meta.id.clone(),
                left.clone(),
            ));
            let (agent, mut session, model_name) =
                self.inner
                    .build_run_with(&def, ctx, None, std::slice::from_ref(&channel_tool));

            // A member that had taken a turn replays its whole session from the
            // trace chain; one whose trace is gone (best-effort persistence can
            // fail) restarts with an empty session and re-reads the channel from
            // the top, so it is never left mid-conversation with lost history.
            let mut cursor = meta.cursor;
            if let Some(run_id) = &meta.run_id {
                let restored = ctx
                    .task_traces
                    .lock()
                    .expect("task traces lock")
                    .restore(run_id)
                    .map(|(_, ledger)| ledger);
                match restored {
                    Some(ledger) => {
                        session.ledger = ledger;
                        session.ledger.close_dangling_tool_calls(
                            "No result: the process exited while this call was in flight. Whether \
                             it took effect is unknown — verify before assuming either way.",
                        );
                    }
                    None => cursor = 0,
                }
            }
            members.push(Member {
                id: meta.id.clone(),
                kind: meta.kind.clone(),
                task: meta.task.clone(),
                summary: (!meta.summary.is_empty())
                    .then(|| meta.summary.clone())
                    .unwrap_or_else(|| meta.task.clone()),
                agent,
                session,
                model_name,
                run_id: meta.run_id.clone(),
                cursor,
                seen_roster_revision: meta.seen_roster_revision,
                active: meta.active,
                left,
            });
        }
        Some(Cohort {
            id: id.to_string(),
            scope: ctx.scratch_dir.clone(),
            members,
            channel,
            round: meta.round,
            max_rounds: meta.max_rounds,
            next_index: meta.next_index,
            roster_revision: meta.roster_revision,
            detached_reports: meta.detached_reports,
            call_id: meta.call_id,
            seq: next_cohort_seq(),
        })
    }

    /// Write the cohort's resumable meta state whole to `cohort-<id>.meta.json`.
    /// Best-effort like the trace log: a failed write just means this exit is
    /// not disk-recoverable, never a failed tool call. Called at each pause.
    fn persist_meta(&self, cohort: &Cohort, ctx: &ToolCtx) {
        let Some(path) = ctx
            .task_traces
            .lock()
            .expect("task traces lock")
            .cohort_meta_path(&cohort.id)
        else {
            return;
        };
        let meta = CohortMeta {
            id: cohort.id.clone(),
            call_id: cohort.call_id.clone(),
            round: cohort.round,
            max_rounds: cohort.max_rounds,
            next_index: cohort.next_index,
            roster_revision: cohort.roster_revision,
            detached_reports: cohort.detached_reports,
            members: cohort
                .members
                .iter()
                .map(|member| MemberMeta {
                    id: member.id.clone(),
                    kind: member.kind.clone(),
                    task: member.task.clone(),
                    summary: member.summary.clone(),
                    run_id: member.run_id.clone(),
                    cursor: member.cursor,
                    seen_roster_revision: member.seen_roster_revision,
                    active: member.active,
                    left: member.left.load(Ordering::SeqCst),
                })
                .collect(),
        };
        if let Ok(json) = serde_json::to_string_pretty(&meta) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Drop a finished cohort's meta so it can never be resurrected: its reports
    /// were already returned, and a stale meta would let `resume` rebuild and
    /// re-finalize it. The channel JSONL stays as the discussion's trace.
    fn remove_cohort_meta(&self, id: &str, ctx: &ToolCtx) {
        if let Some(path) = ctx
            .task_traces
            .lock()
            .expect("task traces lock")
            .cohort_meta_path(id)
        {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Read a resumable cohort's transcript straight off disk, without
    /// rebuilding any member session. Gated on the meta file's existence so a
    /// finished cohort (meta removed) reports no readable channel.
    fn disk_transcript(&self, id: &str, ctx: &ToolCtx) -> Option<String> {
        let (meta_path, channel_path) = {
            let traces = ctx.task_traces.lock().expect("task traces lock");
            (
                traces.cohort_meta_path(id)?,
                traces.cohort_channel_path(id)?,
            )
        };
        if !meta_path.exists() {
            return None;
        }
        Some(render_transcript(&load_channel_log(&channel_path)))
    }

    /// Drive the round-robin until it reaches one of the four exits (§5). The
    /// cohort's `round`/`next_index` are preserved across a yield, so a resume
    /// continues the rest of the current round before the next one.
    async fn drive_to_exit(
        &self,
        cohort: &mut Cohort,
        ctx: &ToolCtx,
        cancel: &CancellationToken,
    ) -> CohortOutcome {
        let call_id = cohort.call_id.clone();
        let cohort_id = cohort.id.clone();
        let channel = cohort.channel.clone();
        loop {
            if cohort.round >= cohort.max_rounds || cohort.members.iter().all(|m| !m.active) {
                return CohortOutcome::Done(self.finalize(cohort, ctx, cancel).await);
            }
            channel.lock().expect("cohort channel lock").round = cohort.round;
            while cohort.next_index < cohort.members.len() {
                let index = cohort.next_index;
                cohort.next_index += 1;
                if !cohort.members[index].active {
                    continue;
                }
                let round = cohort.round;
                let roster_revision = cohort.roster_revision;
                let overview = {
                    let member = &cohort.members[index];
                    (member.run_id.is_none() || member.seen_roster_revision != roster_revision)
                        .then(|| Self::overview(&cohort.members, &member.id))
                };
                let (delta, before_len) = {
                    let ch = channel.lock().expect("cohort channel lock");
                    (ch.delta(cohort.members[index].cursor), ch.log.len())
                };
                let prompt =
                    round_prompt(&cohort.members[index], round, overview.as_deref(), &delta);
                let summary = cohort.members[index].summary.clone();

                let member_id = cohort.members[index].id.clone();
                self.announce(cohort, CohortViewPhase::Running(&member_id), ctx);
                let member = &mut cohort.members[index];
                if overview.is_some() {
                    member.seen_roster_revision = roster_revision;
                }
                let resume_of = member.run_id.clone();
                let result = self
                    .inner
                    .drive(
                        &member.agent,
                        &mut member.session,
                        &member.kind,
                        &member.model_name,
                        &prompt,
                        &summary,
                        &call_id,
                        resume_of.as_deref(),
                        Some(CohortMemberRun {
                            cohort_id: cohort_id.clone(),
                            member_id: member_id.clone(),
                        }),
                        ctx,
                        cancel,
                    )
                    .await;
                match result {
                    // Turn-end text is discarded on debate rounds (§5): only
                    // channel_post is read by anyone.
                    Ok(outcome) => {
                        if member.run_id.is_none() {
                            member.run_id = Some(outcome.run_id);
                        }
                    }
                    // A failed member turn drops it from the rounds and yields
                    // so the parent knows; it still finalizes a report (§8/§9).
                    Err(fail) => {
                        if cohort.members[index].run_id.is_none() {
                            cohort.members[index].run_id = Some(fail.run_id);
                        }
                        cohort.members[index].active = false;
                        cohort.roster_revision += 1;
                        self.announce(cohort, CohortViewPhase::Waiting, ctx);
                        return CohortOutcome::Stalled {
                            member: cohort.members[index].id.clone(),
                            reason: fail.reason,
                        };
                    }
                }
                let after_len = {
                    let ch = channel.lock().expect("cohort channel lock");
                    ch.log.len()
                };
                cohort.members[index].cursor = after_len;
                if cohort.members[index].left.load(Ordering::SeqCst) && cohort.members[index].active
                {
                    cohort.members[index].active = false;
                    cohort.roster_revision += 1;
                }
                self.announce(cohort, CohortViewPhase::Waiting, ctx);
                if cancel.is_cancelled() {
                    return CohortOutcome::Cancelled;
                }
                // A member addressing the parent this turn yields the cohort so
                // the parent can answer in its own loop (§9). The asker's turn
                // already finished; the reply reaches it next round.
                let questions: Vec<String> = {
                    let ch = channel.lock().expect("cohort channel lock");
                    ch.log[before_len..after_len]
                        .iter()
                        .filter(|msg| {
                            msg.from == cohort.members[index].id
                                && msg.to.as_deref() == Some("parent")
                        })
                        .map(|msg| msg.body.clone())
                        .collect()
                };
                if !questions.is_empty() {
                    return CohortOutcome::AskParent {
                        member: cohort.members[index].id.clone(),
                        questions,
                    };
                }
            }
            cohort.next_index = 0;
            cohort.round += 1;
        }
    }

    /// Finalize: every member — active or already left — writes its own report
    /// from the whole channel plus its private exploration (§8). Never yields;
    /// a member that fails here degrades to its last channel message. Each
    /// report is remembered in the shared store so the parent can `attach` it.
    async fn finalize(
        &self,
        cohort: &mut Cohort,
        ctx: &ToolCtx,
        cancel: &CancellationToken,
    ) -> Vec<MemberReport> {
        let call_id = cohort.call_id.clone();
        let cohort_id = cohort.id.clone();
        let channel = cohort.channel.clone();
        let scope = cohort.scope.clone();
        let rounds_run = cohort.round;
        self.announce(cohort, CohortViewPhase::Finalizing, ctx);
        let mut reports = Vec::with_capacity(cohort.members.len());
        for member in &mut cohort.members {
            channel.lock().expect("cohort channel lock").round = rounds_run;
            let delta = {
                let ch = channel.lock().expect("cohort channel lock");
                ch.delta(member.cursor)
            };
            let mut prompt = String::new();
            if !delta.is_empty() {
                prompt.push_str("Final channel activity:\n");
                prompt.push_str(&fence_channel(&delta));
                prompt.push('\n');
            }
            prompt.push_str(FINALIZE_PROMPT);
            let summary = member.summary.clone();

            let resume_of = member.run_id.clone();
            let result = self
                .inner
                .drive(
                    &member.agent,
                    &mut member.session,
                    &member.kind,
                    &member.model_name,
                    &prompt,
                    &summary,
                    &call_id,
                    resume_of.as_deref(),
                    Some(CohortMemberRun {
                        cohort_id: cohort_id.clone(),
                        member_id: member.id.clone(),
                    }),
                    ctx,
                    cancel,
                )
                .await;
            let report = match result {
                Ok(outcome) => outcome.report,
                Err(_) => {
                    let ch = channel.lock().expect("cohort channel lock");
                    ch.log
                        .iter()
                        .rev()
                        .find(|msg| msg.from == member.id)
                        .map(|msg| msg.body.clone())
                        .unwrap_or_else(|| "(no report)".to_string())
                }
            };
            member.cursor = channel.lock().expect("cohort channel lock").log.len();
            // Remembered under the member's stable run id in the shared reports
            // map, so `agent(attach=[<run id>])` splices it verbatim later.
            if let Some(run_id) = &member.run_id {
                self.inner
                    .remember_report(run_id, &member.kind, &report, &scope);
            }
            reports.push(MemberReport {
                member: member.id.clone(),
                kind: member.kind.clone(),
                run_id: member.run_id.clone(),
                report,
            });
        }
        self.announce(cohort, CohortViewPhase::Done, ctx);
        reports
    }

    /// Park a paused cohort, evicting the oldest beyond cap and dropping any
    /// cohort from a conversation this tool has since moved off (same rule as
    /// `AgentTool::park`).
    fn park_cohort(&self, cohort: Cohort) {
        let mut map = self.cohorts.lock().expect("cohorts lock");
        map.retain(|_, parked| parked.scope == cohort.scope);
        if !map.contains_key(&cohort.id) && map.len() >= MAX_LIVE_COHORTS {
            if let Some(oldest) = map
                .values()
                .min_by_key(|parked| parked.seq)
                .map(|parked| parked.id.clone())
            {
                map.remove(&oldest);
            }
        }
        map.insert(cohort.id.clone(), cohort);
    }

    /// Take a paused cohort out for resumption, honoring the issuing scope.
    fn take_cohort(&self, id: &str, scope: &Path) -> Option<Cohort> {
        let mut map = self.cohorts.lock().expect("cohorts lock");
        map.get(id)
            .is_some_and(|cohort| cohort.scope == scope)
            .then(|| map.remove(id))
            .flatten()
    }

    /// Move completed members with a follow-up budget into the shared `live`
    /// store. Their debate-only `channel` tool cannot survive the cohort: it
    /// would still write to an orphaned log no scheduler reads. Rebuild the
    /// agent's normal toolset while preserving the completed session and its
    /// cache scope, so `agent(resume=...)` is a real ordinary follow-up.
    fn park_finalized_members(&self, cohort: &mut Cohort, ctx: &ToolCtx) -> Vec<String> {
        let mut parked = Vec::new();
        for member in &mut cohort.members {
            let Some(run_id) = member.run_id.clone() else {
                continue;
            };
            let Some(def) = self.inner.def_for(&member.kind) else {
                continue;
            };
            if def.max_exchanges == 0 {
                continue;
            }
            let model = member.agent.model.snapshot();
            let (agent, replacement, _) = self.inner.build_run(def, ctx, Some(model));
            let session = std::mem::replace(&mut member.session, replacement);
            self.inner.park(
                &run_id,
                super::LiveTask {
                    agent,
                    session,
                    exchanges_left: def.max_exchanges,
                    def_name: def.name.clone(),
                    model_name: member.model_name.clone(),
                    scope: cohort.scope.clone(),
                    seq: super::next_park_seq(),
                },
            );
            parked.push(run_id);
        }
        parked
    }

    /// Turn a scheduler exit into the tool result. A finished cohort returns its
    /// N reports; a yield parks the cohort and returns how to resume it.
    fn handle_outcome(
        &self,
        mut cohort: Cohort,
        outcome: CohortOutcome,
        ctx: &ToolCtx,
    ) -> ToolOutput {
        match outcome {
            CohortOutcome::Done(reports) => {
                // Finished: its reports are returned and it must not resume.
                self.remove_cohort_meta(&cohort.id, ctx);
                let parked = self.park_finalized_members(&mut cohort, ctx);
                let mut out = format!(
                    "[cohort {}: {} members debated {} round{}; {} independent reports{}]\n",
                    cohort.id,
                    cohort.members.len(),
                    cohort.round,
                    if cohort.round == 1 { "" } else { "s" },
                    reports.len(),
                    if cohort.detached_reports {
                        " kept detached for attach"
                    } else {
                        " below — attach any by its run id"
                    },
                );
                if cohort.detached_reports {
                    for report in &reports {
                        let run = report.run_id.as_deref().unwrap_or("—");
                        out.push_str(&format!(
                            "- {} ({}) · run {run}\n",
                            report.member, report.kind
                        ));
                    }
                } else {
                    for report in &reports {
                        let run = report.run_id.as_deref().unwrap_or("—");
                        out.push_str(&format!(
                            "\n── {} ({}) · run {run} ──\n{}\n",
                            report.member, report.kind, report.report
                        ));
                    }
                }
                if !parked.is_empty() {
                    out.push_str(&format!(
                        "Follow up directly with agent(agent=<kind>, resume=<run id>): {}.\n",
                        parked.join(", ")
                    ));
                }
                ToolOutput::ok(out)
            }
            CohortOutcome::AskParent { member, questions } => {
                let id = cohort.id.clone();
                self.persist_meta(&cohort, ctx);
                self.park_cohort(cohort);
                let mut out = format!("[cohort {id} paused: member {member} asks you:\n");
                for question in &questions {
                    out.push_str(&format!("  • {question}\n"));
                }
                out.push_str(&format!(
                    "Reply with cohort(resume=\"{id}\", answer=\"…\"), or cohort(resume=\"{id}\") to \
                     continue without answering. Read the discussion with \
                     cohort(action=\"channel\", id=\"{id}\").]"
                ));
                ToolOutput::ok(out)
            }
            CohortOutcome::Stalled { member, reason } => {
                let id = cohort.id.clone();
                self.persist_meta(&cohort, ctx);
                self.park_cohort(cohort);
                ToolOutput::ok(format!(
                    "[cohort {id} paused: member {member} failed and left the debate: {reason}\n\
                     Resume with cohort(resume=\"{id}\") to continue with the remaining members; \
                     the failed member still writes a final report.]"
                ))
            }
            CohortOutcome::Cancelled => ToolOutput::err("cohort cancelled by user"),
        }
    }
}

/// The user-turn prompt for one debate-round activation. The first activation
/// carries the shared discipline and the cohort overview; later ones append
/// only the new channel delta, keeping the member's prefix cache-stable.
fn round_prompt(
    member: &Member,
    round: usize,
    overview: Option<&str>,
    delta: &[ChannelMsg],
) -> String {
    if member.run_id.is_none() {
        let overview = overview.expect("first cohort activation always includes the roster");
        let mut prompt = format!(
            "{MEMBER_PREAMBLE}\n\n{overview}\nYour assigned task:\n{}\n",
            member.task
        );
        if !delta.is_empty() {
            prompt.push_str("\nChannel so far:\n");
            prompt.push_str(&fence_channel(delta));
        }
        return prompt;
    }

    let mut prompt = format!("Round {round}.\n");
    if let Some(overview) = overview {
        prompt.push_str("Cohort roster changed:\n");
        prompt.push_str(overview);
    }
    if delta.is_empty() {
        prompt.push_str(
            "No new channel messages since your last turn. Post if you have something to add, \
             or use channel_leave if you are done.\n",
        );
    } else {
        prompt.push_str("New channel activity:\n");
        prompt.push_str(&fence_channel(delta));
    }
    prompt
}
