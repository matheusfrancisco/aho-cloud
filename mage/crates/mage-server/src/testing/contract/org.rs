//! `OrgStore` contract (storage.md "`OrgStore`", "Testing" → `OrgStore`; server.md "Authentication
//! and authorization": identities are never linked by email). Users and orgs are created through
//! the port itself, so the suite needs no seeding of its own tables.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::fix::FixSettings;
use mage_core::domain::{
    Budget, Customer, CustomerId, ExternalRef, HarnessKind, Integration, IntegrationKind, IntegrationStatus, JobKind, JobLane, JobPayload,
    MemberRole, Membership, Org, OrgId, User, UserId,
};
use mage_core::ports::clock::Clock;
use mage_core::ports::common::{Cursor, PageRequest};
use mage_core::ports::jobs::{JobQueue, LeaseRequest, NewJob};
use mage_core::testing::FakeClock;
use mage_core::wire::api::{GeneralSettings, ModelRouteKind, RunBudgets, TriageSettings};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::intake::{IntakeConfig, IntegrationSettings, RepoRoute, RouteWhen};
use crate::ports::auth::{ExternalIdentity, IdentityProviderKind};
use crate::ports::feed::{ChangeFeed, FeedId, FeedKind};
use crate::ports::llm::ModelSettings;
use crate::ports::org::{Invite, InviteId, OrgState, OrgStore, OrgStoreError, RetentionPolicy, normalize_refs};
use crate::testing::seed::Seed;

pub struct OrgFixture {
    pub orgs: Arc<dyn OrgStore>,
    /// The queue the store's outbox writes to.
    pub jobs: Arc<dyn JobQueue>,
    /// The feed the store appends to.
    pub feed: Arc<dyn ChangeFeed>,
    pub seed: Arc<dyn Seed>,
}

