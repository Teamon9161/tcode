//! Slash commands and the pickers they open.
//!
//! Command *semantics* live in core's `CommandRegistry` so the TUI and the
//! REPL share one implementation. What is left here is the frontend's half:
//! interpreting `CommandEffect`, and driving the pickers that manipulate
//! frontend-owned objects (`/model`, `/agents`) and so cannot live in core.
//!
//! Touches: registry, skills, session, overlay, menu, agents, committed_mode,
//! pending_mode, mode_label, dogfood.

use super::*;

impl App {
    /// Apply a direct permission-mode choice. The running agent owns the
    /// Session, so a live turn receives only a staged target and commits it at
    /// the next tool-batch boundary.
    pub(super) fn set_mode(&mut self, next: tcode_core::PermissionMode) {
        self.persist_mode(next);
        match self.session.as_mut() {
            Some(session) => {
                session.mode = next;
                self.committed_mode = next;
                self.pending_mode.clear();
                self.mode_label = next.label().to_string();
            }
            None => {
                self.pending_mode.set(next);
                // A staged target shows with an arrow so the user is never
                // misled into thinking the plan gate is already active while
                // the current batch still runs under the old mode.
                self.mode_label = format!("→ {}", next.label());
            }
        }
    }

    /// shift+tab cycles the permission mode; status-bar selection calls the
    /// same setter so persistence and running-turn staging cannot drift.
    pub(super) fn cycle_mode(&mut self) {
        let base = self.pending_mode.get().unwrap_or(self.committed_mode);
        self.set_mode(base.cycle());
    }

    /// Persist the chosen mode as the default for new sessions — except Unsafe:
    /// a one-off flip to it must not silently arm every future session, so
    /// landing there clears the stored choice instead.
    pub(super) fn persist_mode(&self, mode: tcode_core::PermissionMode) {
        tcode_core::config::ModelState::update(|state| {
            state.mode = (mode != tcode_core::PermissionMode::Unsafe).then_some(mode);
        });
    }

    pub(super) fn apply_folder_trust_choice(&mut self, choice: crate::folder_trust_picker::Choice) {
        let (trust, remember) = crate::folder_trust_picker::outcome(choice);
        let cwd = self.cwd.clone();
        if let Some(session) = self.session.as_mut() {
            session.set_folder_trust(trust);
        }
        let persistence = if remember {
            match tcode_core::config::ModelState::update_checked(|state| {
                state.set_folder_trust(&cwd, trust)
            }) {
                Ok(()) => " and remembered on this machine".to_string(),
                Err(error) => format!(" for this session only (could not remember: {error})"),
            }
        } else {
            " for this session only".to_string()
        };
        let label = match trust {
            FolderTrust::Trusted => "trusted",
            FolderTrust::Untrusted => "not trusted",
        };
        self.bake(vec![Line::styled(
            format!("folder {label}{persistence}: {}", cwd.display()),
            theme::dim(),
        )]);
    }

    pub(super) fn refresh_folder_trust(&mut self) {
        let remembered = tcode_core::config::ModelState::load().folder_trust_for(&self.cwd);
        let Some(session) = self.session.as_mut() else {
            return;
        };
        match remembered {
            Some(trust) => {
                session.set_folder_trust(trust);
                if matches!(self.overlay, Some(Overlay::FolderTrust(_))) {
                    self.overlay = None;
                }
            }
            None => {
                session.clear_folder_trust();
                self.overlay = Some(Overlay::FolderTrust(
                    crate::folder_trust_picker::Picker::new(&self.cwd),
                ));
            }
        }
    }

    pub(super) fn open_mode_picker(&mut self) {
        let current = self.pending_mode.get().unwrap_or(self.committed_mode);
        self.overlay = Some(Overlay::Mode(crate::mode_picker::Picker::new(current)));
    }

    pub(super) fn open_model_picker(&mut self) {
        let effort = self.agent.model.snapshot().effort;
        match model_picker::Picker::new(&self.menu, effort.as_deref()) {
            Some(picker) => self.overlay = Some(Overlay::Model(picker)),
            None => self.bake(vec![Line::styled(
                "no models configured — edit ~/.tcode/config.toml",
                theme::dim(),
            )]),
        }
    }

