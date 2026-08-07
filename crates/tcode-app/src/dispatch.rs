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
    /// owns one — `main.rs` under Tauri today, the Electron main process after
    /// the migration. See `MIGRATION-ELECTRON.md`; `browser_commands_are_the_
    /// shell_s_own` pins the omission so it reads as a decision rather than a
    /// gap.
    pub fn builtin() -> Self {
        let mut t: HashMap<&'static str, Handler> = HashMap::new();
        use crate::commands as c;

        // Conversations.
        value!(t, "sessions", c::sessions[supervisor]());
        value!(t, "plan", c::plan[supervisor](session));
        result!(t, "write_plan", c::write_plan[supervisor](session, phases));
        result!(t, "execute_plan_elsewhere", c::execute_plan_elsewhere[emit, supervisor](session));
        result!(t, "open_folder", c::open_folder[supervisor](path, resume));
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

        // Projects. Both read every log in a folder, so both are async and
        // land on a blocking pool — rule 22.
        async_result!(t, "project_list", c::project_list[]());
        async_result!(t, "project_sessions", c::project_sessions[](path));

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
            "workspace_read_binary",
            c::workspace_read_binary[supervisor](session, path)
        );
        result!(
            t,
            "workspace_write_text",
            c::workspace_write_text[supervisor](session, path, text, revision)
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
        // `MIGRATION-ELECTRON.md`, and rule 9i for why it is worth keeping
        // behind the same audited boundary.
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
            c::choose_model[supervisor](index, effort)
        );
        result!(t, "choose_preset", c::choose_preset[supervisor](key));
        result!(t, "pin_role", c::pin_role[supervisor](kind, pin));
        result!(t, "save_preset", c::save_preset[supervisor](name));

        // The two commands with no dependencies at all.
        result!(t, "clipboard_image", c::clipboard_image[]());
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
    const SHELL_REGISTRARS: [&str; 4] = [
        "src/main.rs",
        "src/browser/commands.rs",
        "electron/main.js",
        "electron/browser.js",
    ];

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

    /// Both shells answer every shell-owned verb, or one of them has a title
    /// bar whose buttons do nothing and a browser pane that cannot open a tab.
    ///
    /// This is the check the migration is actually exposed to: the frontend
    /// calls one name, two processes in two languages have to recognize it, and
    /// nothing else compares them.
    #[test]
    fn both_shells_answer_every_shell_owned_verb() {
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
        ];

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        // Each shell is however many files it takes; what is compared is the
        // set of names it answers, not where it happens to keep them.
        for shell in [
            ["src/main.rs", "src/browser/commands.rs"],
            ["electron/main.js", "electron/browser.js"],
        ] {
            let text: String = shell
                .iter()
                .map(|file| std::fs::read_to_string(root.join(file)).expect(file))
                .collect();
            for verb in VERBS {
                assert!(
                    text.contains(verb),
                    "{shell:?} does not answer `{verb}` — that shell is missing a verb the \
                     frontend calls on both"
                );
            }
        }
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
