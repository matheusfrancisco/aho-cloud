//! Test fixtures and the in-memory world for the triage use cases.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use time::macros::datetime;
use url::Url;
use uuid::Uuid;

use mage_core::domain::evidence::ManifestView;
use mage_core::domain::{
    Budget, BlobOid, Citation, CommitSha, ContentHash, CustomerId, EntryKind, FileClass, LayerEvent, LineRange, ManifestEntry, OrgId,
    RepoId, RepoPath, RepoVersionKey, ReproMethod, RunId, TargetOrigin, Ticket, TicketContent, TicketData, TicketId, TicketSource,
    TicketStatus, TicketTarget, TriageTrigger, VersionReason,
};
use mage_core::domain::{Layer, Repo, RepoAccess};
use mage_core::ports::blob::{ArtifactStore, BlobKey, PutOptions};
use mage_core::ports::knowledge::{BlobMeta, SymbolKind};
use mage_core::ports::{KnowledgeStore, RepoStore};
use mage_core::testing::{MemArtifactStore, MemKnowledgeStore, MemRepoStore};
use mage_core::wire::runner::{CauseDraftDto, CitationDraftDto, RepoRef, TriageSubmission};

use crate::app::knowledge_query::SourceReader;
use crate::app::triage::prompt::TRIAGE_PROMPT_VERSION;
use crate::app::triage::view::load_manifest_view;
use crate::domain::knowledge::{Candidate, CandidateSet, SimilarMatch, SimilarResolved, Signal, Stage, SymbolSummary};
use crate::domain::triage::{CustomerRef, PriorHypothesis, TriageInput, TriageMode, TriageSpec};