/// Runs every `OrgStore` scenario, each against a fresh fixture from `make`.
pub async fn contract_org_store<F, Fut>(make: F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = OrgFixture>,
{
    macro_rules! scenario {
        ($name:ident) => {{
            let clock = Arc::new(FakeClock::new());
            let fixture = make(clock.clone()).await;
            $name(&clock, &fixture).await;
        }};
    }
    scenario!(orgs_are_created_with_an_owner);
    scenario!(org_state_is_a_cas_with_outbox);
    scenario!(retention_is_bounded);
    scenario!(settings_default_then_round_trip);
    scenario!(users_upsert_by_login_identity);
    scenario!(identities_link_only_explicitly);
    scenario!(memberships_protect_the_last_owner);
    scenario!(members_page_by_user_id_and_orgs_sort_by_slug);
    scenario!(invites_are_unique_per_subject_until_expiry);
    scenario!(accept_invites_converts_unexpired_ones);
    scenario!(integrations_bind_one_active_org);
    scenario!(intake_config_round_trips);
    scenario!(customers_own_their_refs);
    scenario!(tenants_are_isolated);
}

fn org_id(n: u128) -> OrgId {
    OrgId::from_uuid(Uuid::from_u128(0x0c00 + n))
}

fn user_id(n: u128) -> UserId {
    UserId::from_uuid(Uuid::from_u128(0x0d00 + n))
}

fn customer_id(n: u128) -> CustomerId {
    CustomerId::from_uuid(Uuid::from_u128(0x0e00 + n))
}

fn invite_id(n: u128) -> InviteId {
    InviteId::from_uuid(Uuid::from_u128(0x0f00 + n))
}

fn github(subject: &str, login: &str, email: Option<&str>) -> ExternalIdentity {
    ExternalIdentity {
        provider: IdentityProviderKind::GitHub,
        subject: subject.into(),
        login: login.into(),
        name: None,
        email: email.map(Into::into),
        avatar_url: None,
    }
}

fn google(subject: &str, email: Option<&str>) -> ExternalIdentity {
    ExternalIdentity {
        provider: IdentityProviderKind::Google,
        subject: subject.into(),
        login: format!("{subject}@accounts"),
        name: None,
        email: email.map(Into::into),
        avatar_url: None,
    }
}

/// User `n` with GitHub identity `gh-{n}` and email `user{n}@example.com`.
async fn user(orgs: &dyn OrgStore, n: u128) -> User {
    let identity = github(&format!("gh-{n}"), &format!("user{n}"), Some(&format!("user{n}@example.com")));
    orgs.upsert_user_by_identity(&identity, user_id(n)).await.expect("upsert user")
}

/// Org `n` with slug `slug`, owned by `owner`, default retention.
async fn org_with_slug(clock: &FakeClock, orgs: &dyn OrgStore, n: u128, slug: &str, owner: UserId) -> Org {
    let org = Org { id: org_id(n), slug: slug.into(), name: format!("Org {n}"), created_at: clock.now() };
    orgs.create_org(&org, owner, &RetentionPolicy::default()).await.expect("create_org");
    org
}

async fn new_org(clock: &FakeClock, orgs: &dyn OrgStore, n: u128, owner: UserId) -> Org {
    org_with_slug(clock, orgs, n, &format!("org-{n}"), owner).await
}

fn member(org: OrgId, user: UserId, role: MemberRole) -> Membership {
    Membership { org_id: org, user_id: user, role }
}

fn invite(org: OrgId, n: u128, subject: &str, role: MemberRole, by: UserId, created_at: OffsetDateTime, ttl_days: i64) -> Invite {
    Invite {
        id: invite_id(n),
        org_id: org,
        provider: "github".into(),
        subject: subject.into(),
        login: format!("login-{subject}"),
        role,
        invited_by: by,
        created_at,
        expires_at: created_at + time::Duration::days(ttl_days),
    }
}

fn integration(org: OrgId, kind: IntegrationKind, external_id: &str, status: IntegrationStatus) -> Integration {
    Integration { org_id: org, kind, external_id: external_id.into(), status }
}

fn customer(org: OrgId, n: u128, name: &str, refs: Vec<ExternalRef>, environment: Option<&str>) -> Customer {
    Customer { id: customer_id(n), org_id: org, name: name.into(), external_refs: refs, environment: environment.map(Into::into) }
}

fn slack(team: &str, channel: &str) -> ExternalRef {
    ExternalRef::SlackChannel { team: team.into(), channel: channel.into() }
}

/// `customer` as the store returns it.
fn stored(customer: &Customer) -> Customer {
    Customer { external_refs: normalize_refs(&customer.external_refs), ..customer.clone() }
}

async fn feed_of(feed: &dyn ChangeFeed, org: OrgId) -> Vec<(FeedKind, String)> {
    feed.after(org, FeedId(0), 100).await.expect("feed").into_iter().map(|e| (e.kind, e.subject)).collect()
}

/// Leases every ready job of the lanes of `kinds` and returns their kinds.
async fn leased(jobs: &dyn JobQueue, kinds: &[JobKind]) -> Vec<JobKind> {
    let mut lanes: Vec<JobLane> = Vec::new();
    for kind in kinds {
        if !lanes.contains(&kind.lane()) {
            lanes.push(kind.lane());
        }
    }
    let mut out = Vec::new();
    for lane in lanes {
        let req = LeaseRequest { lane, kinds: None, worker: "contract".into(), max: 100 };
        out.extend(jobs.lease(&req).await.expect("lease").into_iter().map(|j| j.kind));
    }
    out
}

fn bad_cursor(limit: u16) -> PageRequest {
    PageRequest { after: Some(Cursor("not-a-cursor".into())), limit }
}

async fn orgs_are_created_with_an_owner(clock: &FakeClock, fx: &OrgFixture) {
    let orgs = &*fx.orgs;
    let owner = user(orgs, 1).await;
    let acme = Org { id: org_id(1), slug: "acme".into(), name: "Acme".into(), created_at: clock.now() };
    let retention = RetentionPolicy { run_artifacts_days: 30, run_events_days: 60, keep_referenced_artifacts: false };
    assert_eq!(orgs.create_org(&acme, owner.id, &retention).await, Ok(()));
    assert_eq!(orgs.get_org(acme.id).await, Ok(acme.clone()));
    assert_eq!(orgs.org_by_slug("acme").await, Ok(Some(acme.clone())));
    assert_eq!(orgs.org_by_slug("ACME").await, Ok(None), "slugs match exactly");
    assert_eq!(orgs.org_state(acme.id).await, Ok(OrgState::Active));
    assert_eq!(orgs.retention(acme.id).await, Ok(retention));
    assert_eq!(orgs.membership(acme.id, owner.id).await, Ok(Some(member(acme.id, owner.id, MemberRole::Owner))), "the creator is Owner");
    assert_eq!(orgs.orgs_for_user(owner.id).await, Ok(vec![(acme.clone(), member(acme.id, owner.id, MemberRole::Owner))]));

    let same_id = Org { slug: "other".into(), ..acme.clone() };
    assert_eq!(orgs.create_org(&same_id, owner.id, &retention).await, Err(OrgStoreError::Conflict), "id taken");
    let same_slug = Org { id: org_id(2), ..acme.clone() };
    assert_eq!(orgs.create_org(&same_slug, owner.id, &retention).await, Err(OrgStoreError::Conflict), "slug taken");
    assert_eq!(orgs.get_org(org_id(2)).await, Err(OrgStoreError::NotFound));
    assert_eq!(orgs.org_by_slug("other").await, Ok(None));

    let bad = RetentionPolicy { run_events_days: 6, ..RetentionPolicy::default() };
    let third = Org { id: org_id(3), slug: "third".into(), ..acme.clone() };
    assert!(matches!(orgs.create_org(&third, owner.id, &bad).await, Err(OrgStoreError::Invalid(_))));
    assert_eq!(orgs.get_org(org_id(3)).await, Err(OrgStoreError::NotFound), "an invalid retention writes nothing");
    assert_eq!(orgs.org_state(org_id(9)).await, Err(OrgStoreError::NotFound));
    assert_eq!(orgs.retention(org_id(9)).await, Err(OrgStoreError::NotFound));
    assert_eq!(feed_of(&*fx.feed, acme.id).await, vec![], "creating an org writes no feed entry");
}

async fn org_state_is_a_cas_with_outbox(clock: &FakeClock, fx: &OrgFixture) {
    let orgs = &*fx.orgs;
    let owner = user(orgs, 1).await;
    let org = new_org(clock, orgs, 1, owner.id).await;
    let delete = || NewJob::new(JobPayload::OpsDeleteOrg { org_id: org.id });
    let kinds = [JobKind::OpsDeleteOrg, JobKind::OpsPurge];

    let invalid = NewJob { kind: JobKind::OpsPurge, ..delete() };
    let out = orgs.set_org_state(org.id, OrgState::Active, OrgState::Deleting, &[invalid]).await;
    assert!(matches!(out, Err(OrgStoreError::Invalid(_))), "{out:?}");
    assert_eq!(orgs.org_state(org.id).await, Ok(OrgState::Active), "an invalid outbox job aborts the transition");
    assert_eq!(leased(&*fx.jobs, &kinds).await, vec![]);

    assert_eq!(orgs.set_org_state(org.id, OrgState::Active, OrgState::Deleting, &[delete()]).await, Ok(()));
    assert_eq!(orgs.org_state(org.id).await, Ok(OrgState::Deleting));
    assert_eq!(leased(&*fx.jobs, &kinds).await, vec![JobKind::OpsDeleteOrg], "the job commits with the state");

    assert_eq!(orgs.set_org_state(org.id, OrgState::Active, OrgState::Deleting, &[delete()]).await, Err(OrgStoreError::Conflict));
    assert_eq!(leased(&*fx.jobs, &kinds).await, vec![], "a CAS miss enqueues nothing");
    assert_eq!(orgs.set_org_state(org.id, OrgState::Deleting, OrgState::Active, &[]).await, Ok(()));
    assert_eq!(orgs.org_state(org.id).await, Ok(OrgState::Active));
    assert_eq!(orgs.set_org_state(org_id(9), OrgState::Active, OrgState::Deleting, &[]).await, Err(OrgStoreError::NotFound));
    assert_eq!(feed_of(&*fx.feed, org.id).await, vec![]);
}

async fn retention_is_bounded(clock: &FakeClock, fx: &OrgFixture) {
    let orgs = &*fx.orgs;
    let owner = user(orgs, 1).await;
    let org = new_org(clock, orgs, 1, owner.id).await;
    assert_eq!(orgs.retention(org.id).await, Ok(RetentionPolicy::default()));
    let edges = RetentionPolicy { run_artifacts_days: 7, run_events_days: 730, keep_referenced_artifacts: false };
    assert_eq!(orgs.set_retention(org.id, &edges).await, Ok(()));
    assert_eq!(orgs.retention(org.id).await, Ok(edges));
    for (artifacts, events) in [(6, 90), (90, 731), (0, 0)] {
        let bad = RetentionPolicy { run_artifacts_days: artifacts, run_events_days: events, keep_referenced_artifacts: true };
        assert!(matches!(orgs.set_retention(org.id, &bad).await, Err(OrgStoreError::Invalid(_))), "{bad:?}");
    }
    assert_eq!(orgs.retention(org.id).await, Ok(edges), "rejected policies change nothing");
    assert_eq!(orgs.set_retention(org_id(9), &edges).await, Err(OrgStoreError::NotFound));
    assert_eq!(feed_of(&*fx.feed, org.id).await, vec![(FeedKind::Settings, org.id.to_string())]);
}

async fn settings_default_then_round_trip(clock: &FakeClock, fx: &OrgFixture) {
    let orgs = &*fx.orgs;
    let owner = user(orgs, 1).await;
    let org = new_org(clock, orgs, 1, owner.id).await;
    let other = new_org(clock, orgs, 2, owner.id).await;
    assert_eq!(orgs.triage_settings(org.id).await, Ok(TriageSettings::default()), "no row → Default");
    assert_eq!(orgs.fix_settings(org.id).await, Ok(FixSettings::default()));
    assert_eq!(orgs.model_settings(org.id).await, Ok(ModelSettings::default()));
    assert_eq!(orgs.general_settings(org.id).await, Ok(GeneralSettings::default()));

    let triage = TriageSettings {
        auto_triage: false,
        harness: HarnessKind::ClaudeCode,
        model: Some("claude-sonnet-4-5".into()),
        max_causes: 3,
        layer_wait: Duration::from_secs(90),
        auto_retriage_per_day: 0,
        ticket_cost_cap_micros: 1,
        ..TriageSettings::default()
    };
    let mut fix = FixSettings { run: true, pull_requests: true, allow_workflow_edits: true, repetitions: 3, ..FixSettings::default() };
    fix.pr.labels = vec!["bug".into()];
    fix.pr.reviewers.teams = vec!["core".into()];
    let model = ModelSettings {
        route: ModelRouteKind::Vertex,
        allowed_models: vec!["claude-opus-4-1".into(), "claude-sonnet-4-5".into()],
        model_map: BTreeMap::from([("claude-sonnet-4-5".to_owned(), "claude-sonnet-4-5@20250929".to_owned())]),
        max_tokens_cap: Some(8192),
    };
    let defaults = RunBudgets::default();
    let general = GeneralSettings {
        harness: HarnessKind::ClaudeCode,
        budgets: RunBudgets { learn: Budget { max_turns: 12, ..defaults.learn }, ..defaults },
    };
    assert_eq!(orgs.set_triage_settings(org.id, &triage).await, Ok(()));
    assert_eq!(orgs.set_fix_settings(org.id, &fix).await, Ok(()));
    assert_eq!(orgs.set_model_settings(org.id, &model).await, Ok(()));
    assert_eq!(orgs.set_general_settings(org.id, &general).await, Ok(()));
    assert_eq!(orgs.triage_settings(org.id).await, Ok(triage.clone()));
    assert_eq!(orgs.fix_settings(org.id).await, Ok(fix));
    assert_eq!(orgs.model_settings(org.id).await, Ok(model));
    assert_eq!(orgs.general_settings(org.id).await, Ok(general));

    let again = TriageSettings { auto_triage: true, model: None, ..triage };
    assert_eq!(orgs.set_triage_settings(org.id, &again).await, Ok(()));
    assert_eq!(orgs.triage_settings(org.id).await, Ok(again), "a second write replaces the row");
    assert_eq!(orgs.triage_settings(other.id).await, Ok(TriageSettings::default()), "settings are per org");
    assert_eq!(orgs.model_settings(other.id).await, Ok(ModelSettings::default()));
    let subject = org.id.to_string();
    assert_eq!(feed_of(&*fx.feed, org.id).await, vec![(FeedKind::Settings, subject); 5]);
    assert_eq!(feed_of(&*fx.feed, other.id).await, vec![]);
}

async fn users_upsert_by_login_identity(_clock: &FakeClock, fx: &OrgFixture) {
    let orgs = &*fx.orgs;
    let ada_gh = ExternalIdentity {
        name: Some("Ada Lovelace".into()),
        email: Some(" Ada@Example.com ".into()),
        avatar_url: Some("https://avatars.example/1001".into()),
        ..github("1001", "ada", None)
    };
    let ada = User { id: user_id(1), email: "Ada@Example.com".into(), name: "Ada Lovelace".into() };
    assert_eq!(orgs.upsert_user_by_identity(&ada_gh, user_id(1)).await, Ok(ada.clone()), "new identity → user new_id, email trimmed");
    assert_eq!(orgs.get_user(user_id(1)).await, Ok(ada.clone()));

    let refreshed = ExternalIdentity { login: "ada2".into(), name: None, email: Some("ada@new.example".into()), avatar_url: None, ..ada_gh.clone() };
    let ada = User { id: user_id(1), email: "ada@new.example".into(), name: "ada2".into() };
    assert_eq!(orgs.upsert_user_by_identity(&refreshed, user_id(9)).await, Ok(ada.clone()), "a known identity refreshes its user");
    assert_eq!(orgs.get_user(user_id(9)).await, Err(OrgStoreError::NotFound), "new_id is unused for a known identity");
    let no_email = ExternalIdentity { email: None, name: Some("Ada".into()), ..ada_gh.clone() };
    let ada = User { name: "Ada".into(), ..ada };
    assert_eq!(orgs.upsert_user_by_identity(&no_email, user_id(9)).await, Ok(ada.clone()), "no email keeps the stored one");

    let carol = orgs.upsert_user_by_identity(&github("1003", "carol", None), user_id(3)).await;
    assert_eq!(
        carol,
        Ok(User { id: user_id(3), email: "1003+carol@users.noreply.github.com".into(), name: "carol".into() }),
        "GitHub without email gets the noreply address"
    );

    let clash = github("1002", "bob", Some("ADA@new.example"));
    assert_eq!(orgs.upsert_user_by_identity(&clash, user_id(2)).await, Err(OrgStoreError::Conflict), "emails are unique, case-insensitively");
    assert_eq!(orgs.get_user(user_id(2)).await, Err(OrgStoreError::NotFound), "never linked by email");
    let bob = orgs.upsert_user_by_identity(&github("1002", "bob", Some("bob@example.com")), user_id(2)).await.unwrap();
    assert_eq!(bob, User { id: user_id(2), email: "bob@example.com".into(), name: "bob".into() });

    let steal = ExternalIdentity { email: Some("Bob@Example.com".into()), ..ada_gh.clone() };
    assert_eq!(orgs.upsert_user_by_identity(&steal, user_id(9)).await, Err(OrgStoreError::Conflict), "a refresh cannot take another user's email");
    assert_eq!(orgs.get_user(user_id(1)).await, Ok(ada), "the failed refresh changed nothing");

    let out = orgs.upsert_user_by_identity(&google("g-dan", None), user_id(4)).await;
    assert!(matches!(out, Err(OrgStoreError::Invalid(_))), "Google without email: {out:?}");
    assert_eq!(orgs.get_user(user_id(4)).await, Err(OrgStoreError::NotFound));
    let dan = orgs.upsert_user_by_identity(&google("g-dan", Some("dan@corp.example")), user_id(4)).await;
    assert_eq!(dan, Ok(User { id: user_id(4), email: "dan@corp.example".into(), name: "g-dan@accounts".into() }), "name falls back to the login");

    let reused_id = github("1005", "eve", Some("eve@example.com"));
    assert_eq!(orgs.upsert_user_by_identity(&reused_id, user_id(1)).await, Err(OrgStoreError::Conflict), "new_id already taken");
    assert_eq!(orgs.get_user(user_id(7)).await, Err(OrgStoreError::NotFound));
}

async fn identities_link_only_explicitly(_clock: &FakeClock, fx: &OrgFixture) {
    let orgs = &*fx.orgs;
    let ada_gh = ExternalIdentity { name: Some("Ada".into()), ..github("2001", "ada", Some("ada@example.com")) };
    let ada = orgs.upsert_user_by_identity(&ada_gh, user_id(1)).await.unwrap();
    let bob = user(orgs, 2).await;
    let ada_google = ExternalIdentity { name: Some("Ada".into()), ..google("g-ada", Some("ada@example.com")) };

    assert_eq!(orgs.upsert_user_by_identity(&ada_google, user_id(3)).await, Err(OrgStoreError::Conflict), "same email does not link");
    assert_eq!(orgs.link_identity(&ada_google, ada.id).await, Ok(()));
    assert_eq!(orgs.upsert_user_by_identity(&ada_google, user_id(3)).await, Ok(ada.clone()), "the linked identity logs in as its user");
    assert_eq!(orgs.get_user(user_id(3)).await, Err(OrgStoreError::NotFound));
    assert_eq!(orgs.link_identity(&ada_google, ada.id).await, Ok(()), "linking again to the same user is a no-op");
    assert_eq!(orgs.link_identity(&ada_google, bob.id).await, Err(OrgStoreError::Conflict), "linked to another user");
    assert_eq!(orgs.link_identity(&ada_gh, bob.id).await, Err(OrgStoreError::Conflict));
    assert_eq!(orgs.link_identity(&google("g-new", None), user_id(9)).await, Err(OrgStoreError::NotFound), "unknown user");
    assert_eq!(orgs.link_identity(&ada_google, user_id(9)).await, Err(OrgStoreError::NotFound), "the user is checked first");
    assert_eq!(orgs.link_identity(&github("2009", "bob-alt", None), bob.id).await, Ok(()));
    let bob_alt = orgs.upsert_user_by_identity(&github("2009", "bob-alt", None), user_id(5)).await;
    assert_eq!(bob_alt, Ok(User { name: "bob-alt".into(), ..bob }), "a linked identity without email keeps the email, refreshes the name");
}

async fn memberships_protect_the_last_owner(clock: &FakeClock, fx: &OrgFixture) {
    let orgs = &*fx.orgs;
    let mut users = Vec::new();
    for n in 1..=4 {
        users.push(user(orgs, n).await.id);
    }
    let (u1, u2, u3, u4) = (users[0], users[1], users[2], users[3]);
    let org = new_org(clock, orgs, 1, u1).await;
    let m = |user, role| member(org.id, user, role);

    assert_eq!(orgs.put_membership(&m(u2, MemberRole::Admin)).await, Ok(()));
    assert_eq!(orgs.put_membership(&m(u3, MemberRole::Member)).await, Ok(()));
    assert_eq!(orgs.put_membership(&m(u3, MemberRole::Admin)).await, Ok(()), "put changes the role");
    assert_eq!(orgs.membership(org.id, u3).await, Ok(Some(m(u3, MemberRole::Admin))));
    assert_eq!(orgs.membership(org.id, u4).await, Ok(None));

    assert_eq!(orgs.put_membership(&m(u1, MemberRole::Admin)).await, Err(OrgStoreError::Conflict), "demoting the last Owner");
    assert_eq!(orgs.remove_membership(org.id, u1).await, Err(OrgStoreError::Conflict), "removing the last Owner");
    assert_eq!(orgs.membership(org.id, u1).await, Ok(Some(m(u1, MemberRole::Owner))));
    assert_eq!(orgs.put_membership(&m(u1, MemberRole::Owner)).await, Ok(()), "keeping the Owner role is fine");

    assert_eq!(orgs.put_membership(&m(u2, MemberRole::Owner)).await, Ok(()));
    assert_eq!(orgs.put_membership(&m(u1, MemberRole::Member)).await, Ok(()), "another Owner remains");
    assert_eq!(orgs.put_membership(&m(u2, MemberRole::Admin)).await, Err(OrgStoreError::Conflict), "u2 is now the last Owner");
    assert_eq!(orgs.remove_membership(org.id, u2).await, Err(OrgStoreError::Conflict));

    assert_eq!(orgs.remove_membership(org.id, u4).await, Err(OrgStoreError::NotFound), "not a member");
    assert_eq!(orgs.remove_membership(org.id, u3).await, Ok(()));
    assert_eq!(orgs.membership(org.id, u3).await, Ok(None));
    assert_eq!(orgs.remove_membership(org.id, u3).await, Err(OrgStoreError::NotFound));

    let subjects: Vec<(FeedKind, String)> = [u2, u3, u3, u1, u2, u1, u3].iter().map(|u| (FeedKind::Member, u.to_string())).collect();
    assert_eq!(feed_of(&*fx.feed, org.id).await, subjects, "one Member entry per successful write");
}

async fn members_page_by_user_id_and_orgs_sort_by_slug(clock: &FakeClock, fx: &OrgFixture) {
    let orgs = &*fx.orgs;
    let owner = user(orgs, 5).await;
    let b = org_with_slug(clock, orgs, 1, "b-org", owner.id).await;
    let mut expected = vec![(owner.clone(), member(b.id, owner.id, MemberRole::Owner))];
    for n in [3, 1, 4, 2, 6] {
        let u = user(orgs, n).await;
        orgs.put_membership(&member(b.id, u.id, MemberRole::Member)).await.unwrap();
        expected.push((u.clone(), member(b.id, u.id, MemberRole::Member)));
    }
    expected.sort_by_key(|(u, _)| u.id);

    let mut page = PageRequest::first(2);
    let mut all = Vec::new();
    let mut pages = 0;
    loop {
        let got = orgs.members(b.id, page.clone()).await.expect("members");
        assert!(got.items.len() <= 2);
        pages += 1;
        all.extend(got.items);
        match got.next {
            Some(cursor) => page = page.after(cursor),
            None => break,
        }
    }
    assert_eq!(all, expected, "pages concatenate to every member ordered by user id");
    assert!(pages >= 3, "{pages} pages");
    assert!(matches!(orgs.members(b.id, bad_cursor(2)).await, Err(OrgStoreError::Invalid(_))));

    let u1 = user_id(1);
    let a = org_with_slug(clock, orgs, 2, "a-org", u1).await;
    let z = org_with_slug(clock, orgs, 3, "Z-org", u1).await;
    let listed = orgs.orgs_for_user(u1).await.unwrap();
    assert_eq!(
        listed,
        vec![(z.clone(), member(z.id, u1, MemberRole::Owner)), (a, member(org_id(2), u1, MemberRole::Owner)), (b, member(org_id(1), u1, MemberRole::Member))],
        "ordered by slug, bytewise"
    );
    assert_eq!(orgs.orgs_for_user(user_id(9)).await, Ok(vec![]));
}

async fn invites_are_unique_per_subject_until_expiry(clock: &FakeClock, fx: &OrgFixture) {
    let orgs = &*fx.orgs;
    let owner = user(orgs, 1).await;
    let org = new_org(clock, orgs, 1, owner.id).await;
    let t0 = clock.now();
    let inv1 = invite(org.id, 1, "300", MemberRole::Member, owner.id, t0, 14);
    assert_eq!(orgs.create_invite(&inv1).await, Ok(()));
    let dup = invite(org.id, 2, "300", MemberRole::Admin, owner.id, t0, 14);
    assert_eq!(orgs.create_invite(&dup).await, Err(OrgStoreError::Conflict), "an unexpired invite for the subject exists");
    let same_id = Invite { subject: "301".into(), ..inv1.clone() };
    assert_eq!(orgs.create_invite(&same_id).await, Err(OrgStoreError::Conflict), "invite id taken");
    let inv5 = Invite { id: invite_id(5), provider: "google".into(), ..inv1.clone() };
    assert_eq!(orgs.create_invite(&inv5).await, Ok(()), "the key is (provider, subject)");
    let inv3 = invite(org.id, 3, "301", MemberRole::Member, owner.id, t0, 1);
    assert_eq!(orgs.create_invite(&inv3).await, Ok(()));
    assert_eq!(orgs.invites(org.id).await, Ok(vec![inv1.clone(), inv3.clone(), inv5.clone()]), "oldest first, then by id");

    clock.advance(Duration::from_secs(2 * 86_400));
    assert_eq!(orgs.invites(org.id).await, Ok(vec![inv1.clone(), inv5.clone()]), "expired invites are not listed");
    let inv4 = invite(org.id, 4, "301", MemberRole::Admin, owner.id, clock.now(), 14);
    assert_eq!(orgs.create_invite(&inv4).await, Ok(()), "an expired invite is replaced");
    assert_eq!(orgs.invites(org.id).await, Ok(vec![inv1.clone(), inv5.clone(), inv4.clone()]));
    assert_eq!(orgs.withdraw_invite(org.id, inv3.id).await, Err(OrgStoreError::NotFound), "the replaced invite is gone");

    assert_eq!(orgs.withdraw_invite(org.id, inv1.id).await, Ok(()));
    assert_eq!(orgs.withdraw_invite(org.id, inv1.id).await, Err(OrgStoreError::NotFound));
    assert_eq!(orgs.withdraw_invite(org_id(2), inv5.id).await, Err(OrgStoreError::NotFound), "another org's invite");
    assert_eq!(orgs.invites(org.id).await, Ok(vec![inv5.clone(), inv4.clone()]));
    let subjects: Vec<(FeedKind, String)> = [inv1.id, inv5.id, inv3.id, inv4.id, inv1.id].iter().map(|i| (FeedKind::Member, i.to_string())).collect();
    assert_eq!(feed_of(&*fx.feed, org.id).await, subjects);
}

async fn accept_invites_converts_unexpired_ones(clock: &FakeClock, fx: &OrgFixture) {
    let orgs = &*fx.orgs;
    let owner = user(orgs, 1).await.id;
    let invitee = user(orgs, 2).await.id;
    let a = new_org(clock, orgs, 1, owner).await.id;
    let b = new_org(clock, orgs, 2, owner).await.id;
    let c = new_org(clock, orgs, 3, owner).await.id;
    let d = new_org(clock, orgs, 4, owner).await.id;
    let t0 = clock.now();
    let inv_a = invite(a, 1, "400", MemberRole::Admin, owner, t0, 14);
    let inv_b = invite(b, 2, "400", MemberRole::Member, owner, t0, 1);
    let inv_c = invite(c, 3, "400", MemberRole::Admin, owner, t0, 14);
    let inv_d = Invite { provider: "google".into(), ..invite(d, 4, "400", MemberRole::Admin, owner, t0, 14) };
    orgs.put_membership(&member(c, invitee, MemberRole::Member)).await.unwrap();
    for inv in [&inv_a, &inv_b, &inv_c, &inv_d] {
        orgs.create_invite(inv).await.expect("create_invite");
    }

    clock.advance(Duration::from_secs(2 * 86_400));
    assert_eq!(
        orgs.accept_invites("github", "400", invitee).await,
        Ok(vec![member(a, invitee, MemberRole::Admin), member(c, invitee, MemberRole::Member)]),
        "unexpired invites only; an existing membership keeps its role; ordered by org"
    );
    assert_eq!(orgs.membership(a, invitee).await, Ok(Some(member(a, invitee, MemberRole::Admin))));
    assert_eq!(orgs.membership(b, invitee).await, Ok(None), "the expired invite is skipped");
    assert_eq!(orgs.membership(c, invitee).await, Ok(Some(member(c, invitee, MemberRole::Member))));
    assert_eq!(orgs.membership(d, invitee).await, Ok(None), "other providers do not match");
    assert_eq!(orgs.invites(a).await, Ok(vec![]), "converted invites are deleted");
    assert_eq!(orgs.invites(c).await, Ok(vec![]));
    assert_eq!(orgs.invites(d).await, Ok(vec![inv_d.clone()]));
    assert_eq!(orgs.withdraw_invite(b, inv_b.id).await, Ok(()), "the expired invite was left in place");
    assert_eq!(orgs.accept_invites("github", "400", invitee).await, Ok(vec![]), "nothing left to accept");

    let m = |s: String| (FeedKind::Member, s);
    assert_eq!(feed_of(&*fx.feed, a).await, vec![m(inv_a.id.to_string()), m(invitee.to_string())]);
    assert_eq!(feed_of(&*fx.feed, c).await, vec![m(invitee.to_string()), m(inv_c.id.to_string()), m(invitee.to_string())]);
    assert_eq!(feed_of(&*fx.feed, d).await, vec![m(inv_d.id.to_string())]);
}

async fn integrations_bind_one_active_org(clock: &FakeClock, fx: &OrgFixture) {
    use IntegrationKind::{GitHub, GitHubFix, Linear, Slack};
    use IntegrationStatus::{Active, Revoked};
    let orgs = &*fx.orgs;
    let owner = user(orgs, 1).await.id;
    let a = new_org(clock, orgs, 1, owner).await.id;
    let b = new_org(clock, orgs, 2, owner).await.id;
    let resync = |org| NewJob::new(JobPayload::IntakeGithubResync { org_id: org });
    let kinds = [JobKind::IntakeGithubResync, JobKind::OpsPurge];

    assert_eq!(orgs.upsert_integration(&integration(a, GitHub, "inst-1", Active), &[resync(a)]).await, Ok(()));
    assert_eq!(leased(&*fx.jobs, &kinds).await, vec![JobKind::IntakeGithubResync], "upsert enqueues its jobs");
    assert_eq!(orgs.integration_settings(a, GitHub, "inst-1").await, Ok(IntegrationSettings::default()), "'{{}}' → Default");
    let settings = IntegrationSettings { github_account_login: Some("acme".into()), ..IntegrationSettings::default() };
    assert_eq!(orgs.set_integration_settings(a, GitHub, "inst-1", &settings).await, Ok(()));
    assert_eq!(orgs.integration_settings(a, GitHub, "inst-1").await, Ok(settings.clone()));

    let step = || clock.advance(Duration::from_secs(1));
    step();
    assert_eq!(orgs.upsert_integration(&integration(a, GitHub, "inst-1", Revoked), &[]).await, Ok(()));
    assert_eq!(orgs.integration_settings(a, GitHub, "inst-1").await, Ok(settings.clone()), "upsert keeps the settings");
    assert_eq!(orgs.integrations(a).await, Ok(vec![integration(a, GitHub, "inst-1", Revoked)]));
    let resolved = |org: OrgId, status: IntegrationStatus| -> Result<Option<Integration>, OrgStoreError> {
        Ok(Some(integration(org, GitHub, "inst-1", status)))
    };
    assert_eq!(orgs.integration_by_external(GitHub, "inst-1").await, resolved(a, Revoked), "a revoked row resolves when no active one exists");
    assert_eq!(orgs.integration_by_external(GitHubFix, "inst-1").await, Ok(None), "kind is part of the key; unbound → None");

    step();
    assert_eq!(orgs.upsert_integration(&integration(b, GitHub, "inst-1", Active), &[]).await, Ok(()), "revoked elsewhere is fine");
    step();
    assert_eq!(orgs.set_integration_settings(a, GitHub, "inst-1", &settings).await, Ok(()), "a's revoked row is now the latest update");
    assert_eq!(orgs.integration_by_external(GitHub, "inst-1").await, resolved(b, Active), "the active row wins over a later revoked one");
    assert_eq!(orgs.upsert_integration(&integration(a, GitHub, "inst-1", Active), &[]).await, Err(OrgStoreError::Conflict), "active in b");
    assert_eq!(orgs.set_integration_status(a, GitHub, "inst-1", Active, &[]).await, Err(OrgStoreError::Conflict));
    assert_eq!(orgs.integrations(a).await, Ok(vec![integration(a, GitHub, "inst-1", Revoked)]));

    step();
    assert_eq!(orgs.set_integration_status(b, GitHub, "inst-1", Revoked, &[resync(b)]).await, Ok(()));
    assert_eq!(leased(&*fx.jobs, &kinds).await, vec![JobKind::IntakeGithubResync], "set_integration_status enqueues its jobs");
    assert_eq!(orgs.integration_by_external(GitHub, "inst-1").await, resolved(b, Revoked), "the later-updated revoked row wins");
    step();
    assert_eq!(orgs.set_integration_settings(a, GitHub, "inst-1", &settings).await, Ok(()));
    assert_eq!(orgs.integration_by_external(GitHub, "inst-1").await, resolved(a, Revoked), "a settings write counts as an update");
    step();
    assert_eq!(orgs.set_integration_status(a, GitHub, "inst-1", Active, &[]).await, Ok(()));
    assert_eq!(orgs.integration_by_external(GitHub, "inst-1").await, resolved(a, Active));
    assert_eq!(orgs.set_integration_status(b, GitHub, "inst-1", Active, &[]).await, Err(OrgStoreError::Conflict));
    assert_eq!(orgs.set_integration_status(b, GitHub, "inst-9", Revoked, &[]).await, Err(OrgStoreError::NotFound));
    assert_eq!(
        orgs.set_integration_status(org_id(3), GitHub, "inst-1", Active, &[]).await,
        Err(OrgStoreError::NotFound),
        "a missing integration is NotFound even when the id is active elsewhere"
    );
    assert_eq!(orgs.integration_settings(a, Linear, "w-1").await, Err(OrgStoreError::NotFound));
    assert_eq!(orgs.set_integration_settings(a, Linear, "w-1", &settings).await, Err(OrgStoreError::NotFound));

    let bad = || NewJob { kind: JobKind::OpsPurge, ..resync(a) };
    let out = orgs.upsert_integration(&integration(a, GitHub, "inst-2", Active), &[bad()]).await;
    assert!(matches!(out, Err(OrgStoreError::Invalid(_))), "{out:?}");
    let out = orgs.set_integration_status(a, GitHub, "inst-1", Revoked, &[bad()]).await;
    assert!(matches!(out, Err(OrgStoreError::Invalid(_))), "{out:?}");
    assert_eq!(orgs.integrations(a).await, Ok(vec![integration(a, GitHub, "inst-1", Active)]), "invalid jobs abort the writes");
    assert_eq!(leased(&*fx.jobs, &kinds).await, vec![]);

    for (kind, id) in [(Slack, "T1"), (Linear, "w-1"), (GitHubFix, "fix-1"), (GitHub, "inst-0")] {
        orgs.upsert_integration(&integration(a, kind, id, Active), &[]).await.unwrap();
    }
    assert_eq!(
        orgs.integrations(a).await,
        Ok(vec![
            integration(a, GitHub, "inst-0", Active),
            integration(a, GitHub, "inst-1", Active),
            integration(a, GitHubFix, "fix-1", Active),
            integration(a, Linear, "w-1", Active),
            integration(a, Slack, "T1", Active),
        ]),
        "ordered by (kind, external id)"
    );
    let i = |s: &str| (FeedKind::Integration, s.to_owned());
    assert_eq!(
        feed_of(&*fx.feed, a).await,
        vec![
            // upsert, settings, revoke, settings, settings, reactivate
            i("github/inst-1"),
            i("github/inst-1"),
            i("github/inst-1"),
            i("github/inst-1"),
            i("github/inst-1"),
            i("github/inst-1"),
            i("slack/T1"),
            i("linear/w-1"),
            i("github_fix/fix-1"),
            i("github/inst-0")
        ]
    );
    assert_eq!(feed_of(&*fx.feed, b).await, vec![i("github/inst-1"), i("github/inst-1")]);
}

async fn intake_config_round_trips(clock: &FakeClock, fx: &OrgFixture) {
    let orgs = &*fx.orgs;
    let owner = user(orgs, 1).await.id;
    let org = new_org(clock, orgs, 1, owner).await.id;
    let other = new_org(clock, orgs, 2, owner).await.id;
    assert_eq!(orgs.intake_config(org).await, Ok(IntakeConfig::default()), "no row → Default");
    let mut config = IntakeConfig::default();
    config.linear.teams = vec!["ENG".into()];
    config.default_repo = Some("acme/web".into());
    config.routes = vec![RepoRoute {
        when: RouteWhen { team: Some("ENG".into()), label: None, project: Some("Billing".into()) },
        repo: "acme/billing".into(),
    }];
    config.environments.aliases.insert("prod".into(), "production".into());
    config.debounce.issue_secs = 30;
    assert_eq!(orgs.put_intake_config(org, &config).await, Ok(()));
    assert_eq!(orgs.intake_config(org).await, Ok(config));
    assert_eq!(orgs.intake_config(other).await, Ok(IntakeConfig::default()));
    assert_eq!(orgs.put_intake_config(org, &IntakeConfig::default()).await, Ok(()));
    assert_eq!(orgs.intake_config(org).await, Ok(IntakeConfig::default()), "a second put replaces the config");
    assert_eq!(feed_of(&*fx.feed, org).await, vec![(FeedKind::Settings, org.to_string()); 2]);
}

async fn customers_own_their_refs(clock: &FakeClock, fx: &OrgFixture) {
    let orgs = &*fx.orgs;
    let owner = user(orgs, 1).await.id;
    let org = new_org(clock, orgs, 1, owner).await.id;
    let label = ExternalRef::LinearLabel("customer:initech".into());
    let lc1 = ExternalRef::LinearCustomer("lc-1".into());
    let c1 = customer(org, 1, "Initech", vec![slack("T1", "C2"), label.clone(), lc1.clone(), lc1.clone()], Some("production"));
    assert_eq!(orgs.put_customer(&c1).await, Ok(()));
    let got = orgs.get_customer(org, c1.id).await.unwrap();
    assert_eq!(got, stored(&c1));
    assert_eq!(got.external_refs, vec![lc1.clone(), label.clone(), slack("T1", "C2")], "sorted by (system, value), deduplicated");

    let c2 = customer(org, 2, "Globex", vec![label.clone()], None);
    assert_eq!(orgs.put_customer(&c2).await, Err(OrgStoreError::Conflict), "a ref names at most one customer");
    assert_eq!(orgs.get_customer(org, c2.id).await, Err(OrgStoreError::NotFound), "the conflicting put wrote nothing");

    let c1b = Customer { name: "Initech Corp".into(), external_refs: vec![slack("T1", "C9"), label.clone()], environment: Some("staging".into()), ..c1 };
    assert_eq!(orgs.put_customer(&c1b).await, Ok(()), "put updates and replaces the refs");
    assert_eq!(orgs.get_customer(org, c1b.id).await, Ok(stored(&c1b)));
    let c2 = customer(org, 2, "Globex", vec![lc1.clone()], None);
    assert_eq!(orgs.put_customer(&c2).await, Ok(()), "lc-1 was released by the replace");

    let wanted = [lc1.clone(), slack("T1", "C9"), ExternalRef::LinearLabel("nobody".into())];
    assert_eq!(orgs.find_customers_by_refs(org, &wanted).await, Ok(vec![stored(&c1b), stored(&c2)]), "ordered by id");
    assert_eq!(orgs.find_customers_by_refs(org, &[slack("T1", "C2")]).await, Ok(vec![]), "removed refs no longer match");
    assert_eq!(orgs.find_customers_by_refs(org, &[]).await, Ok(vec![]));
    assert_eq!(orgs.get_customer(org, customer_id(9)).await, Err(OrgStoreError::NotFound));

    for n in [5, 3, 4] {
        orgs.put_customer(&customer(org, n, &format!("Customer {n}"), vec![], None)).await.unwrap();
    }
    let mut page = PageRequest::first(2);
    let mut ids = Vec::new();
    loop {
        let got = orgs.list_customers(org, page.clone()).await.expect("list_customers");
        assert!(got.items.len() <= 2);
        ids.extend(got.items.iter().map(|c| c.id));
        match got.next {
            Some(cursor) => page = page.after(cursor),
            None => break,
        }
    }
    assert_eq!(ids, (1..=5).map(customer_id).collect::<Vec<_>>(), "pages concatenate to every customer ordered by id");
    assert!(matches!(orgs.list_customers(org, bad_cursor(2)).await, Err(OrgStoreError::Invalid(_))));
    assert_eq!(feed_of(&*fx.feed, org).await, vec![], "customer writes are not in the feed");
}

async fn tenants_are_isolated(clock: &FakeClock, fx: &OrgFixture) {
    use IntegrationKind::GitHub;
    let orgs = &*fx.orgs;
    let owner_a = user(orgs, 1).await.id;
    let owner_b = user(orgs, 2).await;
    let member_a = user(orgs, 3).await.id;
    let a = new_org(clock, orgs, 1, owner_a).await.id;
    let b = new_org(clock, orgs, 2, owner_b.id).await.id;
    orgs.put_membership(&member(a, member_a, MemberRole::Admin)).await.unwrap();
    let inv = invite(a, 1, "500", MemberRole::Member, owner_a, clock.now(), 14);
    orgs.create_invite(&inv).await.unwrap();
    orgs.upsert_integration(&integration(a, GitHub, "inst-1", IntegrationStatus::Active), &[]).await.unwrap();
    let settings = IntegrationSettings { github_account_login: Some("acme".into()), ..IntegrationSettings::default() };
    orgs.set_integration_settings(a, GitHub, "inst-1", &settings).await.unwrap();
    orgs.set_triage_settings(a, &TriageSettings { auto_triage: false, ..TriageSettings::default() }).await.unwrap();
    let mut config = IntakeConfig::default();
    config.default_repo = Some("acme/web".into());
    orgs.put_intake_config(a, &config).await.unwrap();
    let lc1 = ExternalRef::LinearCustomer("lc-1".into());
    let ca = customer(a, 1, "Initech", vec![lc1.clone()], None);
    orgs.put_customer(&ca).await.unwrap();

    assert_eq!(orgs.membership(b, member_a).await, Ok(None));
    let members = orgs.members(b, PageRequest::first(50)).await.unwrap();
    assert_eq!(members.items, vec![(owner_b.clone(), member(b, owner_b.id, MemberRole::Owner))]);
    assert_eq!(orgs.remove_membership(b, member_a).await, Err(OrgStoreError::NotFound));
    assert_eq!(orgs.put_membership(&member(b, member_a, MemberRole::Member)).await, Ok(()));
    assert_eq!(orgs.membership(a, member_a).await, Ok(Some(member(a, member_a, MemberRole::Admin))), "memberships are per org");

    assert_eq!(orgs.invites(b).await, Ok(vec![]));
    assert_eq!(orgs.withdraw_invite(b, inv.id).await, Err(OrgStoreError::NotFound));
    assert_eq!(orgs.invites(a).await, Ok(vec![inv]));

    assert_eq!(orgs.integrations(b).await, Ok(vec![]));
    assert_eq!(orgs.integration_settings(b, GitHub, "inst-1").await, Err(OrgStoreError::NotFound));
    assert_eq!(orgs.set_integration_settings(b, GitHub, "inst-1", &IntegrationSettings::default()).await, Err(OrgStoreError::NotFound));
    assert_eq!(orgs.set_integration_status(b, GitHub, "inst-1", IntegrationStatus::Revoked, &[]).await, Err(OrgStoreError::NotFound));
    assert_eq!(orgs.integration_settings(a, GitHub, "inst-1").await, Ok(settings));

    assert_eq!(orgs.triage_settings(b).await, Ok(TriageSettings::default()));
    assert_eq!(orgs.intake_config(b).await, Ok(IntakeConfig::default()));

    assert_eq!(orgs.get_customer(b, ca.id).await, Err(OrgStoreError::NotFound));
    assert_eq!(orgs.find_customers_by_refs(b, std::slice::from_ref(&lc1)).await, Ok(vec![]));
    assert_eq!(orgs.list_customers(b, PageRequest::first(50)).await.unwrap().items, vec![]);
    let cb = customer(b, 1, "Initech (B)", vec![lc1.clone()], None);
    assert_eq!(orgs.put_customer(&cb).await, Ok(()), "customer ids and refs are unique per org");
    assert_eq!(orgs.get_customer(a, ca.id).await, Ok(ca));
    assert_eq!(orgs.get_customer(b, cb.id).await, Ok(cb));

    assert_eq!(feed_of(&*fx.feed, b).await, vec![(FeedKind::Member, member_a.to_string())], "b's feed holds only b's writes");
}

#[cfg(test)]
mod tests {
    use mage_core::testing::{MemJobQueue, SeqIdGen};

    use super::*;
    use crate::testing::{MemChangeFeed, MemOrgStore, NoSeed};

    #[tokio::test]
    async fn mem_org_store_passes_the_contract() {
        contract_org_store(|clock| async move {
            let clock: Arc<dyn Clock> = clock;
            let jobs = Arc::new(MemJobQueue::new(clock.clone(), Arc::new(SeqIdGen::new())));
            let feed = Arc::new(MemChangeFeed::new(clock.clone()));
            let orgs = Arc::new(MemOrgStore::new(clock, jobs.clone(), feed.clone()));
            OrgFixture { orgs, jobs, feed, seed: Arc::new(NoSeed) }
        })
        .await;
    }
}
