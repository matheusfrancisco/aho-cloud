//! Rows a contract scenario needs that its port does not write (orgs, users, repos, runs, gates,
//! artifacts): the Postgres schema enforces those foreign keys, the in-memory fakes do not.
//! Suites call the seeder before writing rows that reference them; [`NoSeed`] serves the fakes,
//! `testing::pg::PgSeed` real databases. Seeding is idempotent and panics on failure (test code).

use async_trait::async_trait;
use mage_core::domain::{ArtifactId, CauseId, CommitSha, CustomerId, GateId, OrgId, RepoId, RunId, TicketId, UserId};

/// What a seeded run is about (`runs` CHECK per kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedRun {
    Triage { ticket: TicketId },
    Diagnose { cause: CauseId, repo: RepoId, sha: CommitSha },
    Fix { cause: CauseId, repo: RepoId, sha: CommitSha },
}

#[async_trait]
pub trait Seed: Send + Sync {
    async fn org(&self, org: OrgId);
    async fn user(&self, user: UserId);
    /// Needs the org.
    async fn repo(&self, org: OrgId, repo: RepoId);
    /// A minimal `New` Linear ticket (issue id = the ticket uuid) for ports that only reference
    /// tickets (outbound log, triage evals). Needs the org.
    async fn ticket(&self, org: OrgId, ticket: TicketId);
    /// A customer without refs. Needs the org.
    async fn customer(&self, org: OrgId, customer: CustomerId);
    /// A finished run (terminal, so it never blocks "one active run per subject"). Needs the
    /// ticket (Triage) or cause and repo (Diagnose, Fix).
    async fn run(&self, org: OrgId, run: RunId, subject: SeedRun);
    /// A passed repro gate of `run`.
    async fn gate(&self, org: OrgId, gate: GateId, run: RunId);
    /// A `fix_brief` artifact of `run`.
    async fn artifact(&self, org: OrgId, artifact: ArtifactId, run: RunId);
}

/// Seeder for in-memory fakes: nothing to insert.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSeed;

#[async_trait]
impl Seed for NoSeed {
    async fn org(&self, _org: OrgId) {}
    async fn user(&self, _user: UserId) {}
    async fn repo(&self, _org: OrgId, _repo: RepoId) {}
    async fn ticket(&self, _org: OrgId, _ticket: TicketId) {}
    async fn customer(&self, _org: OrgId, _customer: CustomerId) {}
    async fn run(&self, _org: OrgId, _run: RunId, _subject: SeedRun) {}
    async fn gate(&self, _org: OrgId, _gate: GateId, _run: RunId) {}
    async fn artifact(&self, _org: OrgId, _artifact: ArtifactId, _run: RunId) {}
}
