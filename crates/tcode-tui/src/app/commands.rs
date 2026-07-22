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
        self.state_store.update(move |state| {
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
            let state_cwd = cwd.clone();
            match self
                .state_store
                .update_checked(move |state| state.set_folder_trust(&state_cwd, trust))
            {
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
        self.reply(format!("folder {label}{persistence}: {}", cwd.display()));
    }

    pub(super) fn refresh_folder_trust(&mut self) {
        let remembered = self
            .state_store
            .load()
            .unwrap_or_default()
            .folder_trust_for(&self.cwd);
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

    /// `/model` and `/agents` open the same hub; the flag only decides which
    /// row it starts on.
    pub(super) fn open_model_hub(&mut self, focus_agents: bool) {
        if self.menu.options.is_empty() {
            self.reply("no models configured — edit the selected config file");
            return;
        }
        self.overlay = Some(Overlay::Model(model_picker::Hub::new(
            &self.agents,
            &self.presets,
            focus_agents,
        )));
    }

    /// Switch the whole line-up. The binary rebuilds the provider and every
    /// pin behind the closure, exactly as a finished `/provider` run does;
    /// the TUI only takes the menus back and says what happened.
    pub(super) fn apply_preset(&mut self, name: &str) {
        match (self.presets.apply)(name) {
            Ok((menu, agents, label)) => {
                self.menu = menu;
                self.agents = agents;
                self.presets.current = self
                    .presets
                    .options
                    .iter()
                    .position(|option| option.key == name);
                self.reply(format!("preset {name} → {label}"));
            }
            Err(e) => self.reply_error(format!("cannot switch to preset '{name}': {e}")),
        }
    }

    /// Capture what is running as a named preset. The draft is assembled here
    /// because only the frontend holds the live pins; naming the profiles and
    /// models behind the indices is the binary's job.
    pub(super) fn save_preset(&mut self, name: &str) {
        let draft = model_picker::PresetDraft {
            main: (!self.menu.options.is_empty()).then_some(self.menu.current),
            main_effort: self.agent.model.snapshot().effort,
            roles: self
                .agents
                .roles
                .iter()
                .zip(&self.agents.pins)
                .map(|(role, pin)| (role.key.clone(), pin.clone()))
                .collect(),
        };
        match (self.presets.save)(name, &draft, &self.menu) {
            Ok((options, current)) => {
                self.presets.options = options;
                self.presets.current = Some(current);
                self.reply(format!("saved preset {name} — /model switches to it"));
            }
            Err(e) => self.reply_error(format!("cannot save preset '{name}': {e}")),
        }
    }

    /// `/provider`: edit credentials without leaving the conversation. Seeded
    /// with the selected user config only — a project overlay must never be
    /// copied into that file.
    pub(super) fn open_provider_setup(&mut self) {
        let profile = self
            .menu
            .options
            .get(self.menu.current)
            .map(|opt| opt.profile.clone());
        match (self.provider_setup.load)() {
            Ok(global) => {
                let setup = crate::setup::Setup::new(global, profile.as_deref());
                self.overlay = Some(Overlay::Provider(Box::new(setup)));
            }
            Err(e) => self.reply_error(format!("cannot read the selected config: {e}")),
        }
    }

    /// Persist a finished setup and take the rebuilt menus. The provider swap
    /// happens inside the closure, so a running turn keeps its snapshot
    /// exactly as a `/model` switch does.
    pub(super) fn apply_setup(&mut self, done: Box<tcode_core::config::Config>) {
        let config = *done;
        match (self.provider_setup.apply)(config) {
            Ok((menu, agents)) => {
                self.menu = menu;
                self.agents = agents;
                let current = self
                    .menu
                    .options
                    .get(self.menu.current)
                    .map(|opt| format!("{} · {}", opt.profile, opt.def.display()))
                    .unwrap_or_else(|| "no models configured".into());
                self.reply(format!(
                    "providers configured — {current} · /model to switch"
                ));
            }
            Err(e) => self.reply_error(format!("cannot save provider setup: {e}")),
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
                self.reply(format!("model → {name} · {label}"));
            }
            Err(e) => self.reply_error(format!("cannot switch model: {e}")),
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
                self.reply(format!("{kind} → {label}"));
            }
            Err(e) => self.reply_error(format!("cannot configure {kind}: {e}")),
        }
    }

    /// What the user typed, then what it answered. A `/name` that resolves to a
    /// skill is deliberately *not* echoed here: it is a prompt, and starting its
    /// turn bakes the same echo through `prompt_echo`.
    pub(super) fn run_slash(&mut self, cmd: &str) {
        if !is_ui_command(cmd) && self.registry.find(cmd).is_none() && self.dispatch_skill(cmd) {
            return;
        }
        self.echo_command(cmd);
        self.dispatch_slash(cmd);
    }

    /// The command line itself, rendered exactly like a prompt: it is one.
    fn echo_command(&mut self, cmd: &str) {
        self.bake_live_text();
        self.finish_thinking();
        self.bake(crate::view::command_echo_lines(cmd));
    }

    /// A command's answer. Every status line a command prints goes through
    /// here (or `reply_error`), so the `⎿` attachment to the echoed command is
    /// decided in one place rather than at forty bake sites.
    pub(super) fn reply(&mut self, text: impl AsRef<str>) {
        self.bake(reply_lines(text.as_ref(), theme::dim()));
    }

    pub(super) fn reply_error(&mut self, text: impl AsRef<str>) {
        self.bake(reply_lines(
            text.as_ref(),
            ratatui::style::Style::default().fg(theme::ERROR),
        ));
    }

    pub(super) fn reply_warn(&mut self, text: impl AsRef<str>) {
        self.bake(reply_lines(text.as_ref(), theme::warn()));
    }

    fn dispatch_slash(&mut self, cmd: &str) {
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
                self.open_model_hub(false);
                return;
            }
            "/agents" => {
                self.open_model_hub(true);
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
        let Some(command) = self.registry.find(cmd) else {
            self.reply(format!("unknown command {cmd} — /help lists commands"));
            return;
        };
        if self.session.is_none() {
            // A running turn owns the session. /cost stays answerable from
            // the UI's own tally; everything else waits.
            if command.name() == "cost" {
                let u = self.meter.turn;
                self.reply(format!(
                    "last turn: in {} | out {} | cache r {} w {}",
                    u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cache_write_tokens
                ));
            } else {
                self.reply("wait for the current turn to finish");
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
                        self.reply_error(format!(
                            "cannot read {}: {e}",
                            dir.join("SKILL.md").display()
                        ));
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
                    self.state_store.update(move |state| state.dogfood = on)
                }
                CommandEffect::PersistSuggestions(on) => {
                    self.state_store
                        .update(move |state| state.suggestions = Some(on));
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
                self.reply(if self.voice_probe {
                    "key probe on — every keystroke is echoed below. Press the key you want \
                         for voice: if nothing appears, something above tcode (an input method, \
                         the terminal, the window manager) is taking it. /voice keys turns this \
                         off."
                } else {
                    "key probe off"
                });
            }
            other if other.starts_with("model ") => {
                self.apply_voice_model(other["model ".len()..].trim().to_string())
            }
            "words" => self.show_voice_words(),
            other if other.starts_with("words ") => self.edit_voice_words(&other["words ".len()..]),
            other => {
                let Some(name) = other.strip_prefix("key ") else {
                    self.reply_error(format!(
                        "usage: /voice [on|off|keys|key <name>|model [<name>]|words [<w>...]] \
                         (got '{other}')"
                    ));
                    return;
                };
                match name.trim().parse::<tcode_core::config::VoiceKey>() {
                    Ok(key) => {
                        self.voice.set_key(key);
                        // The selected config's `[tcode_state]`, like the
                        // permission mode Shift+Tab lands on: a choice the
                        // program made on the user's behalf and must remember.
                        // Runtime updates preserve other handwritten TOML.
                        self.state_store
                            .update(move |state| state.voice_key = Some(key));
                        self.reply(format!(
                            "voice key is now {} (persists across sessions) — {}",
                            key.label(),
                            self.voice.gesture_help()
                        ));
                    }
                    Err(reason) => self.reply_error(reason),
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
        self.reply(line);
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
        self.state_store
            .update(move |state| state.voice_words = Some(words));
        match switched {
            Ok(()) => self.show_voice_words(),
            Err(reason) => self.reply_error(reason),
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
            Err(reason) => self.reply_error(reason),
        }
    }

    /// Switch models and remember it. Reached from the picker and from
    /// `/voice model <name>`, so both routes persist identically.
    pub(super) fn apply_voice_model(&mut self, name: String) {
        // No list of names here on purpose: the sidecar's table is the only
        // one, and a wrong name comes back from it as a menu.
        let switched = self.voice.set_model(name.clone());
        let saved_name = name.clone();
        self.state_store
            .update(move |state| state.voice_model = Some(saved_name));
        match switched {
            Ok(()) => {
                self.notice = Some((
                    format!("voice model → {name} (persists across sessions)"),
                    Instant::now(),
                ))
            }
            // First use of a model downloads it, and that is where a bad name
            // surfaces — so this must stay on screen.
            Err(reason) => self.reply_error(reason),
        }
    }

    /// `/voice`. The choice persists like `/suggest`: a setting you must
    /// re-arm every morning is one you stop using.
    pub(super) fn set_voice(&mut self, on: bool) {
        self.state_store.update(move |state| state.voice = Some(on));
        if !on {
            self.voice.turn_off();
            crate::set_key_release_reporting(false);
            self.notice = Some(("voice off".into(), Instant::now()));
            return;
        }
        self.start_voice(true);
    }

    /// Start voice without changing its persisted preference. Startup uses this
    /// path so a configured, ready backend does not obscure the transcript with
    /// a repeated success notice.
    pub(super) fn start_voice(&mut self, announce: bool) {
        // Ask for key-release reporting *before* starting: on terminals that
        // need the kitty protocol, the gesture the confirmation line describes
        // depends on whether the request was honoured.
        crate::set_key_release_reporting(true);
        // The backend is a separate download, so the first `/voice on` on a
        // machine has nothing to start. Fetch it here rather than making the
        // user read instructions and come back.
        if self.voice.needs_install() {
            self.voice_install_announce = announce;
            self.install_voice_backend();
            return;
        }
        match self.voice.turn_on() {
            // Turning a mode on is not a record worth keeping: the status row
            // shows it is on and which key, so a fading notice is enough.
            // Baking it pushes the conversation up every time.
            Ok(()) if announce => {
                self.notice = Some((
                    format!("voice on — {}", self.voice.gesture_help()),
                    Instant::now(),
                ))
            }
            Ok(()) => {}
            // A failure is the exception: it says what to do about it, so it
            // has to survive longer than three seconds.
            Err(reason) => self.reply_error(reason),
        }
    }

    /// Download the sidecar, then turn voice on. Runs off the UI thread and
    /// reports through the same channel the sidecar itself uses, so the hint
    /// row shows one continuous "getting ready" rather than two mechanisms.
    pub(super) fn install_voice_backend(&mut self) {
        let Some(asset) = crate::voice::release_asset() else {
            self.reply_error(format!(
                "there is no voice backend for {}-{}: the speech library it needs is not \
                     published for this platform.",
                std::env::consts::ARCH,
                std::env::consts::OS
            ));
            return;
        };
        let path = match crate::voice::install_path() {
            Ok(path) => path,
            Err(reason) => {
                self.reply_error(reason);
                return;
            }
        };
        self.voice.begin_install();
        let tx = self.voice.events();
        let watchdog = tx.clone();
        let install = self.voice_install.clone();
        // Blocking: it is a plain synchronous download, and this keeps the
        // injected closure free of an async signature it has no use for.
        let installing = tokio::task::spawn_blocking(move || {
            let progress = {
                let tx = tx.clone();
                // `try_send`, not `blocking_send`: the installer drives its own
                // future here, so this callback runs *inside* a runtime and a
                // blocking send would panic. Dropping a percent when the
                // channel is momentarily full costs nothing — the next chunk
                // reports a fresher number, and the terminal event below is
                // sent outside that future.
                Box::new(move |pct| {
                    let _ = tx.try_send(VoiceEvent::Installing(pct));
                }) as Box<dyn FnMut(u8) + Send>
            };
            let event = match (install.0)(asset, path, progress) {
                Ok(()) => VoiceEvent::Installed,
                Err(reason) => VoiceEvent::Failed(reason),
            };
            let _ = tx.blocking_send(event);
        });
        // The download is the only thing that ever ends the "downloading"
        // state, so a task that dies without sending leaves the hint row at a
        // percentage that will never move again — the one failure mode the
        // user cannot tell from a slow network. Watching the handle turns it
        // back into something with a stated cause and a way out.
        tokio::spawn(async move {
            if installing.await.is_err() {
                let _ = watchdog
                    .send(VoiceEvent::Failed(
                        "the voice backend installer stopped before it finished; \
                         run /voice on to try again"
                            .into(),
                    ))
                    .await;
            }
        });
    }

    pub(super) fn on_voice_event(&mut self, event: VoiceEvent) {
        // The backend has just arrived, so the start that was waiting for it
        // can happen now — and only now is there something to start.
        if matches!(event, VoiceEvent::Installed) {
            let announce = std::mem::replace(&mut self.voice_install_announce, true);
            match self.voice.turn_on() {
                Ok(()) if announce => {
                    self.notice = Some((
                        format!("voice on — {}", self.voice.gesture_help()),
                        Instant::now(),
                    ))
                }
                Ok(()) => {}
                Err(reason) => self.reply_error(reason),
            }
            return;
        }
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
            VoiceOutcome::Announce(text) => self.reply_warn(text),
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

    /// A registry command's own message is an answer like any other, so it
    /// takes the same `⎿` attachment the frontend's replies do. A `Note` is
    /// not: it is text going to the model, and it keeps the user rail.
    pub(super) fn bake_command_message(&mut self, message: CommandMessage) {
        match message.kind {
            MessageKind::Info => self.reply(message.text),
            MessageKind::Error => self.reply_error(message.text),
            MessageKind::Note => {
                let lines = quote_lines(Some(NOTE_LABEL), &message.text);
                self.bake(lines);
            }
        }
    }
}
