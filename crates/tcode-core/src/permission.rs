use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    /// Actions run without routine prompts; non-safe calls are reviewed by the
    /// configured safety classifier.
    Auto,
    /// Everything runs without asking (deny rules still apply). This is an
    /// explicit bypass for isolated environments, not Auto Mode.
    Unsafe,
}

impl PermissionMode {
    pub fn label(&self) -> &'static str {
        match self {
            PermissionMode::Plan => "plan",
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "accept-edits",
            PermissionMode::Auto => "auto",
            PermissionMode::Unsafe => "unsafe",
        }
    }

    pub fn cycle(&self) -> Self {
        match self {
            PermissionMode::Default => PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits => PermissionMode::Plan,
            PermissionMode::Plan => PermissionMode::Auto,
            PermissionMode::Auto => PermissionMode::Unsafe,
            PermissionMode::Unsafe => PermissionMode::Default,
        }
    }
}

/// Rules match descriptors like "shell(git status --short)" against
/// patterns like "shell(git *)". `*` is the only wildcard.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionRules {
    pub allow: Vec<String>,
    /// Explicit human checkpoints. Matches here always prompt, including in
    /// Auto and Unsafe mode.
    pub ask: Vec<String>,
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
    Ask,
    /// Auto Mode still needs tool-specific routing and possibly a model
    /// decision. The agent resolves this with `Tool::auto_safety`.
    Auto,
}

impl PermissionRules {
    pub fn decide(&self, mode: PermissionMode, request: &PermissionRequest) -> Decision {
        if matches!(request, PermissionRequest::UserInput { .. }) {
            return Decision::Ask;
        }
        // Plan review is the one gate that turns plan mode *off*: it must reach
        // the human in plan mode (never auto-approved), and outside plan mode
        // it is a no-op the model is nudged to correct rather than a prompt.
        if matches!(request, PermissionRequest::PlanReview { .. }) {
            return if mode == PermissionMode::Plan {
                Decision::Ask
            } else {
                Decision::Deny(
                    "not in plan mode; nothing to exit. If you want to record a plan, just write it in your reply.".into(),
                )
            };
        }
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
        // Explicit human checkpoints cannot be auto-approved by either an
        // allow rule or the classifier.
        if self.ask.iter().any(|r| pattern_match(r, descriptor)) {
            return Decision::Ask;
        }
        match mode {
            PermissionMode::Plan => {
                Decision::Deny("blocked: plan mode is active; only read-only tools may run".into())
            }
            PermissionMode::Unsafe => Decision::Allow,
            PermissionMode::AcceptEdits if *is_edit => Decision::Allow,
            PermissionMode::Auto => {
                if self.allow.iter().any(|r| pattern_match(r, descriptor)) {
                    Decision::Allow
                } else {
                    Decision::Auto
                }
            }
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
            Some(('*', rest)) => (0..=t.len()).any(|i| inner(rest, &t[i..])),
            Some((c, rest)) => t
                .split_first()
                .is_some_and(|(tc, tr)| tc == c && inner(rest, tr)),
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
    /// A permission-mode transition the approval carries. Set only by the plan
    /// review dialog (approving a plan chooses the mode execution runs under);
    /// the agent loop applies it generically. `None` for every ordinary
    /// approval, which never changes the mode.
    pub set_mode: Option<PermissionMode>,
    /// Replacement input to execute after an approval. This preserves the
    /// assistant's append-only tool-use entry while allowing a review surface
    /// to turn an approved artifact (such as an edited plan) into the actual
    /// tool input and on-disk result.
    pub approved_input: Option<Value>,
}

impl Approval {
    /// A plain yes/no/always answer with no mode transition — the shape of
    /// every ordinary approval.
    pub fn simple(decision: ApprovalDecision, comment: Option<String>) -> Self {
        Self {
            decision,
            comment,
            set_mode: None,
            approved_input: None,
        }
    }
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
    /// `input` is included so an interactive front end can show the exact
    /// file change before asking for consent.
    async fn ask(&self, tool: &str, summary: &str, descriptor: &str, input: &Value) -> Approval;
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
            ask: vec![],
            deny: vec!["shell(rm *)".into()],
        };
        assert!(matches!(
            rules.decide(PermissionMode::Unsafe, &ask("shell(rm -rf x)", false)),
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
        assert_eq!(
            rules.decide(PermissionMode::Unsafe, &shell),
            Decision::Allow
        );
        assert_eq!(rules.decide(PermissionMode::Default, &shell), Decision::Ask);
    }
}
