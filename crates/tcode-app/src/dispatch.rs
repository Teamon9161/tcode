//! The command registry: what `#[tauri::command]` was doing, written out.
//!
//! That attribute does exactly two things — pull each argument out of a JSON
//! object by parameter name, and serialize the return value — and it does them
//! by generating a wrapper the shell's `invoke_handler!` collects. Both halves
//! are reproduced here against a plain table, so the same 50-odd functions can
//! be driven by a Tauri `invoke`, by a JSON-RPC line on stdin, or by a test,
//! without any of them knowing which.
//!
//! This is the registry pattern the root `CLAUDE.md` asks for: adding a command
//! is one line in [`Registry::builtin`], and no `match` anywhere grows a branch.
//!
//! **Argument names are camelCase on the wire.** That is not a preference; it
//! is what Tauri v2 does by default, so the frontend already sends `entryIndex`
//! for a parameter spelled `entry_index`. Reproducing the convention is what
//! makes this table a drop-in for the attribute rather than a rename of the
//! whole protocol ([`camel`], and the test that pins it).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::boot::ServeHandle;
use crate::bridge::Emit;
use crate::state::Supervisor;
use crate::terminal::Terminals;

/// Everything a command can reach.
///
/// The composition root builds one of these and nothing else; a command names
/// the fields it needs at its registry line, so a browser verb cannot quietly
/// acquire the supervisor. `AppHandle` is deliberately absent — the one thing
/// commands used it for is emitting, and that is [`Emit`], which has never
/// needed a window (AGENTS.md hard rule 2).
pub struct Ctx {
    pub supervisor: Arc<Supervisor>,
    pub serve: ServeHandle,
    pub terminals: Arc<Terminals>,
    pub emit: Arc<dyn Emit>,
}

/// A command's answer. Boxed because the table holds sync and async commands
/// side by side and the caller must not care which it got.
pub type Reply<'a> = Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;

/// One command: context and a JSON object of arguments in, JSON or an error
/// message out. The error is a `String` because that is what the frontend
/// already receives and displays — see AGENTS.md rule 7.
pub type Handler = Box<dyn for<'a> Fn(&'a Ctx, &'a Value) -> Reply<'a> + Send + Sync>;

/// Pull one argument out of the payload, by the name the wire uses.
///
/// A missing key becomes `null`, which is how an `Option<T>` parameter arrives
/// as `None` and a required one produces an error naming itself. Both match the
/// attribute's behaviour, and the second is the reason this returns `Result`
/// rather than defaulting: a command that silently ran with a missing argument
/// would be rule 3 ("what the frontend sends is data") broken by convenience.
pub fn arg<T: DeserializeOwned>(args: &Value, name: &str) -> Result<T, String> {
    let key = camel(name);
    let value = args.get(&key).cloned().unwrap_or(Value::Null);
    serde_json::from_value(value).map_err(|error| format!("bad argument '{key}': {error}"))
}

