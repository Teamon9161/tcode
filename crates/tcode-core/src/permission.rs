use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::tool::PermissionRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// Read-only tools run; anything mutating is blocked.
    Plan,
    /// Rules decide; unmatched actions prompt the user.
    #[default]
    Default,
    /// File edits auto-approved; shell etc. still prompt.
    AcceptEdits,
    /// Everything runs (deny rules still apply).
    Auto,
}

impl PermissionMode {
    pub fn label(&self) -> &'static str {
        match self {
            PermissionMode::Plan => "plan",
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "accept-edits",
            PermissionMode::Auto => "auto",
        }
    }

    pub fn cycle(&self) -> Self {
        match self {
            PermissionMode::Default => PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits => PermissionMode::Plan,
            PermissionMode::Plan => PermissionMode::Auto,
            PermissionMode::Auto => PermissionMode::Default,
        }
    }
}

/// Rules match descriptors like "shell(git status --short)" against
/// patterns like "shell(git *)". `*` is the only wildcard.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionRules {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
    Ask,
}

impl PermissionRules {
    pub fn decide(&self, mode: PermissionMode, request: &PermissionRequest) -> Decision {
        let PermissionRequest::Ask {
            descriptor,
            is_edit,
            ..
        } = request
        else {
            return Decision::Allow;
        };
        // Deny always wins, regardless of mode.
        if let Some(rule) = self.deny.iter().find(|r| pattern_match(r, descriptor)) {
            return Decision::Deny(format!("denied by rule '{rule}'"));
        }
        match mode {
            PermissionMode::Plan => Decision::Deny(
                "blocked: plan mode is active; only read-only tools may run".into(),
            ),
            PermissionMode::Auto => Decision::Allow,
            PermissionMode::AcceptEdits if *is_edit => Decision::Allow,
            _ => {
                if self.allow.iter().any(|r| pattern_match(r, descriptor)) {
                    Decision::Allow
                } else {
                    Decision::Ask
                }
            }
        }
    }
}

/// Glob-lite: literal match with `*` spanning any characters.
pub fn pattern_match(pattern: &str, text: &str) -> bool {
    fn inner(p: &[char], t: &[char]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some(('*', rest)) => {
                (0..=t.len()).any(|i| inner(rest, &t[i..]))
            }
            Some((c, rest)) => t.split_first().is_some_and(|(tc, tr)| tc == c && inner(rest, tr)),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    inner(&p, &t)
}

/// The user's answer to an approval prompt. `comment` is the
/// tab-annotation: guidance attached to a yes, or the reason for a no.
#[derive(Debug, Clone)]
pub struct Approval {
    pub decision: ApprovalDecision,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Yes,
    /// Yes + persist an allow rule for this session.
    YesAlways,
    No,
}

/// UI-side implementation of the interactive approval prompt.
#[async_trait]
pub trait Approver: Send + Sync {
    async fn ask(&self, tool: &str, summary: &str, descriptor: &str) -> Approval;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(descriptor: &str, is_edit: bool) -> PermissionRequest {
        PermissionRequest::Ask {
            descriptor: descriptor.into(),
            summary: String::new(),
            is_edit,
        }
    }

    #[test]
    fn pattern_basics() {
        assert!(pattern_match("shell(git *)", "shell(git status --short)"));
        assert!(pattern_match("shell(cargo *)", "shell(cargo build)"));
        assert!(!pattern_match("shell(git *)", "shell(rm -rf /)"));
        assert!(pattern_match("edit(*)", "edit(src/main.rs)"));
        assert!(pattern_match("*", "anything"));
    }

    #[test]
    fn deny_beats_everything() {
        let rules = PermissionRules {
            allow: vec!["shell(*)".into()],
            deny: vec!["shell(rm *)".into()],
        };
        assert!(matches!(
            rules.decide(PermissionMode::Auto, &ask("shell(rm -rf x)", false)),
            Decision::Deny(_)
        ));
        assert_eq!(
            rules.decide(PermissionMode::Default, &ask("shell(ls)", false)),
            Decision::Allow
        );
    }

    #[test]
    fn modes() {
        let rules = PermissionRules::default();
        let edit = ask("edit(a.rs)", true);
        let shell = ask("shell(cargo test)", false);
        assert_eq!(
            rules.decide(PermissionMode::AcceptEdits, &edit),
            Decision::Allow
        );
        assert_eq!(
            rules.decide(PermissionMode::AcceptEdits, &shell),
            Decision::Ask
        );
        assert!(matches!(
            rules.decide(PermissionMode::Plan, &edit),
            Decision::Deny(_)
        ));
        assert_eq!(rules.decide(PermissionMode::Auto, &shell), Decision::Allow);
        assert_eq!(rules.decide(PermissionMode::Default, &shell), Decision::Ask);
    }
}
