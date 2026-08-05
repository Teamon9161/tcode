use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tool::PermissionRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
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
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "accept-edits",
            PermissionMode::Auto => "auto",
            PermissionMode::Unsafe => "unsafe",
        }
    }

    /// Whether this mode routes an ordinary side-effecting action to a human.
    /// Derived by asking `decide` rather than restating its arms, so it cannot
    /// drift from the real policy. Rules are excluded on purpose: this answers
    /// "does this mode expect somebody to be there", not "will this specific
    /// call prompt".
    pub fn expects_a_human(&self) -> bool {
        let probe = PermissionRequest::Ask {
            descriptor: String::new(),
            aliases: Vec::new(),
            summary: String::new(),
            is_edit: false,
        };
        matches!(
            PermissionRules::default().decide(*self, &probe),
            Decision::Ask
        )
    }

    pub fn cycle(&self) -> Self {
        match self {
            PermissionMode::Default => PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits => PermissionMode::Auto,
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
        // Submitting a draft plan for approval always reaches the human,
        // including in unsafe mode. It is not an authorization request that a
        // permissive stance could reasonably waive: the user asked to see the
        // plan, and the mode says how much risk they will take once it starts,
        // which is a separate question with a separate answer in this dialog.
        if matches!(request, PermissionRequest::PlanReview { .. }) {
            return Decision::Ask;
        }
        let PermissionRequest::Ask { is_edit, .. } = request else {
            return Decision::Allow;
        };
        let descriptors = request.rule_descriptors();
        // Deny and explicit checkpoints span the canonical concept and every
        // raw alias. This prevents a broad `run(*)` allow from bypassing a
        // deliberate `bash(rm *)` denial.
        if let Some(rule) = self.deny.iter().find(|rule| {
            descriptors
                .iter()
                .any(|descriptor| pattern_match(rule, descriptor))
        }) {
            return Decision::Deny(format!("denied by rule '{rule}'"));
        }
        if self.ask.iter().any(|rule| {
            descriptors
                .iter()
                .any(|descriptor| pattern_match(rule, descriptor))
        }) {
            return Decision::Ask;
        }
        match mode {
            PermissionMode::Unsafe => Decision::Allow,
            PermissionMode::AcceptEdits if *is_edit => Decision::Allow,
            PermissionMode::Auto => {
                if self.allow.iter().any(|rule| {
                    descriptors
                        .iter()
                        .any(|descriptor| pattern_match(rule, descriptor))
                }) {
                    Decision::Allow
                } else {
                    Decision::Auto
                }
            }
            _ => {
                if self.allow.iter().any(|rule| {
                    descriptors
                        .iter()
                        .any(|descriptor| pattern_match(rule, descriptor))
                }) {
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
    /// A permission-mode transition the approval carries. Plan review and an
    /// "allow all edits" approval choose the mode execution runs under; the
    /// agent loop applies it generically. `None` leaves the mode unchanged.
    pub set_mode: Option<PermissionMode>,
    /// Replacement input to execute after an approval. This preserves the
    /// assistant's append-only tool-use entry while allowing a review surface
    /// to turn an approved artifact (such as an edited plan) into the actual
    /// tool input and on-disk result.
    pub approved_input: Option<Value>,
    /// End the current agent turn once this approved action finishes
    /// successfully. This is an execution-control fact, not a permission
    /// mode: frontends use it to hand an accepted plan to a distinct session
    /// without allowing the planning model another request.
    pub end_turn_after_execution: bool,
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
            end_turn_after_execution: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Yes,
    /// Yes + persist an allow rule until this session ends.
    YesSession,
    /// Yes + add the canonical descriptor to `.tcode/config.toml` while also
    /// allowing this current call even if writing the config fails.
    YesProject,
    No,
}

/// One pending call inside a combined review. The fields mirror `Approver::ask`
/// exactly, so a front end can drive one item through the same widgets it
/// already uses for a single prompt.
pub struct BatchAsk<'a> {
    pub tool: &'a str,
    pub summary: &'a str,
    pub descriptor: &'a str,
    pub is_edit: bool,
    pub allows_project: bool,
    pub input: &'a Value,
}

/// The outcome of a combined review. The reviewer either answers once for the
/// whole set or asks to see the calls one at a time.
pub enum BatchApproval {
    /// This one answer applies to every call that was offered.
    All(Approval),
    /// Fall back to asking each call separately — the per-call flow is the
    /// authority, so nothing here needs to encode a partial verdict.
    Individually,
}

/// UI-side implementation of the interactive approval prompt.
///
/// `ask_with_call` carries one argument past what clippy will accept, and the
/// lint is declined rather than answered: these fields already travel together
/// as [`crate::tool::DelegatedApprovalRequest`], so the real fix is that struct
/// becoming the parameter — a change to seven implementations across three
/// frontends and the ACP adapter, not a signature tidy-up. Bundling them here
/// alone would leave two shapes for one call.
#[allow(clippy::too_many_arguments)]
#[async_trait]
pub trait Approver: Send + Sync {
    /// `input` is included so an interactive front end can show the exact
    /// file change before asking for consent.
    async fn ask(
        &self,
        tool: &str,
        summary: &str,
        descriptor: &str,
        is_edit: bool,
        allows_project: bool,
        input: &Value,
    ) -> Approval;

    /// Like [`Self::ask`], but supplies the provider-issued tool call id when
    /// the caller has one. Existing frontends retain the simpler method;
    /// protocol adapters can use this stable identity to correlate an approval
    /// with an external tool-call lifecycle.
    async fn ask_with_call(
        &self,
        _call_id: &str,
        tool: &str,
        summary: &str,
        descriptor: &str,
        is_edit: bool,
        allows_project: bool,
        input: &Value,
    ) -> Approval {
        self.ask(tool, summary, descriptor, is_edit, allows_project, input)
            .await
    }

    /// Review several pending calls at once. Only calls the harness would
    /// otherwise prompt for individually are offered here, so answering `All`
    /// can never authorize more than the per-call flow would have asked about.
    /// `label` is the batch header the agent loop would use for these calls,
    /// so the review names the change set exactly as the transcript will.
    ///
    /// The default keeps the per-call flow, so a front end that cannot show
    /// several changes at once needs no implementation of its own.
    async fn ask_batch(&self, _label: &str, _calls: &[BatchAsk<'_>]) -> BatchApproval {
        BatchApproval::Individually
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(descriptor: &str, is_edit: bool) -> PermissionRequest {
        PermissionRequest::Ask {
            descriptor: descriptor.into(),
            aliases: Vec::new(),
            summary: String::new(),
            is_edit,
        }
    }

    fn shell_request(kind: &str, command: &str) -> PermissionRequest {
        PermissionRequest::Ask {
            descriptor: format!("run({command})"),
            aliases: vec![format!("{kind}({command})")],
            summary: String::new(),
            is_edit: false,
        }
    }

    #[test]
    fn canonical_run_allows_both_shells_but_raw_rules_stay_specific() {
        let canonical = PermissionRules {
            allow: vec!["run(cargo *)".into()],
            ..Default::default()
        };
        assert_eq!(
            canonical.decide(
                PermissionMode::Default,
                &shell_request("shell", "cargo test")
            ),
            Decision::Allow
        );
        assert_eq!(
            canonical.decide(
                PermissionMode::Default,
                &shell_request("bash", "cargo test")
            ),
            Decision::Allow
        );

        let legacy = PermissionRules {
            allow: vec!["shell(cargo *)".into()],
            ..Default::default()
        };
        assert_eq!(
            legacy.decide(
                PermissionMode::Default,
                &shell_request("shell", "cargo test")
            ),
            Decision::Allow
        );
        assert_eq!(
            legacy.decide(
                PermissionMode::Default,
                &shell_request("bash", "cargo test")
            ),
            Decision::Ask
        );
    }

    #[test]
    fn raw_deny_and_ask_override_a_canonical_allow() {
        let rules = PermissionRules {
            allow: vec!["run(*)".into()],
            ask: vec!["bash(cargo *)".into()],
            deny: vec!["shell(rm *)".into()],
        };
        assert!(matches!(
            rules.decide(PermissionMode::Unsafe, &shell_request("shell", "rm -rf x")),
            Decision::Deny(_)
        ));
        assert_eq!(
            rules.decide(
                PermissionMode::Default,
                &shell_request("bash", "cargo test")
            ),
            Decision::Ask
        );
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
        assert_eq!(
            rules.decide(PermissionMode::Unsafe, &shell),
            Decision::Allow
        );
        assert_eq!(rules.decide(PermissionMode::Default, &shell), Decision::Ask);
    }

    #[test]
    fn modes_that_route_to_a_human_are_derived_from_decide() {
        for mode in [PermissionMode::Default, PermissionMode::AcceptEdits] {
            assert!(mode.expects_a_human(), "{mode:?} routes to a human");
        }
        // Auto only reaches a human when its classifier is unavailable, which
        // is an outage rather than the mode's normal path.
        for mode in [PermissionMode::Auto, PermissionMode::Unsafe] {
            assert!(!mode.expects_a_human(), "{mode:?} runs unattended");
        }
    }

    /// Planning is orthogonal to risk appetite, which is the whole reason it
    /// stopped being a permission mode: a plan under review must still reach
    /// the user in the modes that otherwise never prompt.
    #[test]
    fn plan_review_reaches_the_human_in_every_mode() {
        let rules = PermissionRules {
            allow: vec!["progress".into()],
            ask: vec![],
            deny: vec![],
        };
        let review = PermissionRequest::PlanReview {
            title: "Rewrite the resume path".into(),
        };
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Auto,
            PermissionMode::Unsafe,
        ] {
            assert_eq!(rules.decide(mode, &review), Decision::Ask, "{mode:?}");
        }
    }
}
