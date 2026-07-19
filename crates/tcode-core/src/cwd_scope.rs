use std::path::Path;
use std::sync::Arc;

/// Runtime state derived from the conversation's working directory that must
/// be re-derived when `/cd` moves it.
///
/// Discovery normally happens once, at assembly, because the working directory
/// is normally fixed for a process. `/cd` breaks that assumption, and the bug
/// it produces is silent: the harness keeps answering from the directory the
/// process started in. Registering an implementation with the session is the
/// standing reminder — an implementor is re-derived at every change, from one
/// call site, and nobody has to remember a checklist.
///
/// **What must not be registered here.** Not everything derived from the
/// working directory can be swapped mid-conversation, and the cost of getting
/// this wrong is invisible too:
///
/// - **skills** — the skill list *is* `SkillTool`'s description, so replacing
///   it changes a tool schema, which is part of every request's cached prefix.
///   One `/cd` would invalidate the whole prefix.
/// - **agent definitions** — same reason: `task`'s schema names the available
///   agent kinds.
/// - **MCP servers** — re-discovery means killing and re-spawning processes,
///   which is not something a directory change should do behind the user's
///   back.
///
/// The rule that separates the two groups: register it only if re-deriving it
/// changes nothing the model has already been sent. Post-processing of tool
/// output qualifies; anything that reaches a request does not.
pub trait CwdScoped: Send + Sync {
    /// Re-derive from `cwd`. Called with the new directory already in effect.
    /// Implementations own a shared handle and swap its contents in place.
    ///
    /// Returns anything the user should see about the new directory — a
    /// configuration file that failed to parse, a definition that shadows a
    /// built-in. Re-derivation must not fail loudly (a bad file in a directory
    /// is not a reason to break the session), but it must not fail silently
    /// either, or the user is left wondering why their rules do nothing.
    fn rescope(&self, cwd: &Path) -> Vec<String>;
}

/// Everything a session re-derives on `/cd`. Empty by default: a frontend that
/// discovers nothing from the filesystem registers nothing.
#[derive(Default, Clone)]
pub struct CwdScopes(Vec<Arc<dyn CwdScoped>>);

impl CwdScopes {
    pub fn push(&mut self, scoped: Arc<dyn CwdScoped>) {
        self.0.push(scoped);
    }

    pub fn rescope_all(&self, cwd: &Path) -> Vec<String> {
        self.0
            .iter()
            .flat_map(|scoped| scoped.rescope(cwd))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for CwdScopes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CwdScopes({} registered)", self.0.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl CwdScoped for Recorder {
        fn rescope(&self, cwd: &Path) -> Vec<String> {
            self.0.lock().unwrap().push(cwd.display().to_string());
            vec![format!("recorded {}", cwd.display())]
        }
    }

    #[test]
    fn every_registered_scope_is_rederived() {
        let one = Arc::new(Recorder::default());
        let two = Arc::new(Recorder::default());
        let mut scopes = CwdScopes::default();
        scopes.push(one.clone());
        scopes.push(two.clone());

        scopes.rescope_all(Path::new("/a"));
        let notes = scopes.rescope_all(Path::new("/b"));

        assert_eq!(*one.0.lock().unwrap(), ["/a", "/b"]);
        assert_eq!(*two.0.lock().unwrap(), ["/a", "/b"]);
        // Every scope's notes reach the caller, not just the first one's.
        assert_eq!(notes, ["recorded /b", "recorded /b"]);
    }
}
