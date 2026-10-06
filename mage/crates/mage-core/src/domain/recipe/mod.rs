//! `Recipe` aggregate and `RecipeStatus` (domain.md "Recipe"). The recipe schema, parsing,
//! validation, canonical form and hash live in [`spec`] (executor.md).

mod spec;

pub use spec::*;

use std::ops::Deref;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::domain::error::{DomainError, ensure_same_org};
use crate::domain::gates::BootGatePassed;
use crate::domain::ids::{CommitSha, OrgId, RecipeHash, RepoId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RecipeStatus {
    Draft,
    Validated {
        #[serde(with = "time::serde::rfc3339")]
        #[ts(type = "string")]
        at: OffsetDateTime,
    },
    Broken {
        reason: String,
        #[serde(with = "time::serde::rfc3339")]
        #[ts(type = "string")]
        at: OffsetDateTime,
    },
}

/// Events of the `RecipeStatus` machine (runs.md "ApplyOutcome", Learn row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipeEvent {
    /// The Boot gate passed (the only way to `Validated`).
    BootPassed { proof: BootGatePassed, at: OffsetDateTime },
    /// The Boot gate of a Learn run failed for the currently `Validated` recipe.
    BootFailed { reason: String, at: OffsetDateTime },
}

impl RecipeEvent {
    pub fn name(&self) -> &'static str {
        match self {
            Self::BootPassed { .. } => "boot_passed",
            Self::BootFailed { .. } => "boot_failed",
        }
    }
}

impl RecipeStatus {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Validated { .. } => "validated",
            Self::Broken { .. } => "broken",
        }
    }

    /// `Draft`/`Validated`/`Broken` + boot passed → `Validated`; `Validated` + boot failed →
    /// `Broken`. A failed boot leaves a `Draft` a draft (no transition), and a `Broken` recipe
    /// stays broken.
    pub fn apply(self, event: &RecipeEvent) -> Result<RecipeStatus, DomainError> {
        match (&self, event) {
            (_, RecipeEvent::BootPassed { at, .. }) => Ok(RecipeStatus::Validated { at: *at }),
            (RecipeStatus::Validated { .. }, RecipeEvent::BootFailed { reason, at }) => {
                Ok(RecipeStatus::Broken { reason: reason.clone(), at: *at })
            }
            (from, event) => Err(DomainError::InvalidTransition { from: from.name(), event: event.name() }),
        }
    }
}

/// Fields of a recipe (read through `Recipe`'s `Deref`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct RecipeData {
    pub org_id: OrgId,
    pub repo_id: RepoId,
    /// SHA-256 of the canonical form of `body`.
    pub hash: RecipeHash,
    /// Original TOML text, comments included (for display).
    pub body: String,
    pub validated_at_sha: Option<CommitSha>,
    pub status: RecipeStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(transparent)]
pub struct Recipe(RecipeData);

impl Deref for Recipe {
    type Target = RecipeData;

    fn deref(&self) -> &RecipeData {
        &self.0
    }
}

impl Recipe {
    /// A `Draft` recipe for `body`; the hash is computed from the parsed spec, so it always
    /// matches the body. Validation issues are the caller's concern (Learn result intake).
    pub fn draft(org_id: OrgId, repo_id: RepoId, body: String) -> Result<Recipe, DomainError> {
        let spec = RecipeSpec::parse(&body).map_err(|e| DomainError::Invalid(format!("recipe: {e}")))?;
        Ok(Recipe(RecipeData { org_id, repo_id, hash: spec.hash(), body, validated_at_sha: None, status: RecipeStatus::Draft }))
    }

    /// Rebuild a stored recipe (storage adapters only).
    pub fn rehydrate(data: RecipeData) -> Recipe {
        Recipe(data)
    }

    pub fn into_data(self) -> RecipeData {
        self.0
    }

    /// The only way to reach `Validated`: the Boot gate's proof for this repo and hash.
    pub fn validate(&mut self, proof: BootGatePassed, at: OffsetDateTime) -> Result<(), DomainError> {
        if proof.repo_id() != self.0.repo_id || proof.recipe() != self.0.hash {
            return Err(DomainError::invalid("boot proof is for another recipe"));
        }
        let sha = proof.sha();
        let next = self.0.status.clone().apply(&RecipeEvent::BootPassed { proof, at })?;
        self.0.status = next;
        self.0.validated_at_sha = Some(sha);
        Ok(())
    }

