//! `AuditLog` (server.md "Audit log"): append-only record of security-relevant actions, plus
//! [`Actor`], the "who" stored by every table with a `created_by` / `updated_by` column.

use std::borrow::Cow;
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use async_trait::async_trait;
use mage_core::domain::{OrgId, RunnerId, UserId};
use mage_core::wire::api::AuditActor;
pub use mage_core::wire::api::AuditOutcome;
use time::OffsetDateTime;

use crate::error::AppError;

/// Who acted.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Actor {
    User(UserId),
    Runner(RunnerId),
    /// Operator CLI on a host (`mage admin …`).
    Operator { os_user: String },
    System,
}

impl Actor {
    /// `audit_log.actor_kind` value.
    pub fn kind(&self) -> &'static str {
        match self {
            Actor::User(_) => "user",
            Actor::Runner(_) => "runner",
            Actor::Operator { .. } => "operator",
            Actor::System => "system",
        }
    }

    /// `audit_log.actor_id` value: the prefixed id, the OS user, or `None` for `System`.
    pub fn id(&self) -> Option<String> {
        match self {
            Actor::User(id) => Some(id.to_string()),
            Actor::Runner(id) => Some(id.to_string()),
            Actor::Operator { os_user } => Some(os_user.clone()),
            Actor::System => None,
        }
    }

    /// Inverse of ([`Self::kind`], [`Self::id`]).
    pub fn from_parts(kind: &str, id: Option<&str>) -> Result<Actor, String> {
        let need = || id.ok_or_else(|| format!("actor kind {kind:?} needs an id"));
        match kind {
            "user" => need()?.parse().map(Actor::User).map_err(|e| format!("bad user actor id: {e}")),
            "runner" => need()?.parse().map(Actor::Runner).map_err(|e| format!("bad runner actor id: {e}")),
            "operator" => Ok(Actor::Operator { os_user: need()?.to_owned() }),
            "system" => Ok(Actor::System),
            other => Err(format!("unknown actor kind {other:?}")),
        }
    }

    /// One-column form for `created_by` / `updated_by` text columns: `kind` or `kind:id`.
    pub fn encode(&self) -> String {
        match self.id() {
            Some(id) => format!("{}:{id}", self.kind()),
            None => self.kind().to_owned(),
        }
    }

    /// Inverse of [`Self::encode`].
    pub fn decode(s: &str) -> Result<Actor, String> {
        match s.split_once(':') {
            Some((kind, id)) => Actor::from_parts(kind, Some(id)),
            None => Actor::from_parts(s, None),
        }
    }
}

impl From<&Actor> for AuditActor {
    fn from(actor: &Actor) -> AuditActor {
        match actor {
            Actor::User(id) => AuditActor::User { id: *id },
            Actor::Runner(id) => AuditActor::Runner { id: *id },
            Actor::Operator { os_user } => AuditActor::Operator { os_user: os_user.clone() },
            Actor::System => AuditActor::System,
        }
    }
}

impl From<Actor> for AuditActor {
    fn from(actor: Actor) -> AuditActor {
        AuditActor::from(&actor)
    }
}

impl From<AuditActor> for Actor {
    fn from(actor: AuditActor) -> Actor {
        match actor {
            AuditActor::User { id } => Actor::User(id),
            AuditActor::Runner { id } => Actor::Runner(id),
            AuditActor::Operator { os_user } => Actor::Operator { os_user },
            AuditActor::System => Actor::System,
        }
    }
}

macro_rules! audit_actions {
    ($($variant:ident => $name:literal,)*) => {
        /// Closed set of audited actions (server.md "Audit log" table); `Display` is `area.verb`.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum AuditAction {
            $($variant,)*
        }

        impl AuditAction {
            pub const ALL: &'static [AuditAction] = &[$(AuditAction::$variant,)*];

            /// `audit_log.action` value, `area.verb`.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(AuditAction::$variant => $name,)*
                }
            }
        }
    };
}

