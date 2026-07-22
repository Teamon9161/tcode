//! Switching what the screen is a view of: the main conversation, a
//! sub-agent's trace, or a resumed / imported session.
//!
//! Every view is a `Transcript`, so `active_transcript` is the single place
//! deciding which one scrolling, selection and copy act on — no other
//! consumer branches on the active view.
//!
//! Touches: active_view, trace_view, transcript, session, task_runs,
//! task_trace_root, external_import, overlay.

use super::*;

/// Lazily materialized task transcript. It is rebuilt from the durable trace
/// or from retained live events each time it is opened, keeping only one extra
/// transcript resident while the main conversation continues in the background.
pub(super) struct TraceView {
    pub(super) run: String,
    pub(super) view: SessionView,
    pub(super) consumed: usize,
}

pub(super) fn task_trace_root(session: &Session) -> Option<PathBuf> {
    tcode_core::store::project_data_dir(&session.tool_ctx.cwd).map(|root| {
        root.join("tasks")
            .join(session.prompt_variables().session_id())
    })
}

pub(super) fn discover_task_runs(root: Option<&std::path::Path>) -> Vec<UiTaskRun> {
    let metas = root
        .map(tcode_core::TaskTraces::discover)
        .unwrap_or_default();
    metas
        .into_iter()
        .map(|meta| {
            let mut run = UiTaskRun::new(
                meta.id,
                meta.parent_call,
                meta.kind,
                meta.model,
                meta.prompt,
                meta.summary,
                None,
            );
            run.status = meta.status;
            run.tools = meta.tool_calls;
            run.usage = meta.usage;
            run
        })
        .collect()
}

impl App {
    /// `/clear`, resume and import restart the visual conversation. The
    /// transcript is ours, so this is a plain reset — no terminal purge.
    pub(super) fn clear_conversation_screen(&mut self) {
        self.transcript.clear();
        self.live_text.clear();
        self.live_block = None;
        let banner = self.banner();
        self.bake(banner);
    }

    /// The ledger was cleared or replaced: drop turn-scoped UI state and
    /// restart the visual conversation. Shared by /clear, /resume and
    /// external import.
    pub(super) fn reset_conversation_ui(&mut self) {
        self.drop_suggestion();
        if let Some(session) = self.session.as_mut() {
            if session.last_prompt_tokens == 0 && !session.ledger.is_empty() {
                session.last_prompt_tokens = self.agent.estimate_context_tokens(session);
            }
            let estimated = !session.ledger.is_empty();
            let tokens = session.last_prompt_tokens;
            self.meter.set_context(tokens, estimated);
        } else {
            self.meter.set_context(0, false);
        }
        self.meter.forget_cache_baseline();
        self.progress.clear();
        self.task_trace_root = self.session.as_ref().and_then(task_trace_root);
        self.task_runs = discover_task_runs(self.task_trace_root.as_deref());
        self.active_view = ViewId::Main;
        self.trace_view = None;
        // A view picker naming the discarded conversation must not survive it.
        if matches!(self.overlay, Some(Overlay::View(_))) {
            self.overlay = None;
        }
        self.pending_tool = None;
        self.pending_batch.clear();
        self.thinking_text.clear();
        self.clear_conversation_screen();
    }

    /// Resume picker selections route through the same registry command as
    /// a typed `/resume <id>`.
    pub(super) fn resume_session(&mut self, id: &str) {
        self.run_slash(&format!("/resume {}", id.trim()));
    }

    pub(super) fn open_resume_picker(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let Some(data_dir) = tcode_core::store::project_data_dir(&session.tool_ctx.cwd) else {
            self.reply("cannot locate tcode session storage");
            return;
        };
        match tcode_core::SessionStore::list(&data_dir) {
            Ok(sessions) => self.overlay = Some(Overlay::Resume(resume::Picker::new(sessions))),
            // External import is useful even before tcode itself has stored a
            // prior conversation in this project.
            Err(tcode_core::store::StoreError::NoSession) => {
                self.overlay = Some(Overlay::Resume(resume::Picker::new(Vec::new())))
            }
            Err(e) => self.reply_error(format!("cannot list resumable sessions: {e}")),
        }
    }

