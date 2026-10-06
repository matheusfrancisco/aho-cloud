//! Retrieval quality replay (knowledge.md "Quality metrics"; `mage knowledge eval`). Read-only:
//! for each resolved ticket with a target sha, replay `triage_candidates` at that sha with the
//! memory cutoff `{exclude: [ticket], as_of: ticket.created_at}` and score the ranked files
//! against the ground truth G = files cited by the confirmed cause ∪ files changed by commits
//! linked to the ticket (or its fix PR), restricted to paths existing at the target sha. Also
//! measures how often `lookup_error` verifies the ticket's error strings.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use mage_core::domain::{CommitSha, Layer, OrgId, RepoId, RepoPath, RepoVersionKey, RunId, Ticket, TicketId, TicketSource, TicketStatus};
use mage_core::ports::common::PageRequest;
use mage_core::ports::knowledge::{KnowledgeStore, LinkLookup};
use mage_core::ports::repos::RepoStore;
use mage_core::wire::token::MemoryCutoff;
use serde::Serialize;
use uuid::Uuid;

use super::errors::prepare;
use super::retrieval::{RetrievalLimits, Retriever};
use super::scope::KnowledgeScope;
use crate::domain::knowledge::{CandidateSet, TicketContext};
use crate::error::AppError;
use crate::ports::ticket::{TicketFilter, TicketStore};

/// Distinct candidate files in rank order.
pub fn ranked_files(set: &CandidateSet) -> Vec<RepoPath> {
    let mut seen = BTreeSet::new();
    set.candidates.iter().map(|c| c.citation.path.clone()).filter(|p| seen.insert(p.clone())).collect()
}

/// `(hit, coverage)` at `k`: whether the top-k files hold ≥ 1 truth file, and |top-k ∩ G| / |G|.
pub fn recall_at(ranked: &[RepoPath], truth: &BTreeSet<RepoPath>, k: usize) -> (bool, f64) {
    if truth.is_empty() {
        return (false, 0.0);
    }
    let found = ranked.iter().take(k).filter(|p| truth.contains(*p)).count();
    (found > 0, found as f64 / truth.len() as f64)
}

