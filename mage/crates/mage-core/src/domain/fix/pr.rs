//! Fix pull request progress (fix.md Additions: `FixPullRequest`, `FixPrState`; table
//! `fix_pull_requests`) and `BranchName` (fix.md "Branch, base and rebase policy").

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::domain::error::DomainError;
use crate::domain::fix::change_set::ChangeSetDigest;
use crate::domain::gates::FixGatePassed;
use crate::domain::ids::{CommitSha, FixPullRequestId, GateId, OrgId, RunId, TicketId};

/// Longest branch name Mage creates.
pub const BRANCH_NAME_MAX: usize = 100;

/// Validated Mage branch: `mage/[a-z0-9._-]+`, at most 100 bytes, and a valid git ref name
/// (no `..`, no leading `.`, no trailing `.` or `.lock`).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(try_from = "String", into = "String")]
pub struct BranchName(String);

impl BranchName {
    pub fn new(name: impl Into<String>) -> Result<Self, DomainError> {
        let name = name.into();
        let invalid = |why: &str| DomainError::invalid(format!("branch name {name:?}: {why}"));
        if name.len() > BRANCH_NAME_MAX {
            return Err(invalid("longer than 100 bytes"));
        }
        let Some(rest) = name.strip_prefix("mage/") else {
            return Err(invalid("must start with `mage/`"));
        };
        if rest.is_empty() {
            return Err(invalid("empty after `mage/`"));
        }
        if !rest.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)) {
            return Err(invalid("only [a-z0-9._-] allowed after `mage/`"));
        }
        if rest.starts_with('.') || rest.ends_with('.') || rest.contains("..") || rest.ends_with(".lock") {
            return Err(invalid("not a valid git ref name"));
        }
        Ok(Self(name))
    }

    /// `mage/{identifier lowercased}-{first 7 hex of the digest}`; deterministic per change set.
    pub fn for_change_set(ticket_identifier: &str, digest: &ChangeSetDigest) -> Result<Self, DomainError> {
        let hex = digest.to_hex();
        Self::new(format!("mage/{}-{}", ticket_identifier.to_ascii_lowercase(), &hex[..7]))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for BranchName {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<BranchName> for String {
    fn from(value: BranchName) -> Self {
        value.0
    }
}

impl FromStr for BranchName {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl AsRef<str> for BranchName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BranchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for BranchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BranchName({:?})", self.0)
    }
}

/// Progress of `OpenPullRequest`; each step is persisted so a crash resumes from the last one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FixPrState {
    Planned,
    CommitCreated { commit: CommitSha },
    BranchCreated,
    Opened { number: u64 },
    Closed { merged: bool },
}

/// A completed `OpenPullRequest` step or a PR webhook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FixPrEvent {
    /// `create_commit` returned `commit`.
    CommitCreated { commit: CommitSha },
    /// `create_branch` succeeded (possibly on a `-N` suffixed name, possibly reusing an earlier
    /// commit of ours found at `head`).
    BranchCreated { branch: BranchName, head: CommitSha },
    /// `open` / `find_open` returned the PR.
    Opened { number: u64, url: Url },
    /// `pull_request` closed webhook.
    Closed { merged: bool },
}

impl FixPrEvent {
    pub fn name(&self) -> &'static str {
        match self {
            FixPrEvent::CommitCreated { .. } => "commit_created",
            FixPrEvent::BranchCreated { .. } => "branch_created",
            FixPrEvent::Opened { .. } => "opened",
            FixPrEvent::Closed { .. } => "closed",
        }
    }
}

impl FixPrState {
    pub fn name(&self) -> &'static str {
        match self {
            FixPrState::Planned => "planned",
            FixPrState::CommitCreated { .. } => "commit_created",
            FixPrState::BranchCreated => "branch_created",
            FixPrState::Opened { .. } => "opened",
            FixPrState::Closed { .. } => "closed",
        }
    }

    /// `Planned → CommitCreated → BranchCreated → Opened → Closed`; anything else is an
    /// `InvalidTransition`. `Closed` is terminal.
    pub fn apply(&self, event: &FixPrEvent) -> Result<FixPrState, DomainError> {
        match (self, event) {
            (FixPrState::Planned, FixPrEvent::CommitCreated { commit }) => Ok(FixPrState::CommitCreated { commit: *commit }),
            (FixPrState::CommitCreated { .. }, FixPrEvent::BranchCreated { .. }) => Ok(FixPrState::BranchCreated),
            (FixPrState::BranchCreated, FixPrEvent::Opened { number, .. }) => Ok(FixPrState::Opened { number: *number }),
            (FixPrState::Opened { .. }, FixPrEvent::Closed { merged }) => Ok(FixPrState::Closed { merged: *merged }),
            _ => Err(DomainError::InvalidTransition { from: self.name(), event: event.name() }),
        }
    }
}

