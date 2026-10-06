//! Use case 8, Notify (job `notify.ticket`, intake.md): the one path from Mage to Linear and
//! Slack. Every target goes through `deliver_once`: an `OutboundLog` row with a pre-allocated
//! identity is written before the provider call, so a retry after an unknown outcome is
//! reconciled (Linear: same comment id; Slack: lookup by metadata key) instead of repeated.

use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{
    ArtifactId, ArtifactKind, Cause, GateKind, GateStatus, IntegrationKind, IntegrationStatus, Notice, OrgId, RunId, RunResult, RunStatus,
    TargetOrigin, Ticket, TicketId, TicketSource, Verdict,
};
use mage_core::ports::blob::BlobKey;
use mage_core::ports::{ArtifactStore, Clock, IdGen, RepoStore};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use url::Url;

use crate::app::fix::media::MediaCache;
use crate::app::intake::error::IntakeError;
use crate::app::intake::templates::{self, CauseLine, DiagnosisOutcome, Links, NoticeContext, NoticeDetail, Rendered, TargetLine};
use crate::domain::intake::NoticeKind;
use crate::ports::chat::{ChatError, ChatNotifier, ChatPost};
use crate::ports::org::OrgStore;
use crate::ports::outbound::{OutboundKey, OutboundLog, OutboundState, OutboundTarget};
use crate::ports::runs::{ArtifactRecord, ArtifactState, GateRecord, RunStore};
use crate::ports::ticket::{TicketStore, TicketStoreError};
use crate::ports::tracker::{CommentDraft, LinkAttachment, TicketTracker, TrackerError, TrackerUpload};

/// Lookback before an uncertain Slack attempt's row creation when searching for the reply.
const FIND_REPLY_SLACK: time::Duration = time::Duration::seconds(60);
/// Largest mp4 uploaded to Linear (fix.md targets ≤ 10 MB; GitHub paid plans allow 100 MB).
const VIDEO_MAX_BYTES: u64 = 100 * 1024 * 1024;
/// `MediaCache` destination name for Linear uploads.
const MEDIA_DESTINATION: &str = "linear";
/// Retry after an `Unauthorized` provider answer (the integration check may revoke meanwhile).
const UNAUTHORIZED_RETRY: Duration = Duration::from_secs(5 * 60);

/// What happened to one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivered {
    /// Already `Sent`, `Skipped` or `Failed` before this run.
    AlreadyFinal,
    /// Not delivered yet; the job retries (the row stays `Pending`).
    Retrying(String),
    Sent { external_id: String, reconciled: bool },
    Skipped(String),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NotifyOutcome {
    pub targets: Vec<(OutboundTarget, Delivered)>,
    /// The ticket no longer exists.
    pub ticket_gone: bool,
}

pub struct Notify {
    orgs: Arc<dyn OrgStore>,
    tickets: Arc<dyn TicketStore>,
    runs: Arc<dyn RunStore>,
    repos: Arc<dyn RepoStore>,
    outbound: Arc<dyn OutboundLog>,
    tracker: Arc<dyn TicketTracker>,
    chat: Arc<dyn ChatNotifier>,
    artifacts: Arc<dyn ArtifactStore>,
    media: Arc<MediaCache>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
    public_url: Url,
}

/// A retry the job must schedule (the earliest wins when several targets need one).
struct Pending {
    reason: String,
    at: Option<OffsetDateTime>,
}

