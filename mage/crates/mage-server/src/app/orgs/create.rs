//! Org creation (server.md "Provisioning"): v1 creates the first org and its Owner from the
//! operator CLI (`mage admin org create --owner <login>`); the public product lets any logged-in
//! user create an org (`POST /api/v1/orgs`, caller becomes Owner).

use std::sync::Arc;

use mage_core::domain::{Org, OrgId, UserId};
use mage_core::ports::{Clock, IdGen};

use crate::app::auth::{RequestMeta, audit, event};
use crate::domain::members::valid_org_slug;
use crate::error::AppError;
use crate::ports::audit::{Actor, AuditAction, AuditLog, AuditTarget};
use crate::ports::auth::{IdentityProvider, IdentityProviderKind};
use crate::ports::org::{OrgStore, OrgStoreError, RetentionPolicy};

const NAME_MAX: usize = 100;

/// Who becomes the first Owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrgOwner {
    /// An existing user (public product: the caller).
    User(UserId),
    /// A GitHub login resolved to its stable id; the user row is created if this identity never
    /// logged in, and the first login maps to it (CLI).
    GitHubLogin(String),
}

pub struct CreateOrg {
    orgs: Arc<dyn OrgStore>,
    github: Option<Arc<dyn IdentityProvider>>,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
}

impl CreateOrg {
    pub fn new(
        orgs: Arc<dyn OrgStore>,
        github: Option<Arc<dyn IdentityProvider>>,
        audit: Arc<dyn AuditLog>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGen>,
    ) -> CreateOrg {
        CreateOrg { orgs, github, audit, clock, ids }
    }

    pub async fn create(&self, slug: &str, name: &str, owner: OrgOwner, actor: &Actor, meta: &RequestMeta) -> Result<Org, AppError> {
        if !valid_org_slug(slug) {
            return Err(AppError::Invalid("slug must be 2–40 of [a-z0-9-], not reserved".into()));
        }
        let name = name.trim();
        if name.is_empty() || name.chars().count() > NAME_MAX {
            return Err(AppError::Invalid(format!("name must be 1–{NAME_MAX} characters")));
        }
        if self.orgs.org_by_slug(slug).await?.is_some() {
            return Err(AppError::Conflict(format!("org {slug:?} exists")));
        }
        let owner = match owner {
            OrgOwner::User(user) => self.orgs.get_user(user).await?.id,
            OrgOwner::GitHubLogin(login) => {
                let github = self.github.as_ref().ok_or_else(|| AppError::Invalid("GitHub login is not configured".into()))?;
                let identity = github
                    .lookup_login(login.trim())
                    .await?
                    .filter(|i| i.provider == IdentityProviderKind::GitHub)
                    .ok_or_else(|| AppError::Invalid(format!("unknown GitHub login {login:?}")))?;
                self.orgs.upsert_user_by_identity(&identity, self.ids.new_id()).await?.id
            }
        };
        let org = Org { id: self.ids.new_id::<OrgId>(), slug: slug.to_owned(), name: name.to_owned(), created_at: self.clock.now() };
        self.orgs.create_org(&org, owner, &RetentionPolicy::default()).await.map_err(|e| match e {
            OrgStoreError::Conflict => AppError::Conflict(format!("org {slug:?} exists")),
            other => other.into(),
        })?;
        audit(
            self.audit.as_ref(),
            event(Some(org.id), actor.clone(), AuditAction::OrgCreated, AuditTarget::new("org", org.id.to_string()), meta)
                .detail("slug", org.slug.clone())
                .detail("owner", owner.to_string()),
        )
        .await?;
        Ok(org)
    }
}

#[cfg(test)]
mod tests {
    use mage_core::domain::MemberRole;
    use mage_core::testing::{FakeClock, MemJobQueue, SeqIdGen};

    use super::*;
    use crate::testing::{FakeIdentityProvider, MemAuditLog, MemChangeFeed, MemOrgStore};

    struct Fixture {
        orgs: Arc<MemOrgStore>,
        github: Arc<FakeIdentityProvider>,
        create: CreateOrg,
    }

    fn fixture() -> Fixture {
        let clock = Arc::new(FakeClock::new());
        let ids: Arc<dyn IdGen> = Arc::new(SeqIdGen::new());
        let jobs = Arc::new(MemJobQueue::new(clock.clone(), ids.clone()));
        let orgs = Arc::new(MemOrgStore::new(clock.clone(), jobs, Arc::new(MemChangeFeed::new(clock.clone()))));
        let github = Arc::new(FakeIdentityProvider::new(IdentityProviderKind::GitHub));
        let create = CreateOrg::new(orgs.clone(), Some(github.clone()), Arc::new(MemAuditLog::new(clock.clone())), clock, ids);
        Fixture { orgs, github, create }
    }

    fn operator() -> Actor {
        Actor::Operator { os_user: "ops".into() }
    }

    #[tokio::test]
    async fn cli_creates_the_org_with_the_github_login_as_owner() {
        let f = fixture();
        let identity = f.github.identity("1234", "olivia", None);
        f.github.add_login(identity.clone());
        let org = f.create.create("hoop", " Hoop ", OrgOwner::GitHubLogin("olivia".into()), &operator(), &RequestMeta::default()).await.expect("create");
        assert_eq!((org.slug.as_str(), org.name.as_str()), ("hoop", "Hoop"));
        // The owner's first login maps to the same user (identity key (github, 1234)).
        let user = f.orgs.upsert_user_by_identity(&identity, UserId::from(uuid::Uuid::from_u128(99))).await.expect("user");
        let membership = f.orgs.membership(org.id, user.id).await.expect("read").expect("member");
        assert_eq!(membership.role, MemberRole::Owner);
    }

    #[tokio::test]
    async fn slugs_are_validated_and_unique() {
        let f = fixture();
        let identity = f.github.identity("1", "olivia", None);
        f.github.add_login(identity);
        let owner = || OrgOwner::GitHubLogin("olivia".into());
        for bad in ["API", "x", "has space", "api"] {
            let err = f.create.create(bad, "Name", owner(), &operator(), &RequestMeta::default()).await.expect_err(bad);
            assert!(matches!(err, AppError::Invalid(_)), "{bad}: {err}");
        }
        f.create.create("hoop", "Hoop", owner(), &operator(), &RequestMeta::default()).await.expect("first");
        let dup = f.create.create("hoop", "Hoop 2", owner(), &operator(), &RequestMeta::default()).await.expect_err("duplicate");
        assert!(matches!(dup, AppError::Conflict(_)));
        let unknown = f.create.create("acme", "Acme", OrgOwner::GitHubLogin("ghost".into()), &operator(), &RequestMeta::default()).await.expect_err("unknown");
        assert!(matches!(unknown, AppError::Invalid(_)));
    }
}