/// Pull request number of a GitHub PR URL (`…/pull/123`).
pub fn pr_number(url: &str) -> Option<u32> {
    let (_, rest) = url.split_once("/pull/")?;
    rest.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

#[derive(Debug, Clone)]
pub struct EvalOptions {
    pub repo: Option<RepoId>,
    pub ks: Vec<usize>,
    /// Resolved tickets replayed at most.
    pub max_tickets: usize,
    /// Layer ablation (`KnowledgeScope.disabled_layers`).
    pub disabled_layers: Vec<Layer>,
    pub limits: RetrievalLimits,
}

impl Default for EvalOptions {
    fn default() -> Self {
        EvalOptions { repo: None, ks: vec![1, 5, 10, 30], max_tickets: 500, disabled_layers: Vec::new(), limits: RetrievalLimits::default() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CaseResult {
    pub ticket: TicketId,
    pub identifier: String,
    pub repo: RepoId,
    pub sha: CommitSha,
    pub truth: Vec<RepoPath>,
    /// 1-based rank of the first truth file among candidate files.
    pub first_hit_rank: Option<usize>,
    pub candidates: usize,
    pub error_strings: u32,
    pub error_strings_verified: u32,
    pub verified_in_truth: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SkippedCase {
    pub ticket: TicketId,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LookupErrorMetrics {
    pub error_strings: u32,
    pub with_verified_match: u32,
    pub hit_rate: f64,
    /// Error strings whose verified match lies in G, over those with a verified match.
    pub precision_proxy: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EvalReport {
    pub org: OrgId,
    pub repo: Option<RepoId>,
    pub cases: usize,
    /// `k` → fraction of cases with ≥ 1 truth file in the top k.
    pub recall_at_k: BTreeMap<usize, f64>,
    /// `k` → mean |top-k ∩ G| / |G|.
    pub mean_coverage_at_k: BTreeMap<usize, f64>,
    pub lookup_error: LookupErrorMetrics,
    pub skipped: Vec<SkippedCase>,
    pub per_case: Vec<CaseResult>,
}

/// Aggregates case results into the report metrics.
pub fn summarize(org: OrgId, repo: Option<RepoId>, ks: &[usize], cases: Vec<(CaseResult, Vec<RepoPath>)>, skipped: Vec<SkippedCase>) -> EvalReport {
    let n = cases.len();
    let mut recall_at_k = BTreeMap::new();
    let mut mean_coverage_at_k = BTreeMap::new();
    for &k in ks {
        let (mut hits, mut coverage) = (0usize, 0.0f64);
        for (case, ranked) in &cases {
            let truth: BTreeSet<RepoPath> = case.truth.iter().cloned().collect();
            let (hit, cov) = recall_at(ranked, &truth, k);
            hits += usize::from(hit);
            coverage += cov;
        }
        recall_at_k.insert(k, if n == 0 { 0.0 } else { hits as f64 / n as f64 });
        mean_coverage_at_k.insert(k, if n == 0 { 0.0 } else { coverage / n as f64 });
    }
    let strings: u32 = cases.iter().map(|(c, _)| c.error_strings).sum();
    let verified: u32 = cases.iter().map(|(c, _)| c.error_strings_verified).sum();
    let in_truth: u32 = cases.iter().map(|(c, _)| c.verified_in_truth).sum();
    EvalReport {
        org,
        repo,
        cases: n,
        recall_at_k,
        mean_coverage_at_k,
        lookup_error: LookupErrorMetrics {
            error_strings: strings,
            with_verified_match: verified,
            hit_rate: if strings == 0 { 0.0 } else { f64::from(verified) / f64::from(strings) },
            precision_proxy: if verified == 0 { 0.0 } else { f64::from(in_truth) / f64::from(verified) },
        },
        skipped,
        per_case: cases.into_iter().map(|(c, _)| c).collect(),
    }
}

pub struct KnowledgeEval {
    retriever: Arc<Retriever>,
    knowledge: Arc<dyn KnowledgeStore>,
    repos: Arc<dyn RepoStore>,
    tickets: Arc<dyn TicketStore>,
}

impl KnowledgeEval {
    pub fn new(retriever: Arc<Retriever>, knowledge: Arc<dyn KnowledgeStore>, repos: Arc<dyn RepoStore>, tickets: Arc<dyn TicketStore>) -> Self {
        KnowledgeEval { retriever, knowledge, repos, tickets }
    }

    pub async fn run(&self, org: OrgId, opts: &EvalOptions) -> Result<EvalReport, AppError> {
        let filter = TicketFilter { status: Some(vec![TicketStatus::Resolved]), customer: None, repo: opts.repo, text: None };
        let mut tickets: Vec<Ticket> = Vec::new();
        let mut page = PageRequest::first(PageRequest::MAX_LIMIT);
        loop {
            let got = self.tickets.list_tickets(org, &filter, page.clone()).await?;
            tickets.extend(got.items);
            match got.next {
                Some(next) if tickets.len() < opts.max_tickets => page = page.after(next),
                _ => break,
            }
        }
        tickets.truncate(opts.max_tickets);
        let mut cases = Vec::new();
        let mut skipped = Vec::new();
        for ticket in tickets {
            match self.case(org, &ticket, opts).await {
                Ok(case) => cases.push(case),
                Err(reason) => skipped.push(SkippedCase { ticket: ticket.id, reason }),
            }
        }
        Ok(summarize(org, opts.repo, &opts.ks, cases, skipped))
    }

    async fn case(&self, org: OrgId, ticket: &Ticket, opts: &EvalOptions) -> Result<(CaseResult, Vec<RepoPath>), String> {
        let Some(target) = ticket.target.clone() else { return Err("no target sha".into()) };
        let key = RepoVersionKey::new(target.repo_id, target.sha);
        let version = self.repos.version(org, key).await.map_err(|e| format!("target version: {e}"))?;
        let resolution = self.tickets.resolution(org, ticket.id).await.map_err(|e| format!("resolution: {e}"))?.ok_or("no resolution row")?;
        let identifier = match &ticket.source {
            TicketSource::Linear { identifier, .. } => identifier.clone(),
        };

        let mut candidates: BTreeSet<RepoPath> = BTreeSet::new();
        if let Some(cause) = resolution.confirmed_cause {
            let cause = self.tickets.get_cause(org, cause).await.map_err(|e| format!("cause: {e}"))?;
            candidates.extend(cause.evidence.iter().filter(|c| c.repo_id == target.repo_id).map(|c| c.path.clone()));
        }
        let links = self.knowledge.ticket_links(org, target.repo_id, &LinkLookup::Identifier(identifier.clone())).await.map_err(|e| e.to_string())?;
        let mut shas: BTreeSet<CommitSha> = links.iter().filter_map(|l| l.commit).collect();
        if let Some(number) = resolution.fix_pr_url.as_ref().and_then(|u| pr_number(u.as_str())) {
            let pr_links = self.knowledge.ticket_links(org, target.repo_id, &LinkLookup::Prs(vec![number])).await.map_err(|e| e.to_string())?;
            shas.extend(pr_links.iter().filter_map(|l| l.commit));
        }
        if !shas.is_empty() {
            let commits = self.knowledge.commits(org, target.repo_id, &shas.into_iter().collect::<Vec<_>>()).await.map_err(|e| e.to_string())?;
            for commit in commits {
                candidates.extend(commit.info.files.unwrap_or_default().into_iter().map(|f| f.path));
            }
        }
        let paths: Vec<RepoPath> = candidates.into_iter().collect();
        let truth: BTreeSet<RepoPath> = if paths.is_empty() {
            BTreeSet::new()
        } else {
            self.knowledge.manifest_entries(org, &key, &paths).await.map_err(|e| e.to_string())?.into_iter().map(|e| e.path).collect()
        };
        if truth.is_empty() {
            return Err("no ground-truth file exists at the target sha".into());
        }

        let scope = KnowledgeScope {
            org,
            run: RunId::from_uuid(Uuid::nil()),
            repo: target.repo_id,
            sha: target.sha,
            memory: MemoryCutoff { exclude_tickets: vec![ticket.id], as_of: Some(ticket.created_at) },
            disabled_layers: opts.disabled_layers.clone(),
        };
        let ctx = TicketContext { title: ticket.title.clone(), body: ticket.body.clone(), error_strings: ticket.error_strings.clone(), last_good: None };
        let set = self.retriever.triage_candidates(&scope, &ctx, &opts.limits).await.map_err(|e| format!("retrieval: {e}"))?;
        let ranked = ranked_files(&set);
        let first_hit_rank = ranked.iter().position(|p| truth.contains(p)).map(|i| i + 1);

        let mut verified = 0;
        let mut in_truth = 0;
        let errors_ready = !opts.disabled_layers.contains(&Layer::ErrorSites) && version.layer(Layer::ErrorSites).is_some_and(|s| s.is_ready());
        if let (Some(revs), true) = (version.revs, errors_ready) {
            for s in &ticket.error_strings {
                let matches = self.knowledge.match_errors(org, &key, revs.errors, &prepare(s, 10, false)).await.map_err(|e| e.to_string())?;
                let hits: Vec<_> = matches.iter().filter(|m| m.verified).collect();
                if !hits.is_empty() {
                    verified += 1;
                    if hits.iter().any(|m| truth.contains(&m.path)) {
                        in_truth += 1;
                    }
                }
            }
        }
        let case = CaseResult {
            ticket: ticket.id,
            identifier,
            repo: target.repo_id,
            sha: target.sha,
            truth: truth.into_iter().collect(),
            first_hit_rank,
            candidates: set.candidates.len(),
            error_strings: u32::try_from(ticket.error_strings.len()).unwrap_or(u32::MAX),
            error_strings_verified: verified,
            verified_in_truth: in_truth,
        };
        Ok((case, ranked))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn recall_counts_hits_and_coverage_within_k() {
        let ranked = vec![p("a.go"), p("b.go"), p("c.go"), p("d.go")];
        let truth: BTreeSet<RepoPath> = [p("c.go"), p("z.go")].into();
        assert_eq!(recall_at(&ranked, &truth, 1), (false, 0.0));
        assert_eq!(recall_at(&ranked, &truth, 3), (true, 0.5));
        assert_eq!(recall_at(&ranked, &BTreeSet::new(), 3), (false, 0.0));
    }

    #[test]
    fn summary_averages_over_cases() {
        let org = OrgId::from_uuid(Uuid::from_u128(1));
        let case = |truth: &[&str], hits: u32, verified: u32| CaseResult {
            ticket: TicketId::from_uuid(Uuid::from_u128(u128::from(hits) + 10)),
            identifier: "ENG-1".into(),
            repo: RepoId::from_uuid(Uuid::from_u128(2)),
            sha: CommitSha::from_bytes([1; 20]),
            truth: truth.iter().map(|s| p(s)).collect(),
            first_hit_rank: None,
            candidates: 0,
            error_strings: 2,
            error_strings_verified: hits,
            verified_in_truth: verified,
        };
        let report = summarize(
            org,
            None,
            &[1, 5],
            vec![(case(&["a.go"], 2, 1), vec![p("a.go")]), (case(&["x.go"], 0, 0), vec![p("a.go"), p("x.go")])],
            vec![],
        );
        assert_eq!(report.recall_at_k[&1], 0.5);
        assert_eq!(report.recall_at_k[&5], 1.0);
        assert_eq!(report.lookup_error.hit_rate, 0.5);
        assert_eq!(report.lookup_error.precision_proxy, 0.5);
    }

    #[test]
    fn pr_numbers_from_urls() {
        assert_eq!(pr_number("https://github.com/acme/app/pull/123"), Some(123));
        assert_eq!(pr_number("https://github.com/acme/app/pull/77/files"), Some(77));
        assert_eq!(pr_number("https://github.com/acme/app"), None);
    }
}