fn sha256(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

fn retrying(reason: String, at: Option<OffsetDateTime>) -> (Delivered, Option<Pending>) {
    (Delivered::Retrying(reason.clone()), Some(Pending { reason, at }))
}

impl Notify {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        orgs: Arc<dyn OrgStore>,
        tickets: Arc<dyn TicketStore>,
        runs: Arc<dyn RunStore>,
        repos: Arc<dyn RepoStore>,
        outbound: Arc<dyn OutboundLog>,
        tracker: Arc<dyn TicketTracker>,
        chat: Arc<dyn ChatNotifier>,
        artifacts: Arc<dyn ArtifactStore>,
        media: Arc<MediaCache>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGen>,
        public_url: Url,
    ) -> Self {
        Self { orgs, tickets, runs, repos, outbound, tracker, chat, artifacts, media, clock, ids, public_url }
    }

    pub async fn run(&self, org: OrgId, ticket_id: TicketId, notice: &Notice) -> Result<NotifyOutcome, IntakeError> {
        // 1. Ticket and notice context.
        let ticket = match self.tickets.get_ticket(org, ticket_id).await {
            Ok(t) => t.value,
            Err(TicketStoreError::NotFound) => return Ok(NotifyOutcome { targets: vec![], ticket_gone: true }),
            Err(e) => return Err(e.into()),
        };
        let cfg = self.orgs.intake_config(org).await?;
        let integrations = self.orgs.integrations(org).await?;
        let active = |kind: IntegrationKind| integrations.iter().any(|i| i.kind == kind && i.status == IntegrationStatus::Active);
        let (mut ctx, video) = self.context(org, &ticket, notice).await?;
        let links = Links { public_url: &self.public_url };

        // 2. Render.
        let rendered = templates::render(&ctx, &cfg.notify, &links);

        // 3. Targets.
        let kind = NoticeKind::of(notice);
        let TicketSource::Linear { issue_id, .. } = &ticket.source;
        let linear_active = active(IntegrationKind::Linear);
        let mut outcome = NotifyOutcome::default();
        let mut pending: Vec<Pending> = Vec::new();
        let mut note = |p: Option<Pending>| pending.extend(p);

        // 4. deliver_once per target.
        if linear_active && cfg.notify.linear.contains(&kind) && rendered.linear_markdown.is_some() {
            let key = OutboundKey { ticket_id, target: OutboundTarget::LinearComment, notice_key: notice.key() };
            let (d, p) = self.deliver_linear(org, issue_id, &key, &mut ctx, &rendered, video, &links).await?;
            outcome.targets.push((OutboundTarget::LinearComment, d));
            note(p);
        }
        if let Some(thread) = ticket.slack_thread.as_ref().filter(|_| active(IntegrationKind::Slack) && cfg.notify.slack.contains(&kind)) {
            let key = OutboundKey { ticket_id, target: OutboundTarget::SlackReply, notice_key: notice.key() };
            let (d, p) = self.deliver_slack(org, thread, &key, &rendered).await?;
            outcome.targets.push((OutboundTarget::SlackReply, d));
            note(p);
        }

        // 5. Sidebar attachment, every notice; 6. state on PR.
        if linear_active {
            let link = LinkAttachment {
                url: links.ticket(ticket_id),
                title: "Mage".into(),
                subtitle: rendered.attachment_subtitle.clone(),
                metadata: [("ticket_id".to_owned(), ticket_id.to_string()), ("notice".to_owned(), notice.key())].into_iter().collect(),
            };
            note(self.side_effect("attach_link", self.tracker.attach_link(org, issue_id, &link).await));
            if matches!(notice, Notice::PrOpened { .. }) && !cfg.notify.linear_state_on_pr.is_empty() {
                note(self.side_effect("set_state", self.tracker.set_state(org, issue_id, &cfg.notify.linear_state_on_pr).await));
            }
        }

        // 7. Complete unless a target asked for a retry (targets already sent are skipped then).
        // A provider deadline wins over backoff; with several, the latest (earlier would hit the
        // limit again).
        let reason = pending.iter().map(|p| p.reason.as_str()).collect::<Vec<_>>().join("; ");
        match (pending.is_empty(), pending.iter().filter_map(|p| p.at).max()) {
            (true, _) => Ok(outcome),
            (false, Some(at)) => Err(IntakeError::retry_at(reason, at)),
            (false, None) => Err(IntakeError::retry(reason)),
        }
    }

    /// Attachment / state change: transient errors retry the job, permanent ones are logged.
    fn side_effect(&self, what: &str, result: Result<(), TrackerError>) -> Option<Pending> {
        match result {
            Ok(()) => None,
            Err(TrackerError::RateLimited { retry_at }) => Some(Pending { reason: format!("Linear {what} rate limited"), at: Some(retry_at) }),
            Err(TrackerError::Unavailable(e)) => Some(Pending { reason: format!("Linear {what}: {e}"), at: None }),
            Err(TrackerError::Unauthorized) => {
                Some(Pending { reason: format!("Linear {what} unauthorized"), at: Some(self.clock.now() + UNAUTHORIZED_RETRY) })
            }
            Err(e) => {
                tracing::warn!(what, error = %e, "Linear side effect failed permanently");
                None
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn deliver_linear(
        &self,
        org: OrgId,
        issue_id: &str,
        key: &OutboundKey,
        ctx: &mut NoticeContext,
        rendered: &Rendered,
        video: Option<ArtifactRecord>,
        links: &Links<'_>,
    ) -> Result<(Delivered, Option<Pending>), IntakeError> {
        let body = rendered.linear_markdown.clone().unwrap_or_default();
        let rec = self.outbound.begin(org, key, self.ids.uuid_v4(), sha256(&body)).await?;
        if matches!(rec.state, OutboundState::Sent | OutboundState::Skipped { .. } | OutboundState::Failed { .. }) {
            return Ok((Delivered::AlreadyFinal, None));
        }
        // The mp4 goes into the comment (fix.md "Media"); an upload failure never blocks it.
        let mut body = body;
        if let Some(video) = video {
            if let Some(asset) = self.upload_video(org, &video).await? {
                set_video_asset(&mut ctx.detail, asset);
                body = templates::render(ctx, &self.orgs.intake_config(org).await?.notify, links).linear_markdown.unwrap_or(body);
            }
        }
        self.outbound.mark_attempt(org, key, true, None).await?;
        let result = self.tracker.comment(org, issue_id, &CommentDraft { client_id: rec.client_id, body_markdown: body }).await;
        let now = self.clock.now();
        Ok(match result {
            Ok(posted) => {
                self.outbound.mark_sent(org, key, &posted.id, now).await?;
                (Delivered::Sent { external_id: posted.id, reconciled: !posted.created }, None)
            }
            Err(TrackerError::RateLimited { retry_at }) => {
                self.outbound.mark_attempt(org, key, false, Some("rate limited")).await?;
                retrying("Linear rate limited".into(), Some(retry_at))
            }
            Err(TrackerError::NotFound) => {
                self.outbound.mark_skipped(org, key, "issue not found").await?;
                (Delivered::Skipped("issue not found".into()), None)
            }
            // Template bug or rejected input: no retry.
            Err(TrackerError::Invalid(e)) => {
                self.outbound.mark_failed(org, key, &e).await?;
                (Delivered::Failed(e), None)
            }
            Err(TrackerError::Unauthorized) => {
                self.outbound.mark_attempt(org, key, false, Some("unauthorized")).await?;
                retrying("Linear unauthorized".into(), Some(now + UNAUTHORIZED_RETRY))
            }
            // Outcome unknown: the row stays uncertain; the retry resends the same comment id.
            Err(TrackerError::Unavailable(e)) => retrying(format!("Linear unavailable: {e}"), None),
            Err(TrackerError::Conflict) => retrying("Linear conflict".into(), None),
        })
    }

    async fn deliver_slack(
        &self,
        org: OrgId,
        thread: &mage_core::domain::SlackThreadRef,
        key: &OutboundKey,
        rendered: &Rendered,
    ) -> Result<(Delivered, Option<Pending>), IntakeError> {
        let key_str = format!("{}:{}", key.ticket_id, key.notice_key);
        let rec = self.outbound.begin(org, key, self.ids.uuid_v4(), sha256(&rendered.chat_text)).await?;
        match rec.state {
            OutboundState::Sent | OutboundState::Skipped { .. } | OutboundState::Failed { .. } => return Ok((Delivered::AlreadyFinal, None)),
            OutboundState::Pending { uncertain: true } => {
                match self.chat.find_reply(org, thread, &key_str, rec.created_at - FIND_REPLY_SLACK).await {
                    Ok(Some(found)) => {
                        self.outbound.mark_sent(org, key, &found.ts, self.clock.now()).await?;
                        return Ok((Delivered::Sent { external_id: found.ts, reconciled: true }, None));
                    }
                    Ok(None) => {}
                    Err(e) => return Ok(self.slack_failure(org, key, e).await?),
                }
            }
            OutboundState::Pending { uncertain: false } => {}
        }
        self.outbound.mark_attempt(org, key, true, None).await?;
        let post = ChatPost { key: key_str, text: rendered.chat_text.clone(), link: rendered.chat_link.clone() };
        match self.chat.post_reply(org, thread, &post).await {
            Ok(posted) => {
                self.outbound.mark_sent(org, key, &posted.ts, self.clock.now()).await?;
                Ok((Delivered::Sent { external_id: posted.ts, reconciled: false }, None))
            }
            Err(e) => self.slack_failure(org, key, e).await,
        }
    }

    async fn slack_failure(&self, org: OrgId, key: &OutboundKey, e: ChatError) -> Result<(Delivered, Option<Pending>), IntakeError> {
        let now = self.clock.now();
        Ok(match e {
            ChatError::RateLimited { retry_at } => {
                self.outbound.mark_attempt(org, key, false, Some("rate limited")).await?;
                retrying("Slack rate limited".into(), Some(retry_at))
            }
            // Bot not in channel, thread gone: permanent until an admin acts.
            ChatError::NoAccess(reason) => {
                self.outbound.mark_skipped(org, key, &reason).await?;
                (Delivered::Skipped(reason), None)
            }
            ChatError::NotFound => {
                self.outbound.mark_skipped(org, key, "thread not found").await?;
                (Delivered::Skipped("thread not found".into()), None)
            }
            ChatError::Invalid(reason) => {
                self.outbound.mark_failed(org, key, &reason).await?;
                (Delivered::Failed(reason), None)
            }
            ChatError::Unauthorized => {
                self.outbound.mark_attempt(org, key, false, Some("unauthorized")).await?;
                retrying("Slack unauthorized".into(), Some(now + UNAUTHORIZED_RETRY))
            }
            // Outcome unknown: stays uncertain; the retry looks the reply up by key first.
            ChatError::Unavailable(reason) => retrying(format!("Slack unavailable: {reason}"), None),
            ChatError::Conflict => retrying("Slack conflict".into(), None),
        })
    }

    /// Upload the mp4 once per artifact (MediaCache); any failure leaves the comment without it.
    async fn upload_video(&self, org: OrgId, video: &ArtifactRecord) -> Result<Option<Url>, IntakeError> {
        let id = video.artifact.id;
        match self.media.get(org, id, MEDIA_DESTINATION).await {
            Ok(Some(url)) => return Ok(Some(url)),
            Ok(None) => {}
            Err(e) => tracing::warn!(%org, artifact = %id, error = %e, "media cache read failed"),
        }
        if video.artifact.bytes > VIDEO_MAX_BYTES {
            return Ok(None);
        }
        let bytes = match BlobKey::parse(&video.artifact.key) {
            Ok(key) => match self.artifacts.get(&key, VIDEO_MAX_BYTES).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(%org, artifact = %id, error = %e, "video not readable; comment without it");
                    return Ok(None);
                }
            },
            Err(e) => {
                tracing::warn!(%org, artifact = %id, error = %e, "bad artifact key");
                return Ok(None);
            }
        };
        let upload = TrackerUpload { file_name: video.name.clone(), content_type: "video/mp4".into(), bytes };
        match self.tracker.upload_file(org, &upload).await {
            Ok(url) => {
                if let Err(e) = self.media.put(org, id, MEDIA_DESTINATION, &url).await {
                    tracing::warn!(%org, artifact = %id, error = %e, "media cache write failed");
                }
                Ok(Some(url))
            }
            Err(TrackerError::RateLimited { retry_at }) => Err(IntakeError::retry_at("Linear upload rate limited", retry_at)),
            Err(e) => {
                tracing::warn!(%org, artifact = %id, error = %e, "Linear upload failed; comment without the video");
                Ok(None)
            }
        }
    }

    /// Step 1: the data each template needs, plus the mp4 to upload (if any).
    async fn context(&self, org: OrgId, ticket: &Ticket, notice: &Notice) -> Result<(NoticeContext, Option<ArtifactRecord>), IntakeError> {
        let TicketSource::Linear { identifier, .. } = &ticket.source;
        let mut target = None;
        let mut repo = None;
        if let Some(t) = &ticket.target {
            let r = self.repos.repo(org, t.repo_id).await?;
            let deployed_at = match &t.origin {
                TargetOrigin::Deployment { environment } => {
                    self.repos.latest_deployment(org, t.repo_id, environment, ticket.created_at).await?.filter(|d| d.sha == t.sha).map(|d| d.observed_at)
                }
                _ => None,
            };
            target = Some(TargetLine { sha: t.sha, origin: t.origin.clone(), deployed_at });
            repo = Some((r.owner, r.name));
        }
        let (detail, video) = match notice {
            Notice::TriageReady { run_id } => {
                let mut causes: Vec<Cause> = self.tickets.causes(org, ticket.id, false).await?.into_iter().filter(|c| c.triage_run == *run_id).collect();
                causes.sort_by_key(|c| c.rank);
                let total = causes.len();
                let lines = causes.iter().map(|c| CauseLine { title: c.title.clone(), confidence: c.confidence.get(), citations: c.evidence.clone() }).collect();
                (NoticeDetail::TriageReady { causes: lines, total }, None)
            }
            Notice::DiagnosisVerdict { cause_id, run_id } => {
                let cause = self.tickets.get_cause(org, *cause_id).await?;
                let run = self.runs.get(org, *run_id).await?.run;
                let gate = self.latest_gate(org, *run_id, GateKind::Repro).await?;
                let (verdict, script) = match &run.result {
                    Some(RunResult::Diagnose(r)) => (Some(r.verdict.clone()), r.script),
                    _ => (None, None),
                };
                let outcome = match (run.status, verdict) {
                    (RunStatus::TimedOut, _) => DiagnosisOutcome::TimedOut,
                    (RunStatus::Failed, _) if gate.as_ref().is_some_and(|g| matches!(g.gate.status, GateStatus::Failed { .. })) => {
                        DiagnosisOutcome::GateFailed { attempts: gate.as_ref().map_or(0, |g| g.gate.attempt) }
                    }
                    (_, Some(Verdict::Reproduced)) => DiagnosisOutcome::Reproduced,
                    (_, Some(Verdict::NotReproduced)) => DiagnosisOutcome::NotReproduced,
                    (_, Some(Verdict::Inconclusive { reason })) => DiagnosisOutcome::Inconclusive { reason },
                    (status, None) => DiagnosisOutcome::Inconclusive { reason: format!("run {status:?}") },
                };
                let media = self.gate_media(org, *run_id, gate.as_ref()).await?;
                let detail = NoticeDetail::DiagnosisVerdict {
                    run_id: *run_id,
                    cause_rank: cause.rank,
                    cause_title: cause.title.clone(),
                    outcome,
                    gate_attempt: gate.as_ref().map(|g| g.gate.attempt),
                    script,
                    video: media.video.as_ref().map(|v| v.artifact.id),
                    trace: media.trace,
                    video_asset: None,
                };
                (detail, media.video)
            }
            Notice::FixBriefReady { cause_id, version } => {
                let cause = self.tickets.get_cause(org, *cause_id).await?;
                let brief = self.tickets.fix_briefs(org, *cause_id).await?.into_iter().find(|b| b.version == *version);
                let mode = self.orgs.fix_settings(org).await?.brief_in_linear;
                let detail = NoticeDetail::FixBriefReady {
                    cause_id: *cause_id,
                    cause_title: cause.title.clone(),
                    version: *version,
                    mode,
                    brief_markdown: brief.map(|b| b.markdown).unwrap_or_default(),
                };
                (detail, None)
            }
            Notice::FixReady { cause_id, run_id } => {
                let cause = self.tickets.get_cause(org, *cause_id).await?;
                let run = self.runs.get(org, *run_id).await?.run;
                let (stats, summary) = match &run.result {
                    Some(RunResult::Fix(r)) => (r.stats.clone(), r.summary.clone()),
                    _ => (Default::default(), String::new()),
                };
                let gate = self.latest_gate(org, *run_id, GateKind::Fix).await?;
                let media = self.gate_media(org, *run_id, gate.as_ref()).await?;
                (NoticeDetail::FixReady { run_id: *run_id, cause_rank: cause.rank, stats, summary, video_asset: None }, media.video)
            }
            Notice::FixFailed { cause_id, run_id } => {
                let cause = self.tickets.get_cause(org, *cause_id).await?;
                let run = self.runs.get(org, *run_id).await?.run;
                let gate = self.latest_gate(org, *run_id, GateKind::Fix).await?;
                let reason = match gate.as_ref().map(|g| &g.gate.status) {
                    Some(GateStatus::Failed { reason, .. }) => format!("fix gate failed (attempt {}): {reason}", gate.as_ref().map_or(0, |g| g.gate.attempt)),
                    _ => match &run.error {
                        Some(err) => err.message.clone(),
                        None => format!("fix run ended {:?}", run.status),
                    },
                };
                let media = self.gate_media(org, *run_id, gate.as_ref()).await?;
                (NoticeDetail::FixFailed { run_id: *run_id, cause_rank: cause.rank, reason, video_asset: None }, media.video)
            }
            Notice::PrOpened { url, .. } => {
                let number = url.path_segments().and_then(|mut s| s.next_back()).and_then(|n| n.parse().ok());
                let detail = NoticeDetail::PrOpened {
                    url: url.clone(),
                    number,
                    gate_summary: "Fix gate passed: the repro script passes and the project tests pass.".into(),
                };
                (detail, None)
            }
        };
        Ok((NoticeContext { ticket_id: ticket.id, identifier: identifier.clone(), target, repo, detail }, video))
    }

    async fn latest_gate(&self, org: OrgId, run: RunId, kind: GateKind) -> Result<Option<GateRecord>, IntakeError> {
        let gates = self.runs.gates(org, run).await?;
        Ok(gates.into_iter().filter(|g| g.gate.kind == kind).max_by_key(|g| g.gate.attempt))
    }

    /// The recording of the gate's Verify run (mp4 `Video`, `Trace`), else of the run itself.
    async fn gate_media(&self, org: OrgId, run: RunId, gate: Option<&GateRecord>) -> Result<Media, IntakeError> {
        let mut sources = Vec::new();
        if let Some(verify) = gate.and_then(|g| g.gate.verify_run) {
            sources.push(verify);
        }
        sources.push(run);
        for source in sources {
            let artifacts = self.runs.artifacts(org, source).await?;
            let complete = |kind: ArtifactKind| artifacts.iter().filter(move |a| a.artifact.kind == kind && a.state == ArtifactState::Complete);
            let video = complete(ArtifactKind::Video).filter(|a| is_mp4(a)).last().cloned();
            let trace = complete(ArtifactKind::Trace).last().map(|a| a.artifact.id);
            if video.is_some() || trace.is_some() {
                return Ok(Media { video, trace });
            }
        }
        Ok(Media { video: None, trace: None })
    }
}

struct Media {
    video: Option<ArtifactRecord>,
    trace: Option<ArtifactId>,
}

fn is_mp4(a: &ArtifactRecord) -> bool {
    a.content_type.eq_ignore_ascii_case("video/mp4") || a.name.to_ascii_lowercase().ends_with(".mp4")
}

fn set_video_asset(detail: &mut NoticeDetail, asset: Url) {
    match detail {
        NoticeDetail::DiagnosisVerdict { video_asset, .. } | NoticeDetail::FixReady { video_asset, .. } | NoticeDetail::FixFailed { video_asset, .. } => {
            *video_asset = Some(asset)
        }
        NoticeDetail::TriageReady { .. } | NoticeDetail::FixBriefReady { .. } | NoticeDetail::PrOpened { .. } => {}
    }
}

#[cfg(test)]
mod tests;