    /// `/provider`: edit credentials without leaving the conversation. Seeded
    /// with the user's own config only — a project overlay must never be
    /// copied into `~/.tcode/config.toml`.
    pub(super) fn open_provider_setup(&mut self) {
        let profile = self
            .menu
            .options
            .get(self.menu.current)
            .map(|opt| opt.profile.clone());
        match (self.provider_setup.load)() {
            Ok(global) => {
                let setup = crate::setup::Setup::new(
                    global,
                    profile.as_deref(),
                    tcode_core::config::ModelState::load(),
                );
                self.overlay = Some(Overlay::Provider(Box::new(setup)));
            }
            Err(e) => self.bake(vec![Line::styled(
                format!("cannot read the global config: {e}"),
                ratatui::style::Style::default().fg(theme::ERROR),
            )]),
        }
    }

    /// Persist a finished setup and take the rebuilt menus. The provider swap
    /// happens inside the closure, so a running turn keeps its snapshot
    /// exactly as a `/model` switch does.
    pub(super) fn apply_setup(
        &mut self,
        done: Box<(tcode_core::config::Config, tcode_core::config::ModelState)>,
    ) {
        let (config, state) = *done;
        match (self.provider_setup.apply)(config, state) {
            Ok((menu, agents)) => {
                self.menu = menu;
                self.agents = agents;
                let current = self
                    .menu
                    .options
                    .get(self.menu.current)
                    .map(|opt| format!("{} · {}", opt.profile, opt.def.display()))
                    .unwrap_or_else(|| "no models configured".into());
                self.bake(vec![Line::styled(
                    format!("providers configured — {current} · /model to switch"),
                    theme::dim(),
                )]);
            }
            Err(e) => self.bake(vec![Line::styled(
                format!("cannot save provider setup: {e}"),
                ratatui::style::Style::default().fg(theme::ERROR),
            )]),
        }
    }

    /// Hot-swap the shared ModelCell; a running turn finishes on its
    /// snapshot, the next request uses the new model.
    pub(super) fn apply_model(&mut self, index: usize, effort: Option<String>) {
        let Some(opt) = self.menu.options.get(index) else {
            return;
        };
        match (self.menu.switch)(opt, effort.as_deref()) {
            Ok(active) => {
                let label = active.describe();
                let name = active.provider.name().to_string();
                self.agent.model.swap(active);
                self.menu.current = index;
                self.bake(vec![Line::styled(
                    format!("model → {name} · {label}"),
                    theme::dim(),
                )]);
            }
            Err(e) => self.bake(vec![Line::styled(
                format!("cannot switch model: {e}"),
                ratatui::style::Style::default().fg(theme::ERROR),
            )]),
        }
    }

    /// `/agents`: apply a role's explicit mode. The binary performs the live
    /// registry update and persistence; the TUI only mirrors its result.
    pub(super) fn apply_agent_model(&mut self, kind: &str, choice: model_picker::AgentModelChoice) {
        match (self.agents.pin)(kind, choice.clone()) {
            Ok(label) => {
                if let Some(slot) = self
                    .agents
                    .roles
                    .iter()
                    .position(|role| role.key == kind)
                    .map(|i| &mut self.agents.pins[i])
                {
                    *slot = choice;
                }
                self.bake(vec![Line::styled(
                    format!("{kind} → {label}"),
                    theme::dim(),
                )]);
            }
            Err(e) => self.bake(vec![Line::styled(
                format!("cannot configure {kind}: {e}"),
                ratatui::style::Style::default().fg(theme::ERROR),
            )]),
        }
    }