/// `entry_index` → `entryIndex`. Tauri v2's default argument renaming, and
/// therefore the names already in `ui/src`.
fn camel(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper = false;
    for ch in name.chars() {
        match ch {
            '_' => upper = true,
            _ if upper => {
                out.extend(ch.to_uppercase());
                upper = false;
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Force the higher-ranked lifetime the table needs.
///
/// A function rather than a cast, because the closure cannot be annotated
/// inline: `|ctx: &Ctx, args: &Value| -> Reply<'_>` binds the elided lifetime to
/// one parameter and then cannot prove the returned future outlives the other.
/// Passing it through an explicit `for<'a>` bound is what makes the inference
/// land, and it is the only reason this exists.
///
/// `pub` rather than `pub(crate)` because the shell-owned verbs are registered
/// from outside this crate — `main.rs` is its own target, and after the
/// migration the equivalent is another process entirely.
pub fn handler<F>(f: F) -> Handler
where
    F: for<'a> Fn(&'a Ctx, &'a Value) -> Reply<'a> + Send + Sync + 'static,
{
    Box::new(f)
}

/// Register one command.
///
/// The two bracket groups are the whole grammar: `[…]` names the [`Ctx`] fields
/// this command takes, `(…)` names its wire arguments in signature order.
/// **Neither restates a type** — they are inferred through the call, so a
/// registry line that disagrees with the function is a compile error rather
/// than a command that deserializes into the wrong shape at runtime.
///
/// Three macros rather than one with a marker token, and that is forced: an arm
/// beginning `$f:path` will happily start matching a leading `try` or `async`
/// and then fail *inside* the fragment, which `macro_rules!` cannot back out
/// of — so the shape has to be in the name. It reads better anyway.
macro_rules! value {
    ($table:expr, $name:literal, $f:path [$($dep:ident),*] ($($arg:ident),*)) => {
        $table.insert(
            $name,
            handler(|_ctx, _args| {
                Box::pin(async move {
                    let out = $f($(&_ctx.$dep,)* $(arg(_args, stringify!($arg))?,)*);
                    serde_json::to_value(out).map_err(|error| error.to_string())
                })
            }),
        );
    };
}

macro_rules! result {
    ($table:expr, $name:literal, $f:path [$($dep:ident),*] ($($arg:ident),*)) => {
        $table.insert(
            $name,
            handler(|_ctx, _args| {
                Box::pin(async move {
                    let out = $f($(&_ctx.$dep,)* $(arg(_args, stringify!($arg))?,)*)?;
                    serde_json::to_value(out).map_err(|error| error.to_string())
                })
            }),
        );
    };
}

macro_rules! async_result {
    ($table:expr, $name:literal, $f:path [$($dep:ident),*] ($($arg:ident),*)) => {
        $table.insert(
            $name,
            handler(|_ctx, _args| {
                Box::pin(async move {
                    let out = $f($(&_ctx.$dep,)* $(arg(_args, stringify!($arg))?,)*).await?;
                    serde_json::to_value(out).map_err(|error| error.to_string())
                })
            }),
        );
    };
}

/// The commands, by wire name.
#[derive(Default)]
pub struct Registry(HashMap<&'static str, Handler>);

impl Registry {
    /// Every command that does not depend on which shell is drawing the window.
    ///
    /// The browser verbs are **deliberately not here**: a browser tab is a
    /// native view owned by the shell, so `browser_*` is registered by whoever
    /// owns one — the Electron main process (`electron/browser.js`). See
    /// `AGENTS.md` rule 9h; `the_shell_s_own_verbs_are_not_in_the_portable_
    /// table` pins the omission so it reads as a decision rather than a gap.
    pub fn builtin() -> Self {
        let mut t: HashMap<&'static str, Handler> = HashMap::new();
        use crate::commands as c;

        // Conversations.
        value!(t, "sessions", c::sessions[supervisor]());
        value!(t, "plan", c::plan[supervisor](session));
        result!(t, "write_plan", c::write_plan[supervisor](session, phases));
        result!(t, "execute_plan_elsewhere", c::execute_plan_elsewhere[emit, supervisor](session));
        result!(t, "open_folder", c::open_folder[supervisor](path, resume));
        result!(
            t,
            "change_folder",
            c::change_folder[supervisor](session, path)
        );
        value!(t, "close_session", c::close_session[supervisor](session));
        result!(t, "send_message", c::send_message[emit, supervisor](session, text, images, plan));
        result!(t, "queued", c::queued[supervisor](session));
        result!(
            t,
            "withdraw_queued",
            c::withdraw_queued[supervisor](session, index, text)
        );
        result!(t, "interrupt", c::interrupt[supervisor](session));
        result!(
            t,
            "interrupt_and_send",
            c::interrupt_and_send[supervisor](session, turn)
        );
        result!(
            t,
            "respond_approval",
            c::respond_approval[supervisor](session, answer)
        );
        result!(t, "choose_mode", c::choose_mode[supervisor](session, mode));

        // Rewind. `entry_index` / `restore_files` are the two multi-word
        // arguments in the whole surface, so they are also the only proof that
        // `camel` is doing anything — see the test.
        result!(t, "rewind_targets", c::rewind_targets[supervisor](session));
        result!(
            t,
            "rewind_preview",
            c::rewind_preview[supervisor](session, entry_index)
        );
        result!(
            t,
            "rewind",
            c::rewind[supervisor](session, entry_index, restore_files)
        );

        // Slash commands and the tool view contract.
        value!(t, "slash_commands", c::slash_commands[supervisor]());
        result!(t, "slash_command", c::slash_command[emit, supervisor](session, line));
        value!(t, "tool_views", c::tool_views[supervisor]());

        // Project discovery is cheap; conversation previews are cursor-paged.
        // Both use blocking file IO, so neither runs on the renderer thread.
        async_result!(t, "project_list", c::project_list[]());
        async_result!(t, "project_sessions", c::project_sessions[](path, before));

        // The workspace.
        result!(
            t,
            "workspace_list",
            c::workspace_list[supervisor](session, path)
        );
        async_result!(
            t,
            "workspace_complete",
            c::workspace_complete[supervisor](session, prefix)
        );
        async_result!(
            t,
            "workspace_present",
            c::workspace_present[supervisor](session, paths)
        );
        result!(
            t,
            "workspace_read_text",
            c::workspace_read_text[supervisor](session, path)
        );
        result!(
            t,
            "workspace_stat",
            c::workspace_stat[supervisor](session, path)
        );
        result!(
            t,
            "workspace_read_binary",
            c::workspace_read_binary[supervisor](session, path)
        );
        result!(
            t,
            "workspace_write_text",
            c::workspace_write_text[supervisor](session, path, text, revision, force)
        );
        result!(
            t,
            "workspace_create",
            c::workspace_create[supervisor](session, parent, name, kind)
        );
        result!(
            t,
            "workspace_rename",
            c::workspace_rename[supervisor](session, path, name)
        );
        result!(
            t,
            "workspace_delete",
            c::workspace_delete[supervisor](session, path)
        );
        result!(
            t,
            "workspace_trash",
            c::workspace_trash[supervisor](session, path)
        );
        value!(t, "workspace_openers", c::workspace_openers[]());
        result!(
            t,
            "workspace_open_external",
            c::workspace_open_external[supervisor](session, path, opener)
        );

        // Viewing a file: bytes one way, an origin the other (rule 11b).
        result!(
            t,
            "shown_file",
            c::shown_file[supervisor](session, path, binary)
        );
        result!(t, "serve_url", c::serve_url[supervisor, serve](session, path));

        // The terminal. The PTY stays on this side of the pipe — see
        // `AGENTS.md` rule 9i for why it is worth keeping behind the same
        // audited boundary.
        result!(t, "desktop_settings", c::desktop_settings[supervisor]());
        result!(
            t,
            "set_terminal_shell",
            c::set_terminal_shell[supervisor, terminals](shell)
        );
        result!(t, "terminal_open", c::terminal_open[emit, terminals](cwd, cols, rows));
        result!(t, "terminal_write", c::terminal_write[terminals](id, data));
        result!(
            t,
            "terminal_resize",
            c::terminal_resize[terminals](id, cols, rows)
        );
        result!(t, "terminal_close", c::terminal_close[terminals](id));

        // Model / preset / role / mode selection. The logic is
        // `tcode-frontend`'s; `picker.rs` only shapes it (rule 15).
        result!(t, "picker_state", c::picker_state[supervisor](session));
        result!(
            t,
            "choose_model",
            c::choose_model[supervisor](session, index, effort)
        );
        result!(
            t,
            "choose_preset",
            c::choose_preset[supervisor](session, key)
        );
        result!(t, "pin_role", c::pin_role[supervisor](kind, pin));
        result!(t, "save_preset", c::save_preset[supervisor](session, name));

        // What somebody typed in the address bar, as a URL. A backend command
        // and not a `browser_*` verb on purpose: it needs no view, and the
        // alternative was a second copy of five tests' worth of guesswork
        // living in the Electron main process. See `crate::address`.
        result!(t, "resolve_url", crate::address::resolve_url[](input));

        Self(t)
    }

    /// Add a shell-owned command. The only caller is whoever owns the native
    /// views — see [`Registry::builtin`] on why `browser_*` arrives this way.
    pub fn add(&mut self, name: &'static str, handler: Handler) {
        self.0.insert(name, handler);
    }

    pub fn names(&self) -> Vec<&'static str> {
        let mut names: Vec<_> = self.0.keys().copied().collect();
        names.sort_unstable();
        names
    }

    /// Run one command.
    ///
    /// An unknown name is an error and never a silent success: the frontend
    /// surfaces it (rule 7), and during the migration it is exactly how a
    /// command that lost its registry line announces itself.
    pub async fn call(&self, ctx: &Ctx, method: &str, args: &Value) -> Result<Value, String> {
        let handler = self
            .0
            .get(method)
            .ok_or_else(|| format!("unknown command '{method}'"))?;
        handler(ctx, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_names_are_camel_case() {
        assert_eq!(camel("session"), "session");
        assert_eq!(camel("entry_index"), "entryIndex");
        assert_eq!(camel("restore_files"), "restoreFiles");
    }

    /// A missing argument is `null`, so `Option<T>` parameters are optional and
    /// required ones fail by name. Both are the attribute's behaviour, and the
    /// frontend relies on the first: `open_folder` is called without `resume`.
    #[test]
    fn a_missing_argument_is_absent_rather_than_wrong() {
        let empty = serde_json::json!({});
        assert_eq!(arg::<Option<String>>(&empty, "resume"), Ok(None));
        assert!(arg::<String>(&empty, "session")
            .unwrap_err()
            .contains("session"));
    }

    /// Where a shell registers the verbs only it can answer: a native view, a
    /// window, a file dialog. A command named in one of these is answered even
    /// though [`Registry::builtin`] does not have it.
    const SHELL_REGISTRARS: [&str; 2] = ["electron/main.js", "electron/browser.js"];

    /// The registry is the contract with `ui/src`, and nothing else checks it:
    /// a command that loses its line here still compiles, and the pane that
    /// calls it simply reports an error nobody expected. Same mechanical check
    /// as `bridge.rs::the_event_names_match_the_frontend`.
    ///
    /// The shell registrars are read rather than skipped by prefix, so "the
    /// shell owns this one" has to be true rather than merely spelled that way.
    /// A bare name match, which a name in a comment would also satisfy — this
    /// catches the command nobody registered, not the one someone lied about.
    #[test]
    fn every_command_the_frontend_calls_is_registered() {
        let registry = Registry::builtin();
        let known = registry.names();
        let shells = shell_registrar_text();
        let (mut missing, mut seen) = (Vec::new(), 0usize);

        for file in walk(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/src").as_ref()) {
            let text = std::fs::read_to_string(&file).unwrap_or_default();
            for name in invoked(&text) {
                seen += 1;
                if known.contains(&name.as_str()) || shells.contains(&name) {
                    continue;
                }
                missing.push(format!("{name} (in {})", file.display()));
            }
        }

        // A scan that finds nothing passes, and looks exactly like a scan that
        // found everything. This is the difference: if the walk, the extension
        // filter or `invoked` ever stops matching, the check fails loudly
        // instead of going quietly green.
        assert!(
            seen > 40,
            "only {seen} invoke() call sites found in ui/src — the scan is broken, not the registry"
        );
        assert!(
            missing.is_empty(),
            "the frontend calls commands the registry does not have: {missing:#?}"
        );
    }

    /// A native view, a window and a file dialog belong to whoever draws them,
    /// so their absence here is a decision. Pinned because "the table is
    /// missing a family" and "the table is complete" look identical from inside
    /// this file — and because the tempting fix for a missing verb is to add it
    /// here, where it would compile and then have nothing to act on.
    #[test]
    fn the_shell_s_own_verbs_are_not_in_the_portable_table() {
        let registry = Registry::builtin();
        for prefix in ["browser_", "window_", "dialog_"] {
            assert!(
                !registry.names().iter().any(|n| n.starts_with(prefix)),
                "{prefix}* must be registered by the shell that owns the window, not by builtin()"
            );
        }
    }

    /// The one shell answers every shell-owned verb, or it has a title bar
    /// whose buttons do nothing and a browser pane that cannot open a tab.
    ///
    /// The frontend calls one name; the Electron main process and the browser
    /// views have to recognize it, and nothing else compares them.
    #[test]
    fn the_shell_answers_every_shell_owned_verb() {
        const VERBS: &[&str] = &[
            "window_minimize",
            "window_close",
            "window_is_maximized",
            "window_toggle_maximize",
            "dialog_open_folder",
            "browser_open",
            "browser_show",
            "browser_select",
            "browser_bounds",
            "browser_visible",
            "browser_navigate",
            "browser_step",
            "browser_reload",
            "browser_close",
            // Called by the backend rather than by the frontend — the `browser`
            // tool's way of seeing and working a page (`crate::browser`). Listed
            // here all the same: what this pins is that the one shell answers
            // every verb somebody sends it, and the sidecar is now one of the
            // somebodies.
            "browser_snapshot",
            "browser_computed_style",
            "browser_screenshot",
            "browser_click",
            "browser_type",
            "browser_scroll",
            "browser_wait",
        ];

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        // `main.js` answers the window verbs and forwards the rest to the
        // sidecar; `browser.js` answers the browser verbs. What is compared is
        // the set of names the shell answers, not where it happens to keep
        // them.
        let text: String = ["electron/main.js", "electron/browser.js"]
            .iter()
            .map(|file| std::fs::read_to_string(root.join(file)).expect(file))
            .collect();
        for verb in VERBS {
            assert!(
                text.contains(verb),
                "the Electron shell does not answer `{verb}` — that verb is missing \
                 from the one shell the frontend calls it on"
            );
        }
    }

    /// A browser tab is a page of someone else's, pointed at an arbitrary URL.
    /// What keeps that reasonable is that it carries none of this app's
    /// privileges: no `preload`, no node, an isolated context, a sandbox, and
    /// its own partition rather than the app session. (The Tauri shell used to
    /// say the same sentence as "granted no capabilities, ever" in
    /// `capabilities/default.json`; that file is gone with it.) The migration
    /// doc's rule 9h row; the same style of check as the ones that read source
    /// files for their invariants.
    #[test]
    fn the_browser_views_are_isolated_from_the_app() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/electron/browser.js"))
                .expect("electron/browser.js");

        // Scan the `webPreferences` block rather than the whole file: the
        // file's own header explains why there is no preload, and would trip a
        // whole-file scan. A `preload:` property is the failure; the word in a
        // comment is the explanation.
        let view = source
            .find("new WebContentsView({")
            .expect("browser.js creates a WebContentsView");
        let create = &source[view..];
        let block = &create[..create
            .find("\n    });")
            .expect("the view creation block ends")];

        assert!(
            !block.contains("preload:"),
            "a browser tab gained a `preload` — that script runs inside every page \
             the browser visits, with this app's privileges"
        );
        for required in [
            "nodeIntegration: false",
            "contextIsolation: true",
            "sandbox: true",
            "session: session.fromPartition(PARTITION)",
        ] {
            assert!(
                block.contains(required),
                "the browser view's webPreferences lost `{required}` — a tab must be \
                 sandboxed, isolated and on its own partition"
            );
        }
    }

    /// The app document is never navigated and never opens a window. Tauri had
    /// these as defaults; Electron does not, so `main.js` reinstates them in
    /// `createWindow`. Pinned because the failure mode — a stray `<a href>`
    /// that escaped the frontend's own handler replaces the whole app, or a
    /// `target="_blank"` opens a frameless window carrying this app's preload —
    /// reads as a bug in something else entirely.
    #[test]
    fn the_app_renderer_cannot_be_navigated_away_or_open_a_window() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/electron/main.js"))
                .expect("electron/main.js");

        assert!(
            source.contains("will-navigate") && source.contains("preventDefault"),
            "the app renderer lost its `will-navigate` guard — a link or redirect can \
             replace the whole app with a page of someone else's choosing"
        );
        assert!(
            source.contains("setWindowOpenHandler") && source.contains("action: \"deny\""),
            "the app renderer lost its `setWindowOpenHandler` deny — \
             `target=\"_blank\"` opens a frameless window carrying this app's preload"
        );
    }

    /// A tab opened for a model must not take the screen.
    ///
    /// Two callers reach `browser_open` now: the pane, when somebody clicks
    /// `+`, and the backend, when a model asks for a tab. Only the first has
    /// any business changing which tab is on screen — the whole reason the
    /// agent drives the window's own browser rather than a headless one is that
    /// watching it stays optional (`../AGENT-BROWSER.md`).
    ///
    /// What that rests on is one comparison. `args.select` read strictly means
    /// a caller that omits it gets a background tab; `args.select !== false`,
    /// or a `?? true`, would flip the default to "steal the screen" and the
    /// symptom would be a page appearing over whatever someone was reading —
    /// which reads as a bug in the pane, not in an argument's default.
    #[test]
    fn a_tab_opened_for_a_model_does_not_take_the_screen() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/electron/browser.js"))
                .expect("electron/browser.js");

        assert!(
            source.contains("args.select === true"),
            "`browser_open` no longer reads `select` strictly — a caller that omits it \
             would now take the screen, which is what an agent's tab must never do"
        );
    }

    /// An approval names a host; the click has to land on that host.
    ///
    /// The backend decides what to ask about from the last page it *saw* — a
    /// navigation it resolved, or a snapshot the page answered with. A page can
    /// move on its own between that and the click: a redirect, a meta refresh,
    /// a script. Without the check the user approves "click on github.com" and
    /// the click lands wherever the tab drifted to, which is the exact failure
    /// the per-host descriptor exists to prevent — and it fails silently.
    ///
    /// It lives in the shell rather than in a preceding round trip because that
    /// is what makes it airtight: there is no window between reading the URL and
    /// dispatching the event for the page to move in. The shell is not judging
    /// anything; it compares a value the backend computed.
    #[test]
    fn acting_on_a_tab_checks_the_page_has_not_moved() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/electron/browser.js"))
                .expect("electron/browser.js");

        assert!(
            source.contains("const onHost ="),
            "the browser shell lost its host check — a click can now land on a page the user \
             was never asked about"
        );
        for verb in ["browser_click(args)", "browser_type(args)"] {
            let at = source
                .find(verb)
                .unwrap_or_else(|| panic!("browser.js answers `{verb}`"));
            let body = &source[at..];
            let body = &body[..body.find("\n    },").expect("the verb body ends")];
            assert!(
                body.contains("onHost(args.id, args.host)"),
                "`{verb}` acts without checking the tab is still on the host the approval named"
            );
        }
    }

    /// A screenshot must not need the tab to be current or exposed over the app.
    ///
    /// `Page.captureScreenshot` hangs intermittently on a hidden view. Electron's
    /// `capturePage` is the reliable path, but the original 9/9 measurement only
    /// covered a view that had rendered before it was hidden. A tab hidden from
    /// birth needs the shared `rendered` recovery to create its current document's
    /// compositor surface under the app renderer (`../AGENT-BROWSER.md`).
    #[test]
    fn a_screenshot_does_not_need_the_tab_on_screen() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/electron/browser.js"))
                .expect("electron/browser.js");

        let at = source
            .find("browser_screenshot(args)")
            .expect("browser.js answers `browser_screenshot`");
        let body = &source[at..];
        let body = &body[..body.find("\n    },").expect("the verb body ends")];
        assert!(
            body.contains("rendered(args.id"),
            "`browser_screenshot` bypasses background render recovery — a tab hidden from birth \
             can have a live AX tree and still return an empty image"
        );
        assert!(
            body.contains("capturePage()"),
            "`browser_screenshot` no longer uses Electron's own capture"
        );
        assert!(
            !body.contains("captureScreenshot"),
            "`browser_screenshot` is back on the CDP command, which hangs on a hidden view"
        );
    }

    /// A page in the browser can call `window.open` too, and it must not get a
    /// frameless Electron window. Same-tab loading is the honest interim: it
    /// never silently does nothing, and the page could have navigated itself
    /// there anyway (see AGENTS.md rule 9h).
    #[test]
    fn a_browser_tab_cannot_open_a_window() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/electron/browser.js"))
                .expect("electron/browser.js");

        assert!(
            source.contains("setWindowOpenHandler")
                && source.contains("action: \"deny\"")
                && source.contains("contents.loadURL(url)"),
            "a browser tab can open an Electron window — `setWindowOpenHandler` must \
             deny and load the URL in the same tab"
        );
    }

    /// A page in the browser can ask for the camera, the microphone,
    /// geolocation or notifications. Chromium's default is a prompt — and
    /// there is no user here to answer a prompt about a page that came from
    /// the open web. The partition denies them all, once, at boot.
    #[test]
    fn the_browser_partition_denies_permission_requests() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/electron/browser.js"))
                .expect("electron/browser.js");

        assert!(
            source.contains("setPermissionRequestHandler"),
            "the browser partition lost its permission handler — Chromium will prompt \
             a page in a tab for the camera, microphone, geolocation and notifications"
        );
        assert!(
            source.contains("callback(false)"),
            "the permission handler no longer denies — a page in a tab could be \
             granted camera or geolocation access"
        );
    }

    /// The app's one policy sentence, and the one shell left to say it.
    ///
    /// Under Tauri it was a field in `tauri.conf.json` and a test compared the
    /// two copies; that shell is gone, so this pins the Electron copy — a
    /// header the `app://` handler writes on every response — and the rule
    /// that never changed: no `script-src`, so scripts fall back to
    /// `default-src 'self'`, which carries neither unsafe token. Adding the
    /// directive at all is the change that needs looking at, whatever value it
    /// is given.
    #[test]
    fn the_app_serves_the_content_security_policy() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        // `const CSP = "…" + "…";` — a concatenation of literals, so what is
        // inside the quotes is the policy.
        let main =
            std::fs::read_to_string(root.join("electron/main.js")).expect("electron/main.js");
        let start = main
            .find("const CSP =")
            .expect("electron/main.js declares CSP");
        let declaration = &main[start..];
        // `";` and not `;`, because the policy itself is full of semicolons and
        // a literal cannot contain an unescaped quote.
        let end = declaration.find("\";").expect("the declaration ends") + 1;
        let declaration = &declaration[..end];
        let policy: String = declaration.split('"').skip(1).step_by(2).collect();

        assert!(
            !policy.contains("script-src"),
            "the policy grew a `script-src`. Scripts used to fall back to `default-src \
             'self'`; whatever this directive says now, it is the one change rule 11 is about"
        );
        assert!(
            !policy.contains("unsafe-eval"),
            "`unsafe-eval` in the app's policy — see rule 11 and `ui/src/math.tsx`"
        );
    }

    /// An app-owned title bar must not reach the window through the webview.
    ///
    /// The controls go through `invoke`, answered by whichever shell owns the
    /// window — today that is `electron/main.js` — and a component both shells
    /// used to render must know nothing about windows beyond those command
    /// names. What it pins is the invariant that replaced the grants: no
    /// window call from the frontend. (The Tauri half of this test read
    /// `capabilities/default.json` for the grants those calls used to need;
    /// the file and the shell are gone.) The drag surface is the one thing the
    /// component still carries for the shell: the attribute `app.css` matches
    /// to make the bar draggable, and losing it is a title bar that cannot be
    /// moved.
    #[test]
    fn the_title_bar_does_not_reach_the_window_from_the_webview() {
        /// Window methods that *act*. Their presence in the component is the
        /// failure; each one is a shell command instead.
        const ACTS: &[&str] = &[
            ".minimize()",
            ".maximize()",
            ".unmaximize()",
            ".unminimize()",
            ".toggleMaximize()",
            ".close()",
            ".hide()",
            ".show()",
            ".setTitle(",
            ".setFullscreen(",
            ".startDragging()",
        ];

        let component = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/ui/src/components/WindowControls.tsx"
        ))
        .expect("ui/src/components/WindowControls.tsx");

        assert!(
            !component.contains("from \"@tauri-apps"),
            "the title bar imports a Tauri API — there is no Tauri shell to answer it, \
             and the window belongs to Electron (see `electron/main.js`)"
        );
        for call in ACTS {
            assert!(
                !component.contains(call),
                "the title bar calls `{call}` directly — that reaches the shell's window API from \
                 a component. Add a `window_*` command to the shell instead."
            );
        }
        assert!(
            component.contains("data-drag-region"),
            "the drag surface attribute moved out of WindowControls.tsx — the title bar is no \
             longer draggable, and this test and the CSS that matches it now pin nothing"
        );
    }

    /// Every shell registrar's text, concatenated. Missing files are an error
    /// rather than an empty string: a renamed registrar would otherwise turn
    /// this whole check off without failing anything.
    fn shell_registrar_text() -> String {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        SHELL_REGISTRARS
            .iter()
            .map(|relative| std::fs::read_to_string(root.join(relative)).expect(relative))
            .collect()
    }

    /// Every `invoke("name"` / `invoke<T>("name"` in a source file.
    fn invoked(text: &str) -> Vec<String> {
        let mut found = Vec::new();
        for (index, _) in text.match_indices("invoke") {
            let rest = &text[index + "invoke".len()..];
            let rest = rest.strip_prefix('<').map_or(rest, |generic| {
                generic.find('>').map_or(generic, |end| &generic[end + 1..])
            });
            let Some(rest) = rest.strip_prefix("(\"") else {
                continue;
            };
            if let Some(end) = rest.find('"') {
                found.push(rest[..end].to_string());
            }
        }
        found
    }

    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut files = Vec::new();
        let Ok(entries) = dir.read_dir() else {
            return files;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Fixtures answer commands rather than calling them.
                if path.file_name().is_some_and(|name| name == "preview") {
                    continue;
                }
                files.extend(walk(&path));
            } else if path.extension().is_some_and(|e| e == "ts" || e == "tsx") {
                files.push(path);
            }
        }
        files
    }
}
