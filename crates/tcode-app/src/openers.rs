//! Handing a workspace path to something outside this process.
//!
//! Two rules shape everything here.
//!
//! **The webview names an id, never a command.** [`OPENERS`] is a fixed table
//! compiled into the binary; a request carries one of its ids and nothing else.
//! That is rule 3 in its sharpest form — a webview that could supply the program
//! to run would be a webview that could run anything, and this one renders model
//! output.
//!
//! **The path comes back through the workspace, not from the request.** Callers
//! resolve the wire path with [`crate::workspace::Workspace::host_path`] first,
//! so an external editor is opened on exactly the entries the confined
//! filesystem surface already agreed exist inside the root.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A place a file can be sent to. `exe` is the command's own name, looked up on
/// `PATH`; `installed_at` names the fixed locations worth trying when the shell
/// command was never put on `PATH`, which on Windows and macOS is the norm.
struct Opener {
    id: &'static str,
    name: &'static str,
    exe: &'static str,
    installed_at: &'static [&'static str],
}

/// The editors this app knows about, in the order they are offered.
const OPENERS: &[Opener] = &[
    Opener {
        id: "vscode",
        name: "VS Code",
        exe: "code",
        installed_at: &[
            "%LOCALAPPDATA%\\Programs\\Microsoft VS Code\\Code.exe",
            "%PROGRAMFILES%\\Microsoft VS Code\\Code.exe",
            "/Applications/Visual Studio Code.app/Contents/MacOS/Electron",
        ],
    },
    Opener {
        id: "cursor",
        name: "Cursor",
        exe: "cursor",
        installed_at: &[
            "%LOCALAPPDATA%\\Programs\\cursor\\Cursor.exe",
            "%PROGRAMFILES%\\cursor\\Cursor.exe",
            "/Applications/Cursor.app/Contents/MacOS/Cursor",
        ],
    },
    Opener {
        id: "zed",
        name: "Zed",
        exe: "zed",
        installed_at: &[
            "%LOCALAPPDATA%\\Zed\\Zed.exe",
            "%PROGRAMFILES%\\Zed\\Zed.exe",
            "/Applications/Zed.app/Contents/MacOS/zed",
        ],
    },
];

/// The id of the file manager entry, which is not an editor and is always there
/// because every desktop this ships on has one.
pub const REVEAL: &str = "reveal";

/// What the file manager is called where this is running. Its own name, because
/// "reveal in file manager" is the kind of generic phrasing that reads as though
/// the app does not know which platform it is on.
pub fn reveal_name() -> &'static str {
    if cfg!(windows) {
        "Explorer"
    } else if cfg!(target_os = "macos") {
        "Finder"
    } else {
        "file manager"
    }
}

/// One offer for the webview's menu.
pub struct OpenerInfo {
    pub id: String,
    pub name: String,
}

/// The openers actually present on this machine, file manager first.
///
/// Detection is a filesystem probe rather than a launch, so an editor that is
/// not installed is simply absent from the menu instead of being an item that
/// fails when picked.
pub fn available() -> Vec<OpenerInfo> {
    let mut out = vec![OpenerInfo {
        id: REVEAL.to_owned(),
        name: reveal_name().to_owned(),
    }];
    for opener in OPENERS {
        if locate(opener).is_some() {
            out.push(OpenerInfo {
                id: opener.id.to_owned(),
                name: opener.name.to_owned(),
            });
        }
    }
    out
}

/// Open `path` with the opener named by `id`.
///
/// Unknown ids are refused rather than guessed at — the fail-closed branch rule
/// 3 asks for. The child is spawned and never waited on: an editor is a
/// long-lived program, and its exit is not this app's business.
pub fn open(id: &str, path: &Path) -> Result<(), String> {
    if id == REVEAL {
        return reveal(path);
    }
    let opener = OPENERS
        .iter()
        .find(|opener| opener.id == id)
        .ok_or_else(|| format!("unknown opener '{id}'"))?;
    let program = locate(opener).ok_or_else(|| format!("{} is not installed", opener.name))?;
    spawn(&program, path).map_err(|error| format!("could not open {}: {error}", opener.name))
}