    pub(super) fn open_external_resume_picker(&mut self, source: ExternalSource) {
        let sessions = list_external_sessions(&self.cwd, source);
        match resume::Picker::external(source, sessions) {
            Some(picker) => self.overlay = Some(Overlay::Resume(picker)),
            None => {
                self.overlay = None;
                self.reply(format!(
                    "no {} conversations found for this project",
                    source.label()
                ));
            }
        }
    }

    pub(super) fn import_external_session(&mut self, external: ExternalSessionInfo) {
        if matches!(self.phase, Phase::Running { .. }) || self.external_import.is_some() {
            self.reply("wait for the current turn before importing");
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let Some(data_dir) = tcode_core::store::project_data_dir(&session.tool_ctx.cwd) else {
            self.reply("cannot locate tcode session storage");
            return;
        };
        let cwd = session.tool_ctx.cwd.clone();
        let source = external.source;
        self.external_import = Some(tokio::task::spawn_blocking(move || {
            let result = import_external_session(&data_dir, &cwd, &external);
            (source, result)
        }));
        self.state_label = format!("importing {} conversation", source.label());
    }

    pub(super) fn on_external_import_done(
        &mut self,
        (source, result): (
            ExternalSource,
            Result<tcode_core::Resumed, tcode_core::store::StoreError>,
        ),
    ) {
        self.external_import = None;
        self.state_label.clear();
        let opening_context = self.opening_context.clone();
        let environment = self.environment.clone();
        let Some(session) = self.session.as_mut() else {
            return;
        };
        match result {
            Ok(resumed) => {
                let imported_id = resumed.store.id.clone();
                session.checkpoints = tcode_core::CheckpointStore::default();
                session.ledger = resumed.ledger;
                session.ledger.attach_sink(Box::new(resumed.store));
                session.bind_scratch_session(&imported_id);
                let opening = opening_context(&session.tool_ctx.cwd, &session.tool_ctx.scratch_dir);
                session.set_startup_context(opening);
                session.sync_environment(environment(&session.tool_ctx.cwd), None);
                self.scratch_dir = session.tool_ctx.scratch_dir.clone();
                session.last_prompt_tokens = 0;
                session
                    .tool_ctx
                    .freshness
                    .lock()
                    .expect("freshness lock")
                    .clear();
                self.reset_conversation_ui();
                self.reply(format!(
                    "imported {} as tcode session {imported_id}",
                    source.label()
                ));
                self.bake_transcript();
            }
            Err(e) => self.reply_error(format!("cannot import external session: {e}")),
        }
    }

    pub(super) fn active_transcript(&self) -> &Transcript {
        match (&self.active_view, &self.trace_view) {
            (ViewId::TaskRun(id), Some(trace)) if trace.run == *id => &trace.view.transcript,
            _ => &self.transcript,
        }
    }

    pub(super) fn active_transcript_mut(&mut self) -> &mut Transcript {
        match (&self.active_view, &mut self.trace_view) {
            (ViewId::TaskRun(id), Some(trace)) if trace.run == *id => &mut trace.view.transcript,
            _ => &mut self.transcript,
        }
    }

    pub(super) fn view_entries(&self) -> Vec<view_picker::ViewEntry> {
        // Task traces are navigated from their task header or the live agent
        // tree. `/views` deliberately reserves this picker for concurrent
        // top-level sessions, whose registry has not been introduced yet.
        vec![view_picker::ViewEntry {
            id: ViewId::Main,
            title: "current session".into(),
            detail: if matches!(self.phase, Phase::Running { .. }) {
                "running".into()
            } else {
                "idle".into()
            },
            active: self.active_view == ViewId::Main,
        }]
    }

    pub(super) fn open_view_picker(&mut self) {
        let entries = self.view_entries();
        if entries.len() < 2 {
            self.notice = Some(("no other active sessions".into(), Instant::now()));
            return;
        }
        self.overlay = view_picker::Picker::new(entries, &self.active_view).map(Overlay::View);
    }

    pub(super) fn open_view(&mut self, id: ViewId) {
        if id == ViewId::Main {
            self.active_view = ViewId::Main;
            self.trace_view = None;
            self.drag_scroll = None;
            return;
        }
        let run_id = match &id {
            ViewId::TaskRun(run_id) => run_id.clone(),
            ViewId::Main => return,
        };
        let Some(run) = self.task_runs.iter().find(|run| run.id == run_id) else {
            self.notice = Some(("task trace is no longer available".into(), Instant::now()));
            return;
        };
        let width = area_width(&self.terminal);
        let mut view = SessionView::new(width);
        let status = run.status;
        let header = vec![Line::from(vec![
            Span::styled(
                format!("{} {} {}", status_icon(status), run.id, run.kind),
                theme::bold(),
            ),
            Span::styled(
                format!(" · {} · {} tools", run.model, run.tools),
                theme::dim(),
            ),
            Span::styled(format!(" · {}", status.label()), theme::dim()),
        ])];
        let prompt = run.prompt.clone();
        let events = run.events.clone();
        let path = self
            .task_trace_root
            .as_ref()
            .map(|root| root.join(format!("{}.jsonl", run.id)));
        let mut ctx = BakeCtx {
            renderers: &self.renderers,
            markdown: &mut self.md,
            cwd: &self.cwd,
            show_reasoning: self.show_reasoning,
        };
        view.bake(header);
        view.transcript.push(prompt_echo(&prompt, &[]));
        let mut consumed = 0;
        if status != tcode_core::TaskRunStatus::Running {
            if let Some(path) = path.filter(|path| path.exists()) {
                match tcode_core::TaskTraces::load(&path) {
                    Ok(load) => {
                        view.replay_task_ledger(load.ledger.entries(), &load.batch_labels, &mut ctx)
                    }
                    Err(error) => view.bake(vec![Line::styled(
                        format!("could not load trace: {error}"),
                        theme::error_highlight(),
                    )]),
                }
            } else {
                for event in &events {
                    view.feed_event(event, &mut ctx);
                }
                consumed = events.len();
            }
        } else {
            for event in &events {
                view.feed_event(event, &mut ctx);
            }
            consumed = events.len();
        }
        self.active_view = id;
        self.trace_view = Some(TraceView {
            run: run_id.clone(),
            view,
            consumed,
        });
        self.drag_scroll = None;
    }

    pub(super) fn refresh_open_trace(&mut self, run: &str) {
        let Some(trace) = self.trace_view.as_mut().filter(|trace| trace.run == run) else {
            return;
        };
        let Some(task) = self.task_runs.iter().find(|task| task.id == run) else {
            return;
        };
        let events: Vec<AgentEvent> = task.events.iter().skip(trace.consumed).cloned().collect();
        if events.is_empty() {
            return;
        }
        let mut ctx = BakeCtx {
            renderers: &self.renderers,
            markdown: &mut self.md,
            cwd: &self.cwd,
            show_reasoning: self.show_reasoning,
        };
        for event in &events {
            trace.view.feed_event(event, &mut ctx);
        }
        trace.consumed += events.len();
    }

    pub(super) fn finish_open_trace(&mut self, run: &str) {
        let Some(trace) = self.trace_view.as_mut().filter(|trace| trace.run == run) else {
            return;
        };
        let mut ctx = BakeCtx {
            renderers: &self.renderers,
            markdown: &mut self.md,
            cwd: &self.cwd,
            show_reasoning: self.show_reasoning,
        };
        trace.view.finish(&mut ctx);
    }
}