/// One PR attempt per `(org_id, ticket_id, change_set)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct FixPullRequest {
    pub id: FixPullRequestId,
    pub org_id: OrgId,
    pub ticket_id: TicketId,
    pub fix_run: RunId,
    pub gate: GateId,
    pub change_set: ChangeSetDigest,
    pub branch: BranchName,
    pub base_sha: CommitSha,
    pub head_commit: Option<CommitSha>,
    pub number: Option<u64>,
    #[ts(type = "string | null")]
    pub url: Option<Url>,
    pub state: FixPrState,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub updated_at: OffsetDateTime,
}

impl FixPullRequest {
    /// A `Planned` row for the change set `gate` proved, on its deterministic branch.
    pub fn plan(
        id: FixPullRequestId,
        org_id: OrgId,
        ticket_id: TicketId,
        ticket_identifier: &str,
        gate: &FixGatePassed,
        at: OffsetDateTime,
    ) -> Result<FixPullRequest, DomainError> {
        let change_set = gate.digest();
        Ok(FixPullRequest {
            id,
            org_id,
            ticket_id,
            fix_run: gate.fix_run(),
            gate: gate.gate(),
            change_set,
            branch: BranchName::for_change_set(ticket_identifier, &change_set)?,
            base_sha: gate.base_sha(),
            head_commit: None,
            number: None,
            url: None,
            state: FixPrState::Planned,
            created_at: at,
            updated_at: at,
        })
    }