/// Show the entry in the platform's file manager, selected where that is
/// possible. Linux has no portable "select this one", so it opens the folder.
fn reveal(path: &Path) -> Result<(), String> {
    let failed = |error: std::io::Error| format!("could not open {}: {error}", reveal_name());
    if cfg!(windows) {
        // `explorer` exits non-zero even when it worked, so its status is never
        // read; `/select,<path>` is one argument by Explorer's own grammar.
        let mut argument = std::ffi::OsString::from("/select,");
        argument.push(path);
        Command::new("explorer")
            .arg(argument)
            .spawn()
            .map(|_| ())
            .map_err(failed)
    } else if cfg!(target_os = "macos") {
        Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(failed)
    } else {
        let folder = path.parent().unwrap_or(path);
        Command::new("xdg-open")
            .arg(folder)
            .spawn()
            .map(|_| ())
            .map_err(failed)
    }
}

/// Where this opener's program is, or `None` if it is not installed.
fn locate(opener: &Opener) -> Option<PathBuf> {
    on_path(opener.exe).or_else(|| {
        opener
            .installed_at
            .iter()
            .filter_map(|candidate| expand(candidate))
            .find(|candidate| candidate.is_file())
    })
}

/// `%VAR%` expansion for the fixed install locations. A candidate naming a
/// variable this machine does not set is dropped rather than tried literally.
fn expand(candidate: &str) -> Option<PathBuf> {
    let mut out = String::new();
    let mut rest = candidate;
    while let Some(start) = rest.find('%') {
        let after = &rest[start + 1..];
        let end = after.find('%')?;
        out.push_str(&rest[..start]);
        out.push_str(&std::env::var(&after[..end]).ok()?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(PathBuf::from(out))
}

/// A `which` for the one thing this module needs: the first executable of that
/// name on `PATH`. On Windows the extensions come from `PATHEXT`, so a `.cmd`
/// shim — which is how both VS Code and Cursor put themselves on `PATH` — is
/// found like any other program and dealt with in [`spawn`].
fn on_path(exe: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        for extension in extensions() {
            let candidate = directory.join(format!("{exe}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn extensions() -> Vec<String> {
    if !cfg!(windows) {
        return vec![String::new()];
    }
    let listed = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_owned());
    let mut out: Vec<String> = listed
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| extension.to_ascii_lowercase())
        .collect();
    out.push(String::new());
    out
}

/// Windows cannot `CreateProcess` a batch file, and a batch file is exactly what
/// `code.cmd` is. Running one means going through `cmd.exe`, which is a shell —
/// so a path containing a character `cmd` gives meaning to is refused outright
/// rather than quoted and hoped for. Repositories are allowed to contain a file
/// named `a&b.txt`; this app is not allowed to run it.
fn spawn(program: &Path, path: &Path) -> std::io::Result<()> {
    let batch = program
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
        });

    if !batch {
        return Command::new(program).arg(path).spawn().map(|_| ());
    }

    let shown = path.to_string_lossy();
    if shown.contains(['"', '&', '|', '<', '>', '^', '%', '!']) {
        return Err(std::io::Error::other(
            "this path cannot be passed to a command shell — open it from the editor instead",
        ));
    }
    Command::new("cmd")
        .arg("/C")
        .arg(program)
        .arg(path)
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_an_opener_it_does_not_know() {
        let error = open("notepad-plus-plus", Path::new("/tmp/x")).unwrap_err();
        assert!(error.contains("unknown opener"), "{error}");
    }

    #[test]
    fn the_file_manager_is_always_offered_and_comes_first() {
        let offered = available();
        assert_eq!(
            offered.first().map(|opener| opener.id.as_str()),
            Some(REVEAL)
        );
    }

    #[test]
    fn expands_only_variables_this_machine_sets() {
        std::env::set_var("TCODE_OPENER_TEST", "here");
        assert_eq!(
            expand("%TCODE_OPENER_TEST%/x").unwrap(),
            PathBuf::from("here/x")
        );
        assert!(expand("%TCODE_OPENER_ABSENT%/x").is_none());
    }

    #[test]
    fn a_bare_path_expands_to_itself() {
        assert_eq!(expand("/opt/zed").unwrap(), PathBuf::from("/opt/zed"));
    }
}