pub(crate) fn uuid(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

pub(crate) fn org() -> OrgId {
    OrgId::from_uuid(uuid(0xA1))
}

pub(crate) fn repo_id() -> RepoId {
    RepoId::from_uuid(uuid(0xB2))
}

pub(crate) fn ticket_id() -> TicketId {
    TicketId::from_uuid(uuid(0xC3))
}

pub(crate) fn sha() -> CommitSha {
    let mut b = [0x11; 20];
    b[..4].copy_from_slice(&[0x3f, 0x2a, 0x1c, 0x9e]);
    CommitSha::from_bytes(b)
}

pub(crate) fn prior() -> CommitSha {
    CommitSha::from_bytes([0x22; 20])
}

pub(crate) fn key() -> RepoVersionKey {
    RepoVersionKey::new(repo_id(), sha())
}

pub(crate) fn hash(n: u8) -> ContentHash {
    ContentHash::from_bytes([n; 32])
}

pub(crate) fn t0() -> OffsetDateTime {
    datetime!(2026-10-05 12:00 UTC)
}

pub(crate) fn path(p: &str) -> RepoPath {
    RepoPath::new(p).unwrap()
}

pub(crate) fn target() -> TicketTarget {
    TicketTarget { repo_id: repo_id(), sha: sha(), origin: TargetOrigin::Deployment { environment: "production".into() } }
}

/// A ticket input exercising every prompt section.
pub(crate) fn rich_input() -> TriageInput {
    let citation = |p: &str, s: u32, e: u32| Citation {
        repo_id: repo_id(),
        sha: sha(),
        path: path(p),
        lines: LineRange::new(s, e).unwrap(),
        snippet_hash: None,
        note: None,
    };
    TriageInput {
        content_hash: hash(1),
        identifier: "SUP-123".into(),
        title: "Invoice export crashes for customers without a VAT id".into(),
        body: "Exporting invoices fails since Tuesday.\n\n> TypeError: Cannot read properties of null (reading 'vatId')\n\nComment (ana): same for Acme."
            .into(),
        error_strings: vec!["TypeError: Cannot read properties of null (reading 'vatId')".into(), "export failed: 500".into()],
        target: target(),
        repo: RepoRef { repo_id: repo_id(), owner: "acme".into(), name: "billing".into() },
        customer: Some(CustomerRef { name: "Acme".into(), environment: Some("production".into()) }),
        prior_sha: Some(prior()),
        prior_hypotheses: vec![PriorHypothesis {
            title: "Stale tax table cache".into(),
            verdict: "not reproduced".into(),
            summary: "The cache is rebuilt on deploy.".into(),
        }],
        seeds: Some(CandidateSet {
            version: key(),
            candidates: vec![
                Candidate {
                    citation: citation("src/billing/export.ts", 110, 140),
                    symbol: Some(SymbolSummary { name: "exportInvoices".into(), kind: SymbolKind::Function, signature: None }),
                    score: 1.75,
                    signals: vec![
                        Signal { stage: Stage::ErrorMatch, detail: "vatId".into(), contribution: 1.0 },
                        Signal { stage: Stage::History, detail: "changed since prior".into(), contribution: 0.75 },
                        Signal { stage: Stage::ErrorMatch, detail: "export failed".into(), contribution: 0.5 },
                    ],
                },
                Candidate { citation: citation("src/billing/totals.ts", 80, 100), symbol: None, score: 0.4, signals: vec![] },
            ],
            similar: vec![SimilarResolved {
                ticket: TicketId::from_uuid(uuid(0xD4)),
                identifier: "SUP-77".into(),
                title: "PDF export fails for EU customers".into(),
                url: "https://linear.app/acme/issue/SUP-77".into(),
                resolved_at: datetime!(2026-08-01 09:00 UTC),
                root_cause: "Missing null check on customer.vat".into(),
                cause_citations: vec![],
                fix_pr_url: None,
                score: 0.6,
                matched_on: vec![SimilarMatch::Text],
            }],
            stages: vec![],
        }),
        snapshot_at: t0(),
    }
}

pub(crate) fn spec_for(input: TriageInput) -> TriageSpec {
    TriageSpec {
        mode: TriageMode::Live,
        trigger: TriageTrigger::Auto,
        input,
        prompt_version: TRIAGE_PROMPT_VERSION,
        max_causes: 3,
        degraded_layers: vec![Layer::Semantic],
    }
}

pub(crate) fn budget(gate_attempts: u8) -> Budget {
    Budget { max_wall: Duration::from_secs(480), max_turns: 40, max_cost_micros: 2_000_000, max_gate_attempts: gate_attempts }
}

// ---- submissions ----

pub(crate) fn cit(p: &str, start: u32, end: u32, quote: Option<&str>) -> CitationDraftDto {
    CitationDraftDto { path: p.into(), start_line: start, end_line: end, quote: quote.map(str::to_owned), note: Some("why it matters".into()) }
}

pub(crate) fn cause(title: &str, confidence: f64, evidence: Vec<CitationDraftDto>) -> CauseDraftDto {
    CauseDraftDto {
        title: title.into(),
        explanation: format!("{title}: explanation"),
        evidence,
        repro_steps: vec!["open the invoices page".into()],
        confirm_if: "export returns 500".into(),
        rule_out_if: "export succeeds".into(),
        confidence,
        method: ReproMethod::Ui,
    }
}

pub(crate) fn submission(causes: Vec<CauseDraftDto>) -> TriageSubmission {
    TriageSubmission { summary: "Export crashes on a null VAT id.".into(), causes, missing_info: vec![] }
}

// ---- source fixture ----

/// Text of `src/billing/export.ts` (60 lines, distinct lines).
pub(crate) fn export_ts() -> String {
    let mut lines: Vec<String> = (1..=60).map(|i| format!("const filler{i} = computeFiller({i});")).collect();
    lines[29] = "  const vat = customer.vatId.toUpperCase(); // crashes when customer is null".into();
    lines[30] = "  return renderInvoiceRow(invoice, vat);".into();
    lines.join("\n") + "\n"
}

/// Text of `src/billing/totals.ts` (40 lines).
pub(crate) fn totals_ts() -> String {
    let mut lines: Vec<String> = (1..=40).map(|i| format!("let total{i} = round(sum{i} * rate{i});")).collect();
    lines[9] = "  const tax = Math.round(net * taxRate * 100) / 100;".into();
    lines.join("\n") + "\n"
}

/// Knowledge store + blob store + `SourceReader` holding a manifest at [`key`].
pub(crate) struct SourceFixture {
    pub knowledge: Arc<MemKnowledgeStore>,
    pub artifacts: Arc<MemArtifactStore>,
    pub reader: Arc<SourceReader>,
    entries: Vec<ManifestEntry>,
    next_blob: u8,
}

impl SourceFixture {
    pub(crate) fn new(clock: Arc<dyn mage_core::ports::Clock>, repos: Arc<MemRepoStore>) -> Self {
        let knowledge = Arc::new(MemKnowledgeStore::new(clock.clone(), repos.clone()));
        let artifacts = Arc::new(MemArtifactStore::new(clock));
        let reader = Arc::new(SourceReader::new(knowledge.clone(), artifacts.clone()));
        Self { knowledge, artifacts, reader, entries: Vec::new(), next_blob: 1 }
    }

    /// The default billing repo: two text files, a symlink and a binary.
    pub(crate) async fn billing(clock: Arc<dyn mage_core::ports::Clock>, repos: Arc<MemRepoStore>) -> Self {
        let mut f = Self::new(clock, repos);
        f.add_text("src/billing/export.ts", &export_ts()).await;
        f.add_text("app/billing/export_helpers.ts", "export const helper = () => null;\n").await;
        f.add_text("src/billing/totals.ts", &totals_ts()).await;
        f.add_entry("src/billing/link.ts", EntryKind::Symlink, false, 0);
        f.add_entry("assets/logo.png", EntryKind::File, false, 0);
        f.publish().await;
        f
    }

    fn blob(&mut self) -> BlobOid {
        self.next_blob += 1;
        BlobOid::from_bytes([self.next_blob; 20])
    }

    fn add_entry(&mut self, p: &str, kind: EntryKind, is_text: bool, line_count: u32) -> BlobOid {
        let blob = self.blob();
        self.entries.push(ManifestEntry {
            path: path(p),
            kind,
            executable: false,
            blob,
            lang: None,
            class: FileClass::Source,
            size: 100,
            is_text,
            line_count,
        });
        blob
    }

    pub(crate) async fn add_text(&mut self, p: &str, text: &str) {
        let lines = u32::try_from(text.lines().count()).unwrap();
        let blob = self.add_entry(p, EntryKind::File, true, lines);
        let opts = PutOptions { content_type: "text/plain".into(), if_absent: true };
        self.artifacts.put(&BlobKey::blob_content(org(), blob), text.as_bytes().to_vec(), &opts).await.unwrap();
    }

    /// Writes the manifest and the blob rows its entries describe (the store derives `is_text`
    /// and `line_count` from the blob rows).
    pub(crate) async fn publish(&self) {
        let metas: Vec<BlobMeta> = self
            .entries
            .iter()
            .map(|e| BlobMeta { oid: e.blob, size: e.size, line_count: e.line_count, is_text: e.is_text, content_stored: e.is_text })
            .collect();
        self.knowledge.put_blob_meta(org(), &metas).await.unwrap();
        self.knowledge.put_manifest(org(), &key(), &self.entries).await.unwrap();
    }

    pub(crate) async fn view(&self, paths: &[&str]) -> ManifestView {
        let paths: Vec<RepoPath> = paths.iter().map(|p| path(p)).collect();
        load_manifest_view(self.knowledge.as_ref(), &self.reader, org(), key(), &paths).await.unwrap()
    }
}

/// Indexes [`key`] with the given layers ready (others stay pending) and registers the repo.
pub(crate) async fn index_version(repos: &MemRepoStore, ready: &[Layer], at: OffsetDateTime) {
    let repo = Repo {
        id: repo_id(),
        org_id: org(),
        external_id: 42,
        owner: "acme".into(),
        name: "billing".into(),
        default_branch: "main".into(),
        installation_id: mage_core::domain::InstallationId(7),
        access: RepoAccess::Granted,
        created_at: at,
    };
    repos.upsert_repo(&repo, &[]).await.unwrap();
    repos.add_version_reason(org(), repo_id(), sha(), VersionReason::DefaultHead).await.unwrap();
    for layer in ready {
        repos.apply_layer_event(org(), key(), *layer, LayerEvent::Start, &[]).await.unwrap();
        repos.apply_layer_event(org(), key(), *layer, LayerEvent::Complete { at }, &[]).await.unwrap();
    }
}

/// A Linear ticket in `status` with the given triage fields.
pub(crate) fn ticket(status: TicketStatus, content: ContentHash, triaged: Option<ContentHash>, fence: Option<RunId>) -> Ticket {
    let base = Ticket::new(
        ticket_id(),
        org(),
        TicketSource::Linear { issue_id: "iss".into(), identifier: "SUP-123".into(), url: Url::parse("https://linear.app/acme/issue/SUP-123").unwrap() },
        TicketContent {
            title: "Invoice export crashes".into(),
            body: "TypeError: Cannot read properties of null (reading 'vatId')".into(),
            content_hash: content,
            slack_thread: None,
            customer: None::<CustomerId>,
            target: Some(target()),
            error_strings: vec!["Cannot read properties of null (reading 'vatId')".into()],
        },
        t0(),
    );
    let data = TicketData { status, triaged_hash: triaged, triage_run: fence, ..base.into_data() };
    Ticket::rehydrate(data)
}

pub(crate) fn paths(ps: &[&str]) -> BTreeSet<RepoPath> {
    ps.iter().map(|p| path(p)).collect()
}

// ---- the use-case world ----

/// Run manager over fakes (`RunsHarness`), the billing source fixture, an eval store, and a
/// retriever, all sharing one clock, job queue and set of stores.
pub(crate) struct World {
    pub h: crate::testing::RunsHarness,
    pub source: SourceFixture,
    pub evals: Arc<crate::testing::MemTriageEvalStore>,
    pub retriever: Arc<crate::app::knowledge_query::Retriever>,
    pub cfg: crate::app::triage::TriageConfig,
}

impl World {
    /// Clock at [`t0`], [`key`] indexed with every layer ready, the billing files stored.
    pub(crate) async fn new() -> Self {
        Self::with_layers(&Layer::ALL).await
    }

    pub(crate) async fn with_layers(ready: &[Layer]) -> Self {
        let h = crate::testing::RunsHarness::new();
        h.clock.set(t0());
        let clock: Arc<dyn mage_core::ports::Clock> = h.clock.clone();
        index_version(&h.repos, ready, t0()).await;
        let source = SourceFixture::billing(clock.clone(), h.repos.clone()).await;
        let ids: Arc<dyn mage_core::ports::IdGen> = h.ids.clone();
        let evals = Arc::new(crate::testing::MemTriageEvalStore::new(clock.clone(), ids));
        let retriever = Arc::new(crate::app::knowledge_query::Retriever::new(crate::app::knowledge_query::RetrieverDeps {
            knowledge: source.knowledge.clone(),
            repos: h.repos.clone(),
            tickets: h.tickets.clone(),
            indexer: Arc::new(crate::testing::FakeIndexerLink::new()),
            weights: crate::app::knowledge_query::RetrievalWeights::default(),
        }));
        Self { h, source, evals, retriever, cfg: crate::app::triage::TriageConfig::new("model-default") }
    }

    pub(crate) fn start(&self) -> crate::app::triage::StartTriage {
        crate::app::triage::StartTriage::new(
            crate::app::triage::StartTriageDeps {
                tickets: self.h.tickets.clone(),
                runs: self.h.runs.clone(),
                run_manager: self.h.manager.clone(),
                repos: self.h.repos.clone(),
                orgs: self.h.orgs.clone(),
                retriever: self.retriever.clone(),
                jobs: self.h.jobs.clone(),
                clock: self.h.clock.clone(),
                ids: self.h.ids.clone(),
            },
            self.cfg.clone(),
        )
    }

    pub(crate) fn apply(&self) -> crate::app::triage::ApplyTriageOutcome {
        crate::app::triage::ApplyTriageOutcome::new(
            crate::app::triage::ApplyTriageDeps {
                tickets: self.h.tickets.clone(),
                runs: self.h.runs.clone(),
                knowledge: self.source.knowledge.clone(),
                reader: self.source.reader.clone(),
                evals: self.evals.clone(),
                jobs: self.h.jobs.clone(),
                clock: self.h.clock.clone(),
                ids: self.h.ids.clone(),
            },
            self.cfg.clone(),
        )
    }

    pub(crate) fn retriage(&self) -> crate::app::triage::RequestRetriage {
        crate::app::triage::RequestRetriage::new(
            self.h.tickets.clone(),
            self.h.runs.clone(),
            self.h.orgs.clone(),
            self.h.jobs.clone(),
            self.h.clock.clone(),
            self.cfg.clone(),
        )
    }

    pub(crate) fn eval_case(&self) -> crate::app::triage::StartEvalCase {
        crate::app::triage::StartEvalCase::new(
            crate::app::triage::StartEvalCaseDeps {
                tickets: self.h.tickets.clone(),
                runs: self.h.runs.clone(),
                run_manager: self.h.manager.clone(),
                repos: self.h.repos.clone(),
                orgs: self.h.orgs.clone(),
                retriever: self.retriever.clone(),
                evals: self.evals.clone(),
                jobs: self.h.jobs.clone(),
                clock: self.h.clock.clone(),
                ids: self.h.ids.clone(),
            },
            self.cfg.clone(),
        )
    }

    pub(crate) fn now(&self) -> OffsetDateTime {
        mage_core::ports::Clock::now(self.h.clock.as_ref())
    }

    pub(crate) async fn put_ticket(&self, t: &Ticket) {
        use crate::ports::ticket::TicketStore;
        match self.h.tickets.get_ticket(org(), t.id).await {
            Ok(current) => self.h.tickets.save_ingested(org(), t, Some(current.rev), &[]).await.unwrap(),
            Err(_) => self.h.tickets.save_ingested(org(), t, None, &[]).await.unwrap(),
        };
    }

    pub(crate) async fn ticket(&self) -> Ticket {
        use crate::ports::ticket::TicketStore;
        self.h.tickets.get_ticket(org(), ticket_id()).await.unwrap().value
    }

    pub(crate) async fn set_settings(&self, edit: impl FnOnce(&mut mage_core::wire::api::TriageSettings)) {
        use crate::ports::org::OrgStore;
        let mut s = self.h.orgs.triage_settings(org()).await.unwrap();
        edit(&mut s);
        self.h.orgs.set_triage_settings(org(), &s).await.unwrap();
    }

    /// A `triage.ticket` lease enqueued now.
    pub(crate) fn job(&self, trigger: TriageTrigger, content: ContentHash, attempt: u16, max_attempts: u16) -> mage_core::ports::jobs::LeasedJob {
        let n = mage_core::ports::IdGen::uuid_v7(self.h.ids.as_ref());
        mage_core::ports::jobs::LeasedJob {
            lease: mage_core::ports::jobs::JobLease { job_id: mage_core::domain::JobId::from_uuid(n), lease_seq: 1 },
            kind: mage_core::domain::JobKind::TriageTicket,
            org_id: Some(org()),
            payload: mage_core::domain::JobPayload::TriageTicket { org_id: org(), ticket_id: ticket_id(), content_hash: content, trigger },
            attempt,
            max_attempts,
            leased_until: self.now() + time::Duration::minutes(1),
            enqueued_at: self.now(),
        }
    }

    pub(crate) fn payload(job: &mage_core::ports::jobs::LeasedJob) -> crate::app::triage::start::TriageTicketPayload {
        crate::app::triage::start::TriageTicketPayload::from_payload(&job.payload).unwrap()
    }

    /// A live Triage run of the ticket for `content`, created now through the run manager.
    /// The ticket is inserted (`New`, content `content`) first when it does not exist yet.
    pub(crate) async fn seed_run(&self, content: ContentHash, trigger: TriageTrigger) -> crate::ports::runs::RunRecord {
        {
            use crate::ports::ticket::TicketStore;
            if self.h.tickets.get_ticket(org(), ticket_id()).await.is_err() {
                self.put_ticket(&ticket(TicketStatus::New, content, None, None)).await;
            }
        }
        let mut input = rich_input();
        input.content_hash = content;
        let mut spec = spec_for(input);
        spec.trigger = trigger;
        let mut create = crate::app::runs::CreateRun::new(org(), mage_core::domain::RunKind::Triage, mage_core::domain::RunSubject::Ticket(ticket_id()));
        create.prompt = Some(crate::app::runs::RunPrompt { prompt: "p".into(), system_append: "s".into() });
        create.triage = Some(spec);
        self.h.manager.create_run(create).await.unwrap()
    }

    pub(crate) async fn record(&self, run: RunId) -> crate::ports::runs::RunRecord {
        use crate::ports::runs::RunStore;
        self.h.runs.get(org(), run).await.unwrap()
    }

    /// Live triage runs of the ticket, newest first.
    pub(crate) async fn live_runs(&self) -> Vec<crate::ports::runs::RunRecord> {
        crate::app::triage::start::live_triage_runs(self.h.runs.as_ref(), org(), ticket_id()).await.unwrap()
    }

    /// Drives a run through `events` (with `result` / `error` set first) and commits it.
    pub(crate) async fn drive(
        &self,
        run: RunId,
        events: &[mage_core::domain::RunStatusEvent],
        result: Option<mage_core::domain::TriageResult>,
        error: Option<mage_core::domain::RunError>,
    ) {
        use crate::ports::runs::{RunStore, RunUpdate, RunWrite};
        let mut rec = self.record(run).await;
        if let Some(r) = result {
            rec.run.set_result(mage_core::domain::RunResult::Triage(r)).unwrap();
        }
        if let Some(e) = error {
            rec.run.set_error(e);
        }
        for e in events {
            rec.run.apply(*e, self.now()).unwrap();
        }
        self.h.runs.commit(org(), RunWrite { updates: vec![RunUpdate::of(rec)], ..RunWrite::default() }).await.unwrap();
    }

    /// `Queued` → `Running`.
    pub(crate) async fn start_running(&self, run: RunId) {
        use mage_core::domain::RunStatusEvent as E;
        let runner = mage_core::domain::RunnerLease { runner_id: mage_core::domain::RunnerId::from_uuid(uuid(0x77)), until: self.now() };
        self.drive(run, &[E::Lease { runner }, E::EnvPreparing, E::Started], None, None).await;
    }

    /// Sets the metered and reported cost of a run.
    pub(crate) async fn set_cost(&self, run: RunId, cost_micros: u64) {
        use crate::ports::runs::{RunStore, RunUpdate, RunWrite};
        let mut rec = self.record(run).await;
        rec.usage_metered.cost_micros = cost_micros;
        rec.usage_reported.cost_micros = cost_micros;
        self.h.runs.commit(org(), RunWrite { updates: vec![RunUpdate::of(rec)], ..RunWrite::default() }).await.unwrap();
    }

    /// Active (`Queued` / `Running`) jobs of `kind`, oldest first.
    pub(crate) fn queued(&self, kind: mage_core::domain::JobKind) -> Vec<mage_core::ports::jobs::JobRecord> {
        self.h.jobs.jobs().into_iter().filter(|j| j.kind == kind && j.state.is_active()).collect()
    }
}
