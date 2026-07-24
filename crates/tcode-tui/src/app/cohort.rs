//! Persistent transcript cards for a live cohort roster.
//!
//! The scheduler emits typed roster snapshots. This module turns them into
//! member-owned transcript blocks, so task traces remain reachable after the
//! generic live agent tree intentionally drops completed runs.

use super::*;

use tcode_core::{CohortChannelMessage, CohortMemberRun, CohortMemberStatus, CohortUpdate};

pub(super) struct CohortCard {
    round: usize,
    max_rounds: usize,
    channel_block: usize,
    messages: Vec<CohortChannelMessage>,
    members: Vec<CohortMemberCard>,
}

struct CohortMemberCard {
    id: String,
    kind: String,
    task: String,
    summary: String,
    model: String,
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
        if !self.cohorts.contains_key(&id) {
            let channel_block = self.transcript.block_count();
            self.bake(vec![cohort_channel_header(&id)]);
            self.transcript
                .link_cohort_channel(channel_block, id.clone());
            self.cohorts.insert(
                id.clone(),
                CohortCard {
                    round,
                    max_rounds,
                    channel_block,
                    messages: Vec::new(),
                    members: Vec::new(),
                },
            );
        }

        for member in members {
            let candidate_block = self.transcript.block_count();
            let (is_new, block, task, model, run, header) = {
                let card = self.cohorts.get_mut(&id).expect("cohort just inserted");
                card.round = round;
                card.max_rounds = max_rounds;
                let card_round = card.round;
                let card_max_rounds = card.max_rounds;
                if let Some(existing) = card.members.iter_mut().find(|item| item.id == member.id) {
                    existing.kind = member.kind;
                    existing.task = member.task;
                    existing.summary = member.summary;
                    existing.model = member.model;
                    if existing.run.is_none() {
                        existing.run = member.run;
                    }
                    existing.status = member.status;
                    let header = cohort_member_header(
                        &existing.id,
                        &existing.kind,
                        &existing.summary,
                        existing.status,
                        card_round,
                        card_max_rounds,
                    );
                    (
                        false,
                        existing.block,
                        existing.task.clone(),
                        existing.model.clone(),
                        existing.run.clone(),
                        header,
                    )
                } else {
                    let header = cohort_member_header(
                        &member.id,
                        &member.kind,
                        &member.summary,
                        member.status,
                        card_round,
                        card_max_rounds,
                    );
                    card.members.push(CohortMemberCard {
                        id: member.id,
                        kind: member.kind,
                        task: member.task.clone(),
                        summary: member.summary,
                        model: member.model.clone(),
                        run: member.run.clone(),
                        status: member.status,
                        block: candidate_block,
                    });
                    (
                        true,
                        candidate_block,
                        member.task,
                        member.model,
                        member.run,
                        header,
                    )
                }
            };
            if is_new {
                self.bake(vec![header]);
                self.transcript.attach_detail(
                    block,
                    task_summary_detail_with_model(&task, &model),
                    OUTPUT_VIEW_ROWS,
                );
            } else {
                self.transcript
                    .replace_head_preserving_state(block, vec![header]);
            }
            if let Some(run) = run {
                self.transcript.link_task_run(block, run);
            }
        }
    }

    /// Record a shared-channel post independently of tool batches. The channel
    /// card opens a dedicated discussion view; member cards still open their
    /// own private traces.
    pub(super) fn on_cohort_channel_message(&mut self, message: CohortChannelMessage) {
        let Some(card) = self.cohorts.get_mut(&message.cohort_id) else {
            return;
        };
        card.messages.push(message.clone());
        let header = cohort_channel_header_with_count(&message.cohort_id, card.messages.len());
        self.transcript
            .replace_head_preserving_state(card.channel_block, vec![header]);
        self.append_open_cohort_channel(&message);
    }

    pub(super) fn cohort_channel_messages(&self, id: &str) -> Option<&[CohortChannelMessage]> {
        self.cohorts.get(id).map(|card| card.messages.as_slice())
    }

    pub(super) fn cohort_channel_message_lines(
        message: &CohortChannelMessage,
    ) -> Vec<Line<'static>> {
        let to = message.to.as_deref().unwrap_or("all");
        let mut lines = vec![Line::from(vec![Span::styled(
            format!(
                "  {} · round {} · {} → {}",
                message.seq,
                message.round + 1,
                message.from,
                to
            ),
            theme::metadata(),
        )])];
        lines.extend(message.body.lines().map(|line| {
            Line::from(vec![
                Span::styled("    │ ", theme::dim()),
                Span::raw(line.to_owned()),
            ])
        }));
        lines.push(Line::default());
        lines
    }

    /// Bind the first activation's trace to its persistent member card. Later
    /// round traces append to that same root run, so Ctrl+click opens the whole
    /// member conversation rather than an isolated incremental turn.
    pub(super) fn cohort_member_started(
        &mut self,
        membership: &CohortMemberRun,
        run: &str,
    ) -> Option<(usize, String, String)> {
        let (block, summary, root, header, run_to_link) = {
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
                &member.summary,
                member.status,
                card_round,
                card_max_rounds,
            );
            (
                member.block,
                member.summary.clone(),
                member
                    .run
                    .clone()
                    .expect("cohort member root run is assigned"),
                header,
                run_to_link,
            )
        };
        self.transcript
            .replace_head_preserving_state(block, vec![header]);
        if let Some(root) = run_to_link {
            self.transcript.link_task_run(block, root);
        }
        Some((block, summary, root))
    }
}

fn cohort_channel_header(id: &str) -> Line<'static> {
    cohort_channel_header_with_count(id, 0)
}

fn cohort_channel_header_with_count(id: &str, count: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  ◈ cohort {id} · channel"), theme::accent()),
        Span::styled(
            format!(" · {count} messages · ctrl+click to view"),
            theme::dim(),
        ),
    ])
}

fn cohort_member_header(
    member: &str,
    kind: &str,
    summary: &str,
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
        Span::styled(format!("  ├ {member} · {kind} · {summary}"), theme::bold()),
        Span::styled(
            format!(" · round {}/{} · {label}", round + 1, max_rounds),
            style,
        ),
    ])
}