audit_actions! {
    AuthLogin => "auth.login",
    AuthLoginFailed => "auth.login_failed",
    AuthLogout => "auth.logout",
    AuthSessionsRevoked => "auth.sessions_revoked",
    OrgCreated => "org.created",
    OrgDeleted => "org.deleted",
    MemberInvited => "member.invited",
    MemberInviteWithdrawn => "member.invite_withdrawn",
    MemberJoined => "member.joined",
    MemberRoleChanged => "member.role_changed",
    MemberRemoved => "member.removed",
    IntegrationConnected => "integration.connected",
    IntegrationRevoked => "integration.revoked",
    IntegrationInstallRejected => "integration.install_rejected",
    ByokKeySet => "byok.key_set",
    ByokKeyDeleted => "byok.key_deleted",
    ByokKeyRejected => "byok.key_rejected",
    SettingsChanged => "settings.changed",
    ModelRouteChanged => "model.route_changed",
    RunStarted => "run.started",
    RunCancelled => "run.cancelled",
    RunSteered => "run.steered",
    PrOpened => "pr.opened",
    ArtifactDownloaded => "artifact.downloaded",
    ArtifactShared => "artifact.shared",
    RunnerBootstrapIssued => "runner.bootstrap_issued",
    RunnerRegistered => "runner.registered",
    RunnerRevoked => "runner.revoked",
    RunnerKeyReplaced => "runner.key_replaced",
    SecretPut => "secret.put",
    SecretStateChanged => "secret.state_changed",
    KeysRotated => "keys.rotated",
    KeysRevoked => "keys.revoked",
    TokenRevoked => "token.revoked",
}

impl fmt::Display for AuditAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AuditAction {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        AuditAction::ALL.iter().copied().find(|a| a.as_str() == s).ok_or_else(|| format!("unknown audit action {s:?}"))
    }
}

/// What the action touched: `kind` is a static noun (`"session"`, `"secret"`, …); `id` a prefixed
/// id, name, or fingerprint. `Cow` so records read back from storage keep the same type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditTarget {
    pub kind: Cow<'static, str>,
    pub id: String,
}

impl AuditTarget {
    pub fn new(kind: &'static str, id: impl Into<String>) -> AuditTarget {
        AuditTarget { kind: Cow::Borrowed(kind), id: id.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    /// `None` = platform-level.
    pub org: Option<OrgId>,
    pub actor: Actor,
    pub action: AuditAction,
    pub target: AuditTarget,
    pub outcome: AuditOutcome,
    pub ip: Option<IpAddr>,
    pub request_id: Option<String>,
    /// Fingerprints and ids only; never secret values. Order is kept.
    pub details: Vec<(Cow<'static, str>, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    pub id: i64,
    pub at: OffsetDateTime,
    pub event: AuditEvent,
}

/// `list` filter; `before` pages by record id (exclusive).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditFilter {
    pub action: Option<AuditAction>,
    pub actor: Option<Actor>,
    pub before: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuditError {
    #[error("not found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
}

/// Maximum records per `list` call (`limit` is clamped to `1..=AUDIT_LIST_MAX`).
pub const AUDIT_LIST_MAX: u32 = 200;

#[async_trait]
pub trait AuditLog: Send + Sync {
    /// Appends one record (time from the adapter's `Clock`); returns its id. Ids increase.
    async fn record(&self, event: AuditEvent) -> Result<i64, AuditError>;
    /// The org's records, newest first (`id DESC`), filtered, at most `limit` (clamped).
    async fn list(&self, org: OrgId, filter: AuditFilter, limit: u32) -> Result<Vec<AuditRecord>, AuditError>;
}

impl From<AuditError> for AppError {
    fn from(err: AuditError) -> AppError {
        match err {
            AuditError::NotFound => AppError::NotFound("audit record".into()),
            AuditError::Conflict => AppError::Conflict("audit record".into()),
            AuditError::Invalid(msg) => AppError::Invalid(msg),
            AuditError::Unavailable(msg) => AppError::Unavailable(msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    #[test]
    fn actor_round_trips_through_its_column_encoding() {
        let actors = [
            Actor::User(UserId::from_uuid(Uuid::from_u128(7))),
            Actor::Runner(RunnerId::from_uuid(Uuid::from_u128(9))),
            Actor::Operator { os_user: "ops:alice".into() },
            Actor::System,
        ];
        for actor in actors {
            assert_eq!(Actor::decode(&actor.encode()), Ok(actor.clone()));
            assert_eq!(Actor::from_parts(actor.kind(), actor.id().as_deref()), Ok(actor.clone()));
            assert_eq!(Actor::from(AuditActor::from(&actor)), actor);
        }
        assert!(Actor::decode("user").is_err());
        assert!(Actor::decode("robot:1").is_err());
        assert!(Actor::decode("user:rnr_0000000000000000000000000z").is_err());
    }

    #[test]
    fn action_names_are_unique_and_parse_back() {
        let mut names: Vec<&str> = AuditAction::ALL.iter().map(|a| a.as_str()).collect();
        for action in AuditAction::ALL {
            assert_eq!(action.as_str().parse::<AuditAction>(), Ok(*action));
        }
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), AuditAction::ALL.len());
        assert!("auth.unknown".parse::<AuditAction>().is_err());
    }
}
