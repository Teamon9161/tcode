//! Persistent transcript cards for a live cohort roster.
//!
//! The scheduler emits typed roster snapshots. This module turns them into
//! member-owned transcript blocks, so task traces remain reachable after the
//! generic live agent tree intentionally drops completed runs.

use super::*;

use tcode_core::{CohortMemberRun, CohortMemberStatus, CohortUpdate};

pub(super) struct CohortCard {
    round: usize,
    max_rounds: usize,
    members: Vec<CohortMemberCard>,
}

struct CohortMemberCard {
    id: String,
    kind: String,
    task: String,
    run: Option<String>,
    status: CohortMemberStatus,
    block: usize,
}

impl App {
    /// Apply a full roster snapshot in place. A cohort is a long-lived tool
    /// call, unlike an ordinary one-shot task, so its member cards persist in
    /// the transcript after their individual activations finish.
    pub(super) fn on_cohort_updated(&mut self, update: CohortUpdate) {
        let CohortUpdate {
            id,
            parent_call: _,
            round,
            max_rounds,
            members,
        } = update;
        self.cohorts
            .entry(id.clone())
            .or_insert_with(|| CohortCard {
                round,
                max_rounds,
                members: Vec::new(),
            });

        for member in members {
            let candidate_block = self.transcript.block_count();
            let (is_new, block, task, run, header) = {
                let card = self.cohorts.get_mut(&id).expect("cohort just inserted");
                card.round = round;
                card.max_rounds = max_rounds;
                let card_round = card.round;
                let card_max_rounds = card.max_rounds;
                if let Some(existing) = card.members.iter_mut().find(|item| item.id == member.id) {
                    existing.kind = member.kind;
                    existing.task = member.task;
                    if existing.run.is_none() {
                        existing.run = member.run;
                    }
                    existing.status = member.status;
                    let header = cohort_member_header(
                        &existing.id,
                        &existing.kind,
                        &existing.task,
                        existing.status,
                        card_round,
                        card_max_rounds,
                    );
                    (
                        false,
                        existing.block,
                        existing.task.clone(),
                        existing.run.clone(),
                        header,
                    )
                } else {
                    let header = cohort_member_header(
                        &member.id,
                        &member.kind,
                        &member.task,
                        member.status,
                        card_round,
                        card_max_rounds,
                    );
                    card.members.push(CohortMemberCard {
                        id: member.id,
                        kind: member.kind,
                        task: member.task.clone(),
                        run: member.run.clone(),
                        status: member.status,
                        block: candidate_block,
                    });
                    (true, candidate_block, member.task, member.run, header)
                }
            };
            if is_new {
                self.bake(vec![header]);
                self.transcript
                    .attach_detail(block, task_summary_detail(&task), OUTPUT_VIEW_ROWS);
            } else {
                self.transcript
                    .replace_head_preserving_state(block, vec![header]);
            }
            if let Some(run) = run {
                self.transcript.link_task_run(block, run);
            }
        }
    }

    /// Bind the first activation's trace to its persistent member card. Later
    /// round traces append to that same root run, so Ctrl+click opens the whole
    /// member conversation rather than an isolated incremental turn.
    pub(super) fn cohort_member_started(
        &mut self,
        membership: &CohortMemberRun,
        run: &str,
    ) -> Option<(usize, String)> {
        let (block, summary, header, run_to_link) = {
            let card = self.cohorts.get_mut(&membership.cohort_id)?;
            let card_round = card.round;
            let card_max_rounds = card.max_rounds;
            let member = card
                .members
                .iter_mut()
                .find(|member| member.id == membership.member_id)?;
            let run_to_link = member.run.is_none().then(|| run.to_string());
            if let Some(root) = &run_to_link {
                member.run = Some(root.clone());
            }
            member.status = CohortMemberStatus::Running;
            let header = cohort_member_header(
                &member.id,
                &member.kind,
                &member.task,
                member.status,
                card_round,
                card_max_rounds,
            );
            (member.block, member.task.clone(), header, run_to_link)
        };
        self.transcript
            .replace_head_preserving_state(block, vec![header]);
        if let Some(root) = run_to_link {
            self.transcript.link_task_run(block, root);
        }
        Some((block, summary))
    }
}

fn cohort_member_header(
    member: &str,
    kind: &str,
    task: &str,
    status: CohortMemberStatus,
    round: usize,
    max_rounds: usize,
) -> Line<'static> {
    let (label, style) = match status {
        CohortMemberStatus::Waiting => ("waiting", theme::dim()),
        CohortMemberStatus::Running => ("running", theme::accent()),
        CohortMemberStatus::Left => ("left", theme::warn()),
        CohortMemberStatus::Finalizing => ("finalizing", theme::accent()),
        CohortMemberStatus::Done => ("done", theme::ok()),
        CohortMemberStatus::Failed => ("failed", theme::error_highlight()),
    };
    Line::from(vec![
        Span::styled(format!("  ├ {member} · {kind} · {task}"), theme::bold()),
        Span::styled(
            format!(" · round {}/{} · {label}", round + 1, max_rounds),
            style,
        ),
    ])
}