    /// Advance `state` and record what the step produced. Unchanged on error.
    pub fn apply(&mut self, event: FixPrEvent, at: OffsetDateTime) -> Result<(), DomainError> {
        let next = self.state.apply(&event)?;
        match event {
            FixPrEvent::CommitCreated { commit } => self.head_commit = Some(commit),
            FixPrEvent::BranchCreated { branch, head } => {
                self.branch = branch;
                self.head_commit = Some(head);
            }
            FixPrEvent::Opened { number, url } => {
                self.number = Some(number);
                self.url = Some(url);
            }
            FixPrEvent::Closed { .. } => {}
        }
        self.state = next;
        self.updated_at = at;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::fix::change_set::ChangeSet;
    use crate::domain::gates::testing::fix_gate_passed;
    use crate::domain::ids::{ArtifactId, CauseId};
    use uuid::Uuid;

    fn digest() -> ChangeSetDigest {
        ChangeSetDigest([0x3f; 32])
    }

    #[test]
    fn branch_name_validation() {
        let at_limit = format!("mage/{}", "a".repeat(BRANCH_NAME_MAX - 5));
        let over = format!("mage/{}", "a".repeat(BRANCH_NAME_MAX - 4));
        let cases: &[(&str, bool)] = &[
            ("mage/eng-412-3f9a2c1", true),
            ("mage/a", true),
            ("mage/a.b_c-d9", true),
            (&at_limit, true),
            (&over, false),
            ("", false),
            ("mage/", false),
            ("main", false),
            ("magex/a", false),
            ("Mage/a", false),
            ("mage/ENG-1", false),
            ("mage/a b", false),
            ("mage/a/b", false),
            ("mage/é", false),
            ("mage/.a", false),
            ("mage/a.", false),
            ("mage/a..b", false),
            ("mage/a.lock", false),
            ("refs/heads/mage/a", false),
        ];
        for &(name, ok) in cases {
            assert_eq!(BranchName::new(name).is_ok(), ok, "{name:?}");
        }
        assert!(serde_json::from_str::<BranchName>("\"main\"").is_err());
    }

    #[test]
    fn branch_for_change_set() {
        let branch = BranchName::for_change_set("ENG-412", &digest()).unwrap();
        assert_eq!(branch.as_str(), "mage/eng-412-3f3f3f3");
        assert_eq!(BranchName::for_change_set("ENG-412", &digest()).unwrap(), branch);
        let other = BranchName::for_change_set("ENG-412", &ChangeSetDigest([0xab; 32])).unwrap();
        assert_ne!(other, branch);
        assert!(BranchName::for_change_set("ENG 412", &digest()).is_err());
        assert!(BranchName::for_change_set(&"X".repeat(100), &digest()).is_err());
    }

    fn sha(n: u8) -> CommitSha {
        CommitSha([n; 20])
    }

    fn url() -> Url {
        Url::parse("https://github.com/acme/app/pull/7").unwrap()
    }

    #[test]
    fn state_transition_table() {
        let states = [
            FixPrState::Planned,
            FixPrState::CommitCreated { commit: sha(1) },
            FixPrState::BranchCreated,
            FixPrState::Opened { number: 7 },
            FixPrState::Closed { merged: false },
            FixPrState::Closed { merged: true },
        ];
        let events = [
            FixPrEvent::CommitCreated { commit: sha(1) },
            FixPrEvent::BranchCreated { branch: BranchName::new("mage/eng-1-abcdef0").unwrap(), head: sha(1) },
            FixPrEvent::Opened { number: 7, url: url() },
            FixPrEvent::Closed { merged: true },
        ];
        for state in &states {
            for event in &events {
                let want = match (state, event) {
                    (FixPrState::Planned, FixPrEvent::CommitCreated { commit }) => Some(FixPrState::CommitCreated { commit: *commit }),
                    (FixPrState::CommitCreated { .. }, FixPrEvent::BranchCreated { .. }) => Some(FixPrState::BranchCreated),
                    (FixPrState::BranchCreated, FixPrEvent::Opened { .. }) => Some(FixPrState::Opened { number: 7 }),
                    (FixPrState::Opened { .. }, FixPrEvent::Closed { .. }) => Some(FixPrState::Closed { merged: true }),
                    _ => None,
                };
                let got = state.apply(event);
                match want {
                    Some(next) => assert_eq!(got, Ok(next), "{state:?} + {event:?}"),
                    None => assert_eq!(
                        got,
                        Err(DomainError::InvalidTransition { from: state.name(), event: event.name() }),
                        "{state:?} + {event:?}"
                    ),
                }
            }
        }
    }

    #[test]
    fn plan_and_progress_record_each_step() {
        let change_set =
            ChangeSet { base_sha: sha(4), patch: ArtifactId::from_uuid(Uuid::from_u128(10)), patch_sha256: [7; 32], regression: None };
        let digest = change_set.digest();
        let gate = fix_gate_passed(
            GateId::from_uuid(Uuid::from_u128(1)),
            RunId::from_uuid(Uuid::from_u128(2)),
            CauseId::from_uuid(Uuid::from_u128(3)),
            change_set,
        );
        let t0 = OffsetDateTime::UNIX_EPOCH;
        let t1 = t0 + time::Duration::seconds(1);
        let mut pr = FixPullRequest::plan(
            FixPullRequestId::from_uuid(Uuid::from_u128(9)),
            OrgId::from_uuid(Uuid::from_u128(5)),
            TicketId::from_uuid(Uuid::from_u128(6)),
            "ENG-412",
            &gate,
            t0,
        )
        .unwrap();
        assert_eq!(pr.state, FixPrState::Planned);
        assert_eq!((pr.fix_run, pr.gate, pr.change_set, pr.base_sha), (gate.fix_run(), gate.gate(), digest, sha(4)));
        assert_eq!(pr.branch, BranchName::for_change_set("ENG-412", &digest).unwrap());

        // Opening before the commit exists is refused and changes nothing.
        let before = pr.clone();
        assert!(pr.apply(FixPrEvent::Opened { number: 7, url: url() }, t1).is_err());
        assert_eq!(pr, before);

        pr.apply(FixPrEvent::CommitCreated { commit: sha(8) }, t1).unwrap();
        assert_eq!((pr.head_commit, pr.updated_at), (Some(sha(8)), t1));
        // Branch name taken by someone else → suffixed name; an earlier commit of ours reused.
        let suffixed = BranchName::new(format!("{}-2", pr.branch)).unwrap();
        pr.apply(FixPrEvent::BranchCreated { branch: suffixed.clone(), head: sha(9) }, t1).unwrap();
        assert_eq!((&pr.branch, pr.head_commit), (&suffixed, Some(sha(9))));
        pr.apply(FixPrEvent::Opened { number: 7, url: url() }, t1).unwrap();
        assert_eq!((pr.number, pr.url.clone(), &pr.state), (Some(7), Some(url()), &FixPrState::Opened { number: 7 }));
        pr.apply(FixPrEvent::Closed { merged: false }, t1).unwrap();
        assert_eq!(pr.state, FixPrState::Closed { merged: false });

        let json = serde_json::to_value(&pr).unwrap();
        assert_eq!(json["state"], serde_json::json!({"type": "closed", "merged": false}));
        assert_eq!(serde_json::from_value::<FixPullRequest>(json).unwrap(), pr);
    }
}