    pub(super) fn run_slash(&mut self, cmd: &str) {
        // UI-only commands: their substance drives frontend-owned objects
        // (key table, model picker, provider wizard), so they never reach
        // the shared registry.
        match cmd {
            "/help" => {
                self.show_help();
                return;
            }
            "/views" => {
                self.open_view_picker();
                return;
            }
            "/provider" => {
                self.open_provider_setup();
                return;
            }
            "/model" => {
                self.open_model_picker();
                return;
            }
            "/agents" => {
                self.overlay = model_picker::AgentPicker::new(&self.agents).map(Overlay::Agent);
                return;
            }
            _ => {}
        }
        // `/voice` is the one UI command with arguments, because picking a key
        // that this terminal and this input method both leave alone is
        // trial and error and has to be fast.
        if let Some(rest) = cmd
            .strip_prefix("/voice ")
            .or_else(|| (cmd == "/voice").then_some(""))
        {
            self.run_voice(rest.trim());
            return;
        }
        if self.registry.find(cmd).is_none() && self.dispatch_skill(cmd) {
            return;
        }
        let Some(command) = self.registry.find(cmd) else {
            self.bake(vec![Line::styled(
                format!("unknown command {cmd} — /help lists commands"),
                theme::dim(),
            )]);
            return;
        };
        if self.session.is_none() {
            // A running turn owns the session. /cost stays answerable from
            // the UI's own tally; everything else waits.
            if command.name() == "cost" {
                let u = self.meter.turn;
                self.bake(vec![Line::styled(
                    format!(
                        "last turn: in {} | out {} | cache r {} w {}",
                        u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cache_write_tokens
                    ),
                    theme::dim(),
                )]);
            } else {
                self.bake(vec![Line::styled(
                    "wait for the current turn to finish",
                    theme::dim(),
                )]);
            }
            return;
        }
        let session = self.session.as_mut().expect("checked above");
        let mut ctx = CommandCtx {
            session,
            opening_context: &self.opening_context,
            environment: &self.environment,
            turn_usage: self.meter.turn,
        };
        let outcome = self
            .registry
            .dispatch(&mut ctx, cmd)
            .expect("command found above");
        self.apply_command_outcome(outcome);
    }