    /// A Learn run's Boot gate failed for this (currently validated) recipe.
    pub fn mark_broken(&mut self, reason: impl Into<String>, at: OffsetDateTime) -> Result<(), DomainError> {
        let next = self.0.status.clone().apply(&RecipeEvent::BootFailed { reason: reason.into(), at })?;
        self.0.status = next;
        Ok(())
    }

    /// Tenancy check when linking a recipe to another aggregate of `org_id`.
    pub fn ensure_org(&self, org_id: OrgId) -> Result<(), DomainError> {
        ensure_same_org(self.0.org_id, org_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::gates::testing::boot_gate_passed;
    use crate::domain::ids::{GateId, RunId};
    use time::macros::datetime;
    use uuid::Uuid;

    const T0: OffsetDateTime = datetime!(2026-06-01 00:00 UTC);
    const T1: OffsetDateTime = datetime!(2026-06-02 00:00 UTC);

    fn proof(repo: RepoId, hash: RecipeHash) -> BootGatePassed {
        boot_gate_passed(
            GateId::from_uuid(Uuid::from_u128(1)),
            RunId::from_uuid(Uuid::from_u128(2)),
            repo,
            hash,
            CommitSha::from_bytes([4; 20]),
        )
    }

    fn any_proof() -> BootGatePassed {
        proof(RepoId::from_uuid(Uuid::from_u128(3)), RecipeHash::from_bytes([1; 32]))
    }

    #[test]
    fn recipe_status_full_transition_table() {
        let states = [RecipeStatus::Draft, RecipeStatus::Validated { at: T0 }, RecipeStatus::Broken { reason: "old".into(), at: T0 }];
        let events =
            [RecipeEvent::BootPassed { proof: any_proof(), at: T1 }, RecipeEvent::BootFailed { reason: "migrate failed".into(), at: T1 }];
        for s in &states {
            for e in &events {
                let got = s.clone().apply(e);
                let want = match (s, e) {
                    (_, RecipeEvent::BootPassed { .. }) => Some(RecipeStatus::Validated { at: T1 }),
                    (RecipeStatus::Validated { .. }, RecipeEvent::BootFailed { .. }) => {
                        Some(RecipeStatus::Broken { reason: "migrate failed".into(), at: T1 })
                    }
                    _ => None,
                };
                match want {
                    Some(to) => assert_eq!(got, Ok(to), "{} + {}", s.name(), e.name()),
                    None => assert_eq!(got, Err(DomainError::InvalidTransition { from: s.name(), event: e.name() })),
                }
            }
        }
    }

    fn data(repo: RepoId, hash: RecipeHash, status: RecipeStatus) -> RecipeData {
        RecipeData {
            org_id: OrgId::from_uuid(Uuid::from_u128(9)),
            repo_id: repo,
            hash,
            body: "version = 1".into(),
            validated_at_sha: None,
            status,
        }
    }

    #[test]
    fn validate_requires_proof_for_this_recipe() {
        let repo = RepoId::from_uuid(Uuid::from_u128(3));
        let hash = RecipeHash::from_bytes([1; 32]);
        let mut recipe = Recipe::rehydrate(data(repo, hash, RecipeStatus::Draft));
        let other_hash = proof(repo, RecipeHash::from_bytes([2; 32]));
        assert!(recipe.validate(other_hash, T1).is_err());
        let other_repo = proof(RepoId::from_uuid(Uuid::from_u128(4)), hash);
        assert!(recipe.validate(other_repo, T1).is_err());
        assert_eq!(recipe.status, RecipeStatus::Draft);
        recipe.validate(proof(repo, hash), T1).unwrap();
        assert_eq!(recipe.status, RecipeStatus::Validated { at: T1 });
        assert_eq!(recipe.validated_at_sha, Some(CommitSha::from_bytes([4; 20])));
        recipe.mark_broken("seed failed", T1).unwrap();
        assert!(matches!(recipe.status, RecipeStatus::Broken { .. }));
        // A broken recipe can be validated again by a passing Boot gate.
        recipe.validate(proof(repo, hash), T1).unwrap();
        assert!(matches!(recipe.status, RecipeStatus::Validated { .. }));
    }

    #[test]
    fn draft_cannot_be_marked_broken() {
        let mut recipe =
            Recipe::rehydrate(data(RepoId::from_uuid(Uuid::from_u128(3)), RecipeHash::from_bytes([1; 32]), RecipeStatus::Draft));
        assert!(recipe.mark_broken("x", T1).is_err());
    }
}