    /// Slash lines that miss both `UI_COMMANDS` and the shared registry fall
    /// back to the skill table — `/name` becomes shorthand for loading that
    /// skill, matching Claude Code. Returns `false` for a genuinely unknown
    /// command, so the caller still reports it.
    ///
    /// This runs even while a turn is in flight: unlike registry commands
    /// (which need `&mut Session`, unavailable to the frontend mid-turn) a
    /// skill invocation is just a prompt, submitted through the same queue a
    /// typed message would use.
    pub(super) fn dispatch_skill(&mut self, cmd: &str) -> bool {
        let rest = cmd.trim_start_matches('/');
        let (name, args) = match rest.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args.trim()),
            None => (rest, ""),
        };
        let Some(skill) = self.skills.iter().find(|skill| skill.name == name) else {
            return false;
        };
        // Small, one-off, user-initiated read outside any tool batch: a
        // blocking std::fs read here does not touch the parallel-batch path
        // the async-IO rule guards (`tool.run` keeps using `tokio::fs`).
        let body = match &skill.source {
            tcode_tools::SkillSource::Dir(dir) => {
                match std::fs::read_to_string(dir.join("SKILL.md")) {
                    Ok(body) => body,
                    Err(e) => {
                        self.bake(vec![Line::styled(
                            format!("cannot read {}: {e}", dir.join("SKILL.md").display()),
                            ratatui::style::Style::default().fg(theme::ERROR),
                        )]);
                        return true;
                    }
                }
            }
            tcode_tools::SkillSource::Builtin(body) => body.to_string(),
        };
        let rendered = tcode_tools::render_skill(skill, &body, args, &self.cwd, &self.scratch_dir);
        let wrapped = tcode_tools::wrap_skill_echo(name, args, &rendered);
        let message = self.compose_draft(wrapped);
        if matches!(self.phase, Phase::Running { .. }) {
            self.pending.push(message);
        } else {
            self.transcript.scroll_to_bottom();
            self.start_turn(message);
        }
        true
    }

    pub(super) fn show_help(&mut self) {
        let mut lines: Vec<Line> = vec![Line::styled("keys:", theme::bold().fg(theme::ACCENT))];
        for (k, d) in [
            ("enter", "send · during a turn: queue · shift+enter newline"),
            (
                "esc",
                "take back a queued prompt / cancel turn / clear input",
            ),
            ("shift+tab", "cycle permission mode"),
            (
                "ctrl+v / alt+v",
                "paste (images/long text become inline tokens)",
            ),
            ("ctrl+a", "select prompt · ctrl+c copy selection"),
            ("alt+c / alt+x", "copy / cut prompt"),
            (
                "mouse",
                "click mode/model to switch · click prompt to move cursor · drag to copy",
            ),
            ("backspace", "delete · after an [attachment] token drops it"),
            (
                "ctrl+c",
                "interrupt turn (sends anything queued) / clear input",
            ),
            ("ctrl+d", "quit · /exit also works"),
        ] {
            lines.push(Line::styled(format!("  {k:<16} {d}"), theme::dim()));
        }
        lines.push(Line::styled("commands:", theme::bold().fg(theme::ACCENT)));
        for (c, d) in UI_COMMANDS {
            lines.push(Line::styled(format!("  {c:<16} {d}"), theme::dim()));
        }
        for (c, d) in self.registry.entries() {
            lines.push(Line::styled(format!("  {c:<16} {d}"), theme::dim()));
        }
        for skill in &self.skills {
            let command = format!("/{}", skill.name);
            let description = clip_description(&skill.description, 100);
            lines.push(Line::styled(
                format!("  {command:<16} {description}"),
                theme::dim(),
            ));
        }
        self.bake(lines);
    }

    /// Interpret a command's effects, then bake its messages. Effects run
    /// first: /clear must wipe the screen before "conversation cleared"
    /// appears in the fresh transcript.
    pub(super) fn apply_command_outcome(&mut self, outcome: tcode_core::commands::CommandOutcome) {
        for effect in outcome.effects {
            match effect {
                CommandEffect::Exit => self.should_exit = true,
                CommandEffect::Compact { focus } => self.start_compact(focus),
                CommandEffect::ConversationCleared => self.reset_conversation_ui(),
                CommandEffect::ConversationReplaced => {
                    self.reset_conversation_ui();
                    self.bake_transcript();
                }
                CommandEffect::OpenResumePicker => self.open_resume_picker(),
                CommandEffect::PersistDogfood(on) => {
                    tcode_core::config::ModelState::update(|state| state.dogfood = on)
                }
                CommandEffect::PersistSuggestions(on) => {
                    tcode_core::config::ModelState::update(|state| state.suggestions = Some(on));
                    // Off means the pending guess is stale; on means the next
                    // turn's end starts one.
                    self.drop_suggestion();
                }
            }
        }
        for message in outcome.messages {
            self.bake_command_message(message);
        }
        // Cheap mirror sync instead of per-command effects: a command may
        // have moved the cwd (/cd) or cycled the permission mode (/mode).
        let old_cwd = self.cwd.clone();
        if let Some(session) = self.session.as_ref() {
            self.cwd = session.tool_ctx.cwd.clone();
            self.scratch_dir = session.tool_ctx.scratch_dir.clone();
            self.mode_label = session.mode.label().to_string();
            self.committed_mode = session.mode;
            self.pending_mode.clear();
            self.dogfood = session.dogfood();
        }
        if self.cwd != old_cwd {
            self.reference_index.clear();
            self.refresh_reference_index();
            self.refresh_folder_trust();
        }
    }

    /// `/voice [on|off|keys|key <name>|model [<name>]]`.
    fn run_voice(&mut self, args: &str) {
        match args {
            // Bare `model` opens the picker. Remembering which models exist is
            // the machine's job, not the user's — and the list has to come from
            // the installed sidecar anyway.
            "model" | "models" => self.open_voice_model_picker(),
            "" => {
                let on = !self.voice.is_on();
                self.set_voice(on);
            }
            "on" => self.set_voice(true),
            "off" => self.set_voice(false),
            "keys" => {
                self.voice_probe = !self.voice_probe;
                self.bake(vec![Line::styled(
                    if self.voice_probe {
                        "key probe on — every keystroke is echoed below. Press the key you want \
                         for voice: if nothing appears, something above tcode (an input method, \
                         the terminal, the window manager) is taking it. /voice keys turns this \
                         off."
                    } else {
                        "key probe off"
                    },
                    theme::dim(),
                )]);
            }
            other if other.starts_with("model ") => {
                self.apply_voice_model(other["model ".len()..].trim().to_string())
            }
            "words" => self.show_voice_words(),
            other if other.starts_with("words ") => self.edit_voice_words(&other["words ".len()..]),
            other => {
                let Some(name) = other.strip_prefix("key ") else {
                    self.bake(vec![Line::styled(
                        format!(
                            "usage: /voice [on|off|keys|key <name>|model [<name>]|words [<w>...]] \
                             (got '{other}')"
                        ),
                        ratatui::style::Style::default().fg(theme::ERROR),
                    )]);
                    return;
                };
                match name.trim().parse::<tcode_core::config::VoiceKey>() {
                    Ok(key) => {
                        self.voice.set_key(key);
                        // state.toml, like the permission mode Shift+Tab
                        // lands on: a choice the program made on the user's
                        // behalf and must remember. config.toml stays
                        // hand-written and untouched.
                        tcode_core::config::ModelState::update(|state| state.voice_key = Some(key));
                        self.bake(vec![Line::styled(
                            format!(
                                "voice key is now {} (persists across sessions) — {}",
                                key.label(),
                                self.voice.gesture_help()
                            ),
                            theme::dim(),
                        )]);
                    }
                    Err(reason) => self.bake(vec![Line::styled(
                        reason,
                        ratatui::style::Style::default().fg(theme::ERROR),
                    )]),
                }
            }
        }
    }

    /// `/voice words`. Shows the list, since the words are the whole point of
    /// having it and there is nowhere else they appear.
    pub(super) fn show_voice_words(&mut self) {
        let line = if self.voice.words().is_empty() {
            "no voice hotwords — `/voice words tokio serde` adds some, `-tokio` removes one"
                .to_string()
        } else {
            format!(
                "voice hotwords: {} · `-<word>` removes one",
                self.voice.words().join(" ")
            )
        };
        self.bake(vec![Line::styled(line, theme::dim())]);
    }

    /// `/voice words <w>...`. A leading `-` removes; everything else adds. One
    /// rule rather than add/remove/clear verbs, and no real hotword begins with
    /// a hyphen.
    pub(super) fn edit_voice_words(&mut self, args: &str) {
        let mut words = self.voice.words().to_vec();
        for token in args.split_whitespace() {
            match token.strip_prefix('-') {
                Some(drop) => words.retain(|word| word != drop),
                // Adding twice is not an error, but it must not double the
                // entry: sherpa would then weight that word twice.
                None if words.iter().any(|word| word == token) => {}
                None => words.push(token.to_string()),
            }
        }
        if words == self.voice.words() {
            self.show_voice_words();
            return;
        }
        // Seeded from the effective list, so words written by hand in
        // config.toml survive the first edit made from in here.
        let switched = self.voice.set_words(words.clone());
        tcode_core::config::ModelState::update(|state| state.voice_words = Some(words));
        match switched {
            Ok(()) => self.show_voice_words(),
            Err(reason) => self.bake(vec![Line::styled(
                reason,
                ratatui::style::Style::default().fg(theme::ERROR),
            )]),
        }
    }

    /// `/voice model`. The catalogue is read from the installed sidecar, so the
    /// menu can only ever offer what that binary can actually load.
    pub(super) fn open_voice_model_picker(&mut self) {
        match self.voice.catalogue() {
            Ok(models) => {
                let picker = crate::voice_picker::Picker::new(models, self.voice.model_name());
                self.overlay = Some(Overlay::VoiceModel(picker));
            }
            // Missing or stale sidecar: the reason already carries its own fix.
            Err(reason) => self.bake(vec![Line::styled(
                reason,
                ratatui::style::Style::default().fg(theme::ERROR),
            )]),
        }
    }

    /// Switch models and remember it. Reached from the picker and from
    /// `/voice model <name>`, so both routes persist identically.
    pub(super) fn apply_voice_model(&mut self, name: String) {
        // No list of names here on purpose: the sidecar's table is the only
        // one, and a wrong name comes back from it as a menu.
        let switched = self.voice.set_model(name.clone());
        tcode_core::config::ModelState::update(|state| state.voice_model = Some(name.clone()));
        match switched {
            Ok(()) => {
                self.notice = Some((
                    format!("voice model → {name} (persists across sessions)"),
                    Instant::now(),
                ))
            }
            // First use of a model downloads it, and that is where a bad name
            // surfaces — so this must stay on screen.
            Err(reason) => self.bake(vec![Line::styled(
                reason,
                ratatui::style::Style::default().fg(theme::ERROR),
            )]),
        }
    }

    /// `/voice`. The choice persists like `/suggest`: a setting you must
    /// re-arm every morning is one you stop using.
    pub(super) fn set_voice(&mut self, on: bool) {
        tcode_core::config::ModelState::update(|state| state.voice = Some(on));
        if !on {
            self.voice.turn_off();
            crate::set_key_release_reporting(false);
            self.notice = Some(("voice off".into(), Instant::now()));
            return;
        }
        // Ask for key-release reporting *before* starting: on terminals that
        // need the kitty protocol, the gesture the confirmation line describes
        // depends on whether the request was honoured.
        crate::set_key_release_reporting(true);
        match self.voice.turn_on() {
            // Turning a mode on is not a record worth keeping: the status row
            // shows it is on and which key, so a fading notice is enough.
            // Baking it pushes the conversation up every time.
            Ok(()) => {
                self.notice = Some((
                    format!("voice on — {}", self.voice.gesture_help()),
                    Instant::now(),
                ))
            }
            // A failure is the exception: it says what to do about it, so it
            // has to survive longer than three seconds.
            Err(reason) => self.bake(vec![Line::styled(
                reason,
                ratatui::style::Style::default().fg(theme::ERROR),
            )]),
        }
    }

    pub(super) fn on_voice_event(&mut self, event: VoiceEvent) {
        let outcome = self.voice.on_event(event);
        self.apply_voice(outcome);
    }

    /// Voice's only reach into the app: text goes to the editor, never to the
    /// wire.
    pub(super) fn apply_voice(&mut self, outcome: VoiceOutcome) {
        match outcome {
            VoiceOutcome::None => {}
            VoiceOutcome::Insert(text) => {
                self.dismissed_reference = None;
                self.focus_voice_target();
                if let Some(editor) = self.voice_target() {
                    editor.insert_str(&text);
                }
            }
            VoiceOutcome::Notice(text) => self.notice = Some((text, Instant::now())),
            // Into the transcript: it wraps, it stays, and it is the kind of
            // thing the user has to be able to re-read.
            VoiceOutcome::Announce(text) => self.bake(vec![Line::styled(text, theme::warn())]),
            // The key doubles as a character. It types normally and is taken
            // back only once the hold is proven, so a space stays a space.
            VoiceOutcome::TypeSpace => {
                self.dismissed_reference = None;
                if let Some(editor) = self.voice_target() {
                    editor.insert_char(' ');
                }
            }
            VoiceOutcome::RetractSpaces(count) => {
                if let Some(editor) = self.voice_target() {
                    for _ in 0..count {
                        editor.backspace();
                    }
                }
            }
        }
    }

    /// Where dictation lands: the prompt box, or the approval dialog's own text
    /// field when one is on screen. `None` for the pickers, which have no text
    /// cursor for it to arrive at.
    ///
    /// This is the same question `Dialog::paste_text` answers, and it is
    /// answered in the same place, so pasted and dictated words can never end
    /// up in different fields.
    pub(super) fn voice_target(&mut self) -> Option<&mut crate::editor::Editor> {
        match self.overlay.as_mut() {
            None => Some(&mut self.editor),
            Some(overlay) => overlay.as_dialog_mut().map(|dialog| dialog.text_target()),
        }
    }

    /// True where dictation has somewhere to go. Immutable, because deciding
    /// whether a keystroke is push-to-talk must not move the caret.
    pub(super) fn has_voice_target(&self) -> bool {
        self.overlay
            .as_ref()
            .is_none_or(|overlay| overlay.as_dialog().is_some())
    }

    /// A space key is push-to-talk only at a word boundary in the field the
    /// text would land in — see `Editor::at_word_boundary`.
    pub(super) fn voice_at_boundary(&mut self) -> bool {
        self.voice_target()
            .is_none_or(|editor| editor.at_word_boundary())
    }

    fn focus_voice_target(&mut self) {
        if let Some(dialog) = self.overlay.as_mut().and_then(Overlay::as_dialog_mut) {
            dialog.focus_text_target();
        }
    }

    pub(super) fn bake_command_message(&mut self, message: CommandMessage) {
        let lines = match message.kind {
            MessageKind::Info => message
                .text
                .lines()
                .map(|line| Line::styled(line.to_string(), theme::dim()))
                .collect(),
            MessageKind::Error => vec![Line::styled(
                message.text,
                ratatui::style::Style::default().fg(theme::ERROR),
            )],
            MessageKind::Note => quote_lines(Some(NOTE_LABEL), &message.text),
        };
        self.bake(lines);
    }
}
