//! `SecretStore` contract (storage.md "Testing" → `SecretStore`: version sequencing, single
//! `Active`, `Destroyed` wipes columns, `rewrap` CAS; plus the state rules of server.md
//! `SecretVault`, `list_wraps` paging across secrets and data keys, and scope isolation).

use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{OrgId, UserId};
use mage_core::ports::clock::Clock;
use mage_core::testing::FakeClock;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::ports::audit::Actor;
use crate::ports::secrets::{DataKeyState, SealedDataKey, SealedSecret, SecretStore, SecretStoreError, WrapInfo, WrapRef};
use crate::ports::vault::{SecretName, SecretScope, SecretState, SecretVersion};
use crate::testing::seed::Seed;

pub struct SecretFixture {
    pub secrets: Arc<dyn SecretStore>,
    pub seed: Arc<dyn Seed>,
}

/// Runs every `SecretStore` scenario, each against a fresh fixture from `make`.
pub async fn contract_secret_store<F, Fut>(make: F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = SecretFixture>,
{
    macro_rules! scenario {
        ($name:ident) => {{
            let clock = Arc::new(FakeClock::new());
            let fixture = make(clock.clone()).await;
            fixture.seed.org(org(1)).await;
            fixture.seed.org(org(2)).await;
            $name(&clock, &*fixture.secrets).await;
        }};
    }
    scenario!(versions_are_sequenced_from_one_without_gaps);
    scenario!(one_active_version_per_name);
    scenario!(state_changes_follow_can_become);
    scenario!(destroyed_wipes_dek_and_ciphertext);
    scenario!(scopes_are_isolated);
    scenario!(list_secrets_orders_by_name_bytes_then_version);
    scenario!(data_keys_have_one_active_version);
    scenario!(list_wraps_pages_across_secrets_and_data_keys);
    scenario!(rewrap_is_a_cas_on_the_kek_version);
    scenario!(destroy_org_deletes_every_row_of_that_org);
}

/// `org(1) < org(2)` in uuid (byte) order.
fn org(n: u128) -> OrgId {
    OrgId::from_uuid(Uuid::from_u128(0x0f00 + n))
}

fn org_scope(n: u128) -> SecretScope {
    SecretScope::Org(org(n))
}

fn name(s: &str) -> SecretName {
    SecretName::new(s).expect("secret name")
}

fn v(n: u32) -> SecretVersion {
    SecretVersion(n)
}

fn sealed(scope: SecretScope, secret: &str, version: u32, state: SecretState, at: OffsetDateTime) -> SealedSecret {
    let tag = version as u8;
    SealedSecret {
        scope,
        name: name(secret),
        version: v(version),
        state,
        wrapped_dek: Some(vec![tag; 40]),
        kek_version: "kek-1".into(),
        nonce: [tag; 12],
        ciphertext: Some(format!("{scope}|{secret}|{version}").into_bytes()),
        fingerprint: format!("…{version:04}"),
        created_by: Actor::User(UserId::from_uuid(Uuid::from_u128(0x0b01))),
        created_at: at,
        state_changed_at: at,
    }
}

fn data_key(org: OrgId, version: u32, state: DataKeyState, at: OffsetDateTime) -> SealedDataKey {
    SealedDataKey { org, version, wrapped_dek: Some(vec![version as u8; 40]), kek_version: "kek-1".into(), state, created_at: at }
}

fn secret_ref(scope: SecretScope, secret: &str, version: u32) -> WrapRef {
    WrapRef::Secret { scope, name: name(secret), version: v(version) }
}

fn wrap(r: WrapRef) -> WrapInfo {
    WrapInfo { r, kek_version: "kek-1".into() }
}

fn invalid<T: std::fmt::Debug>(result: Result<T, SecretStoreError>) -> bool {
    matches!(result, Err(SecretStoreError::Invalid(_)))
}

async fn insert_all(secrets: &dyn SecretStore, rows: &[&SealedSecret]) {
    for row in rows {
        secrets.insert_secret(row).await.unwrap_or_else(|e| panic!("insert {} v{}: {e}", row.name, row.version));
    }
}

async fn versions_are_sequenced_from_one_without_gaps(clock: &FakeClock, secrets: &dyn SecretStore) {
    let p = SecretScope::Platform;
    let n = "auth/csrf_key";
    let t0 = clock.now();
    assert_eq!(secrets.next_secret_version(&p, &name(n)).await, Ok(v(1)));
    assert_eq!(
        secrets.insert_secret(&sealed(p, n, 2, SecretState::Active, t0)).await,
        Err(SecretStoreError::Conflict),
        "a new name starts at 1"
    );

    let v1 = sealed(p, n, 1, SecretState::Active, t0);
    secrets.insert_secret(&v1).await.expect("insert v1");
    assert_eq!(secrets.next_secret_version(&p, &name(n)).await, Ok(v(2)));
    assert_eq!(secrets.insert_secret(&v1).await, Err(SecretStoreError::Conflict), "an existing version");
    assert_eq!(secrets.insert_secret(&sealed(p, n, 3, SecretState::Staged, t0)).await, Err(SecretStoreError::Conflict), "no gaps");
    let v2 = sealed(p, n, 2, SecretState::Staged, t0);
    secrets.insert_secret(&v2).await.expect("insert v2");
    assert_eq!(secrets.next_secret_version(&p, &name(n)).await, Ok(v(3)));

    let v3 = sealed(p, n, 3, SecretState::Staged, t0);
    assert!(invalid(
        secrets.insert_secret(&SealedSecret { state: SecretState::Destroyed, wrapped_dek: None, ciphertext: None, ..v3.clone() }).await
    ));
    assert!(invalid(secrets.insert_secret(&SealedSecret { wrapped_dek: None, ..v3.clone() }).await), "a live version needs its DEK");
    assert!(invalid(secrets.insert_secret(&SealedSecret { ciphertext: None, ..v3.clone() }).await), "a live version needs its ciphertext");
    assert!(invalid(secrets.insert_secret(&sealed(p, "auth/other", 0, SecretState::Staged, t0)).await), "versions start at 1");
    assert_eq!(secrets.next_secret_version(&p, &name(n)).await, Ok(v(3)), "invalid rows write nothing");
    assert_eq!(secrets.next_secret_version(&p, &name("auth/other")).await, Ok(v(1)));

    assert_eq!(secrets.secret_versions(&p, &name(n)).await, Ok(vec![v1.clone(), v2.clone()]), "oldest first, stored as given");
    assert_eq!(secrets.secret_version(&p, &name(n), v(2)).await, Ok(v2));
    assert_eq!(secrets.secret_version(&p, &name(n), v(3)).await, Err(SecretStoreError::NotFound));
    assert_eq!(secrets.secret_version(&p, &name("auth/other"), v(1)).await, Err(SecretStoreError::NotFound));
    assert_eq!(secrets.secret_versions(&p, &name("auth/other")).await, Ok(vec![]));
    assert_eq!(secrets.active_secret(&p, &name("auth/other")).await, Ok(None));
    assert_eq!(secrets.active_secret(&p, &name(n)).await, Ok(Some(v1)));
}

async fn one_active_version_per_name(clock: &FakeClock, secrets: &dyn SecretStore) {
    let a = org_scope(1);
    let n = "model/anthropic_api_key";
    let other = "slack/bot_token";
    let t0 = clock.now();
    let v1 = sealed(a, n, 1, SecretState::Active, t0);
    let other1 = sealed(a, other, 1, SecretState::Active, t0);
    insert_all(secrets, &[&v1, &other1]).await;

    clock.advance(Duration::from_secs(60));
    let t1 = clock.now();
    let v2 = sealed(a, n, 2, SecretState::Active, t1);
    secrets.insert_secret(&v2).await.expect("insert v2");
    assert_eq!(
        secrets.secret_version(&a, &name(n), v(1)).await,
        Ok(SealedSecret { state: SecretState::Retired, state_changed_at: t1, ..v1.clone() }),
        "inserting an Active version retires the previous one"
    );
    assert_eq!(secrets.active_secret(&a, &name(n)).await, Ok(Some(v2.clone())));

    clock.advance(Duration::from_secs(60));
    let v3 = sealed(a, n, 3, SecretState::Staged, clock.now());
    secrets.insert_secret(&v3).await.expect("insert staged v3");
    assert_eq!(secrets.active_secret(&a, &name(n)).await, Ok(Some(v2.clone())), "a staged version changes nothing");

    clock.advance(Duration::from_secs(60));
    let t3 = clock.now();
    let active3 = SealedSecret { state: SecretState::Active, state_changed_at: t3, ..v3.clone() };
    assert_eq!(secrets.set_secret_state(&a, &name(n), v(3), SecretState::Active).await, Ok(active3.clone()));
    assert_eq!(
        secrets.secret_versions(&a, &name(n)).await,
        Ok(vec![
            SealedSecret { state: SecretState::Retired, state_changed_at: t1, ..v1 },
            SealedSecret { state: SecretState::Retired, state_changed_at: t3, ..v2 },
            active3.clone(),
        ]),
        "Staged → Active retires the previous Active"
    );
    assert_eq!(secrets.active_secret(&a, &name(n)).await, Ok(Some(active3)));
    assert_eq!(secrets.active_secret(&a, &name(other)).await, Ok(Some(other1)), "other names keep their Active");
}

async fn state_changes_follow_can_become(clock: &FakeClock, secrets: &dyn SecretStore) {
    let p = SecretScope::Platform;
    let n = "github/oauth_client_secret";
    let s1 = sealed(p, n, 1, SecretState::Staged, clock.now());
    secrets.insert_secret(&s1).await.unwrap();
    clock.advance(Duration::from_secs(5));
    let key = name(n);
    let set = |state| secrets.set_secret_state(&p, &key, v(1), state);

    assert_eq!(secrets.set_secret_state(&p, &name(n), v(9), SecretState::Active).await, Err(SecretStoreError::NotFound));
    assert_eq!(secrets.set_secret_state(&p, &name("github/nope"), v(1), SecretState::Active).await, Err(SecretStoreError::NotFound));
    assert_eq!(set(SecretState::Staged).await, Ok(s1.clone()), "same state: no-op, state_changed_at untouched");
    assert!(invalid(set(SecretState::Retired).await), "Staged cannot become Retired");

    let t1 = clock.now();
    let active = SealedSecret { state: SecretState::Active, state_changed_at: t1, ..s1.clone() };
    assert_eq!(set(SecretState::Active).await, Ok(active.clone()));
    clock.advance(Duration::from_secs(5));
    assert_eq!(set(SecretState::Active).await, Ok(active.clone()), "same state: no-op");
    assert!(invalid(set(SecretState::Staged).await), "Active cannot go back to Staged");

    let t2 = clock.now();
    let retired = SealedSecret { state: SecretState::Retired, state_changed_at: t2, ..s1.clone() };
    assert_eq!(set(SecretState::Retired).await, Ok(retired.clone()));
    assert_eq!(secrets.active_secret(&p, &name(n)).await, Ok(None), "retiring the Active leaves none");
    assert!(invalid(set(SecretState::Active).await), "a Retired version never comes back");
    assert!(invalid(set(SecretState::Staged).await));
    assert_eq!(secrets.secret_version(&p, &name(n), v(1)).await, Ok(retired), "refused changes write nothing");

    clock.advance(Duration::from_secs(5));
    let destroyed = set(SecretState::Destroyed).await.expect("Retired → Destroyed");
    assert_eq!(destroyed.state, SecretState::Destroyed);
    for next in [SecretState::Staged, SecretState::Active, SecretState::Retired] {
        assert!(invalid(set(next).await), "Destroyed is terminal ({next})");
    }
    assert_eq!(set(SecretState::Destroyed).await, Ok(destroyed));
}

async fn destroyed_wipes_dek_and_ciphertext(clock: &FakeClock, secrets: &dyn SecretStore) {
    let a = org_scope(1);
    let n = "model/anthropic_api_key";
    let t0 = clock.now();
    let v1 = sealed(a, n, 1, SecretState::Active, t0);
    let v2 = sealed(a, n, 2, SecretState::Staged, t0);
    insert_all(secrets, &[&v1, &v2]).await;
    clock.advance(Duration::from_secs(60));
    let t1 = clock.now();

    let wiped1 = SealedSecret { state: SecretState::Destroyed, wrapped_dek: None, ciphertext: None, state_changed_at: t1, ..v1 };
    assert_eq!(
        secrets.set_secret_state(&a, &name(n), v(1), SecretState::Destroyed).await,
        Ok(wiped1.clone()),
        "emergency Active → Destroyed wipes the DEK and ciphertext and keeps the metadata"
    );
    assert_eq!(secrets.secret_version(&a, &name(n), v(1)).await, Ok(wiped1.clone()));
    assert_eq!(secrets.active_secret(&a, &name(n)).await, Ok(None));
    let wiped2 = SealedSecret { state: SecretState::Destroyed, wrapped_dek: None, ciphertext: None, state_changed_at: t1, ..v2 };
    assert_eq!(secrets.set_secret_state(&a, &name(n), v(2), SecretState::Destroyed).await, Ok(wiped2.clone()), "Staged → Destroyed");
    clock.advance(Duration::from_secs(60));
    assert_eq!(secrets.set_secret_state(&a, &name(n), v(1), SecretState::Destroyed).await, Ok(wiped1.clone()), "same state: no-op");

    assert_eq!(secrets.next_secret_version(&a, &name(n)).await, Ok(v(3)), "destroyed versions keep their numbers");
    let v3 = sealed(a, n, 3, SecretState::Active, clock.now());
    secrets.insert_secret(&v3).await.expect("insert v3");
    assert_eq!(secrets.secret_versions(&a, &name(n)).await, Ok(vec![wiped1, wiped2, v3]));
}

async fn scopes_are_isolated(clock: &FakeClock, secrets: &dyn SecretStore) {
    let (p, a, b) = (SecretScope::Platform, org_scope(1), org_scope(2));
    let n = "model/anthropic_api_key";
    let t0 = clock.now();
    let p1 = sealed(p, n, 1, SecretState::Active, t0);
    let a1 = sealed(a, n, 1, SecretState::Active, t0);
    let a2 = sealed(a, n, 2, SecretState::Staged, t0);
    insert_all(secrets, &[&p1, &a1, &a2]).await;

    assert_eq!(secrets.next_secret_version(&p, &name(n)).await, Ok(v(2)));
    assert_eq!(secrets.next_secret_version(&a, &name(n)).await, Ok(v(3)));
    assert_eq!(secrets.next_secret_version(&b, &name(n)).await, Ok(v(1)));
    assert_eq!(secrets.active_secret(&b, &name(n)).await, Ok(None));
    assert_eq!(secrets.secret_version(&b, &name(n), v(1)).await, Err(SecretStoreError::NotFound));
    assert_eq!(secrets.secret_versions(&b, &name(n)).await, Ok(vec![]));
    assert_eq!(secrets.list_secrets(&b).await, Ok(vec![]));
    assert_eq!(
        secrets.set_secret_state(&b, &name(n), v(2), SecretState::Active).await,
        Err(SecretStoreError::NotFound),
        "org B cannot touch org A's versions"
    );
    assert_eq!(secrets.secret_version(&a, &name(n), v(2)).await, Ok(a2.clone()));

    let b1 = sealed(b, n, 1, SecretState::Active, t0);
    secrets.insert_secret(&b1).await.expect("org B v1");
    assert_eq!(secrets.active_secret(&a, &name(n)).await, Ok(Some(a1.clone())), "an Active in org B retires nothing in org A");
    assert_eq!(secrets.active_secret(&p, &name(n)).await, Ok(Some(p1.clone())), "nor in the platform scope");

    clock.advance(Duration::from_secs(1));
    secrets.set_secret_state(&p, &name(n), v(1), SecretState::Retired).await.expect("retire platform v1");
    assert_eq!(secrets.active_secret(&p, &name(n)).await, Ok(None));
    assert_eq!(secrets.active_secret(&a, &name(n)).await, Ok(Some(a1.clone())));
    assert_eq!(secrets.list_secrets(&a).await, Ok(vec![a1, a2]));
    assert_eq!(secrets.list_secrets(&b).await, Ok(vec![b1]));
    assert_eq!(secrets.list_secrets(&p).await.unwrap().len(), 1);
}

async fn list_secrets_orders_by_name_bytes_then_version(clock: &FakeClock, secrets: &dyn SecretStore) {
    let a = org_scope(1);
    let t0 = clock.now();
    for (n, version) in [("b", 1), ("a_b", 1), ("a/z", 1), ("a/z", 2), ("a.b", 1), ("a-b", 1)] {
        secrets.insert_secret(&sealed(a, n, version, SecretState::Staged, t0)).await.unwrap();
    }
    secrets.insert_secret(&sealed(SecretScope::Platform, "a/a", 1, SecretState::Staged, t0)).await.unwrap();

    let listed: Vec<(String, u32)> =
        secrets.list_secrets(&a).await.unwrap().into_iter().map(|s| (s.name.as_str().to_owned(), s.version.0)).collect();
    let expected = [("a-b", 1), ("a.b", 1), ("a/z", 1), ("a/z", 2), ("a_b", 1), ("b", 1)];
    assert_eq!(listed, expected.map(|(n, version)| (n.to_owned(), version)), "byte order of names, then version; scope only");
}

async fn data_keys_have_one_active_version(clock: &FakeClock, secrets: &dyn SecretStore) {
    let t0 = clock.now();
    assert_eq!(secrets.active_data_key(org(1)).await, Ok(None));
    assert_eq!(secrets.data_key(org(1), 1).await, Err(SecretStoreError::NotFound));
    let k1 = data_key(org(1), 1, DataKeyState::Active, t0);
    secrets.insert_data_key(&k1).await.expect("insert k1");
    assert_eq!(secrets.active_data_key(org(1)).await, Ok(Some(k1.clone())));
    assert_eq!(secrets.insert_data_key(&k1).await, Err(SecretStoreError::Conflict), "an existing version");

    clock.advance(Duration::from_secs(60));
    let k2 = data_key(org(1), 2, DataKeyState::Active, clock.now());
    secrets.insert_data_key(&k2).await.expect("insert k2");
    let retired1 = SealedDataKey { state: DataKeyState::Retired, ..k1.clone() };
    assert_eq!(secrets.data_key(org(1), 1).await, Ok(retired1.clone()), "a new Active retires the previous one");
    assert_eq!(secrets.active_data_key(org(1)).await, Ok(Some(k2.clone())));

    assert_eq!(secrets.insert_data_key(&k1).await, Err(SecretStoreError::Conflict));
    assert_eq!(secrets.active_data_key(org(1)).await, Ok(Some(k2.clone())), "a refused Active insert retires nothing");
    assert_eq!(secrets.data_key(org(1), 1).await, Ok(retired1));

    let k3 = data_key(org(1), 3, DataKeyState::Retired, clock.now());
    secrets.insert_data_key(&k3).await.expect("insert retired k3");
    assert_eq!(secrets.active_data_key(org(1)).await, Ok(Some(k2.clone())));
    assert_eq!(secrets.data_key(org(1), 3).await, Ok(k3));

    assert!(invalid(secrets.insert_data_key(&data_key(org(1), 4, DataKeyState::Destroyed, t0)).await));
    assert!(invalid(secrets.insert_data_key(&SealedDataKey { wrapped_dek: None, ..data_key(org(1), 4, DataKeyState::Active, t0) }).await));
    assert_eq!(secrets.data_key(org(1), 4).await, Err(SecretStoreError::NotFound), "invalid keys write nothing");
    assert_eq!(secrets.active_data_key(org(1)).await, Ok(Some(k2.clone())));

    assert_eq!(secrets.active_data_key(org(2)).await, Ok(None), "org B sees none of org A's keys");
    assert_eq!(secrets.data_key(org(2), 2).await, Err(SecretStoreError::NotFound));
    let b1 = data_key(org(2), 1, DataKeyState::Active, t0);
    secrets.insert_data_key(&b1).await.expect("org B k1");
    assert_eq!(secrets.active_data_key(org(2)).await, Ok(Some(b1)));
    assert_eq!(secrets.active_data_key(org(1)).await, Ok(Some(k2)), "org B's Active retires nothing in org A");
}

async fn list_wraps_pages_across_secrets_and_data_keys(clock: &FakeClock, secrets: &dyn SecretStore) {
    let (p, a, b) = (SecretScope::Platform, org_scope(1), org_scope(2));
    let t0 = clock.now();
    // Inserted out of order; names chosen so byte order differs from natural-language collations.
    let rows = [
        sealed(b, "b/key", 1, SecretState::Active, t0),
        sealed(a, "a_b", 1, SecretState::Active, t0),
        sealed(a, "a/z", 1, SecretState::Active, t0),
        sealed(a, "a/z", 2, SecretState::Active, t0),
        sealed(a, "a/y", 1, SecretState::Staged, t0),
        sealed(p, "p/two", 1, SecretState::Staged, t0),
        sealed(p, "p/one", 1, SecretState::Active, t0),
    ];
    insert_all(secrets, &rows.iter().collect::<Vec<_>>()).await;
    secrets.set_secret_state(&a, &name("a/y"), v(1), SecretState::Destroyed).await.expect("destroy a/y");
    for key in [
        data_key(org(2), 1, DataKeyState::Active, t0),
        data_key(org(1), 1, DataKeyState::Active, t0),
        data_key(org(1), 2, DataKeyState::Active, t0),
    ] {
        secrets.insert_data_key(&key).await.expect("insert data key");
    }

    let expected: Vec<WrapInfo> = [
        secret_ref(p, "p/one", 1),
        secret_ref(p, "p/two", 1),
        secret_ref(a, "a/z", 1),
        secret_ref(a, "a/z", 2),
        secret_ref(a, "a_b", 1),
        secret_ref(b, "b/key", 1),
        WrapRef::DataKey { org: org(1), version: 1 },
        WrapRef::DataKey { org: org(1), version: 2 },
        WrapRef::DataKey { org: org(2), version: 1 },
    ]
    .into_iter()
    .map(wrap)
    .collect();
    assert_eq!(
        secrets.list_wraps(None, 100).await,
        Ok(expected.clone()),
        "platform secrets, then org secrets by (org, name bytes, version), then data keys; destroyed skipped, retired kept"
    );

    for limit in [1, 2, 4] {
        let mut pages = Vec::new();
        let mut after: Option<WrapRef> = None;
        loop {
            let page = secrets.list_wraps(after.as_ref(), limit).await.expect("list_wraps page");
            assert!(page.len() <= limit as usize);
            let done = page.len() < limit as usize;
            after = page.last().map(|w| w.r.clone()).or(after);
            pages.extend(page);
            if done {
                break;
            }
        }
        assert_eq!(pages, expected, "pages of {limit} concatenate to the full list");
    }
    assert_eq!(
        secrets.list_wraps(Some(&secret_ref(b, "b/key", 1)), 2).await,
        Ok(expected[6..8].to_vec()),
        "after the last secret come the data keys"
    );
    assert_eq!(secrets.list_wraps(Some(&WrapRef::DataKey { org: org(2), version: 1 }), 10).await, Ok(vec![]));
}

async fn rewrap_is_a_cas_on_the_kek_version(clock: &FakeClock, secrets: &dyn SecretStore) {
    let a = org_scope(1);
    let n = "model/anthropic_api_key";
    let t0 = clock.now();
    let s1 = sealed(a, n, 1, SecretState::Active, t0);
    secrets.insert_secret(&s1).await.unwrap();
    let k1 = data_key(org(1), 1, DataKeyState::Active, t0);
    secrets.insert_data_key(&k1).await.unwrap();
    clock.advance(Duration::from_secs(60));

    let r = secret_ref(a, n, 1);
    secrets.rewrap(&r, "kek-1", b"rewrapped-secret", "kek-2").await.expect("rewrap secret");
    let rewrapped = SealedSecret { wrapped_dek: Some(b"rewrapped-secret".to_vec()), kek_version: "kek-2".into(), ..s1 };
    assert_eq!(secrets.secret_version(&a, &name(n), v(1)).await, Ok(rewrapped.clone()), "only the DEK wrap and kek_version change");
    assert_eq!(secrets.rewrap(&r, "kek-1", b"stale", "kek-3").await, Err(SecretStoreError::Conflict), "a stale kek_version loses");
    assert_eq!(secrets.secret_version(&a, &name(n), v(1)).await, Ok(rewrapped));

    let dk = WrapRef::DataKey { org: org(1), version: 1 };
    secrets.rewrap(&dk, "kek-1", b"rewrapped-key", "kek-2").await.expect("rewrap data key");
    assert_eq!(
        secrets.data_key(org(1), 1).await,
        Ok(SealedDataKey { wrapped_dek: Some(b"rewrapped-key".to_vec()), kek_version: "kek-2".into(), ..k1 })
    );
    assert_eq!(secrets.rewrap(&dk, "kek-1", b"stale", "kek-3").await, Err(SecretStoreError::Conflict));
    assert_eq!(
        secrets.list_wraps(None, 10).await,
        Ok(vec![WrapInfo { r: r.clone(), kek_version: "kek-2".into() }, WrapInfo { r: dk, kek_version: "kek-2".into() }])
    );

    assert_eq!(secrets.rewrap(&secret_ref(a, n, 9), "kek-1", b"x", "kek-2").await, Err(SecretStoreError::NotFound));
    assert_eq!(
        secrets.rewrap(&secret_ref(org_scope(2), n, 1), "kek-2", b"x", "kek-3").await,
        Err(SecretStoreError::NotFound),
        "scope is part of the ref"
    );
    assert_eq!(
        secrets.rewrap(&WrapRef::DataKey { org: org(1), version: 9 }, "kek-1", b"x", "kek-2").await,
        Err(SecretStoreError::NotFound)
    );
    assert_eq!(
        secrets.rewrap(&WrapRef::DataKey { org: org(2), version: 1 }, "kek-2", b"x", "kek-3").await,
        Err(SecretStoreError::NotFound)
    );

    let p = SecretScope::Platform;
    secrets.insert_secret(&sealed(p, "auth/csrf_key", 1, SecretState::Staged, t0)).await.unwrap();
    secrets.set_secret_state(&p, &name("auth/csrf_key"), v(1), SecretState::Destroyed).await.unwrap();
    assert_eq!(
        secrets.rewrap(&secret_ref(p, "auth/csrf_key", 1), "kek-1", b"x", "kek-2").await,
        Err(SecretStoreError::NotFound),
        "a destroyed version has nothing to rewrap"
    );
}

async fn destroy_org_deletes_every_row_of_that_org(clock: &FakeClock, secrets: &dyn SecretStore) {
    let (p, a, b) = (SecretScope::Platform, org_scope(1), org_scope(2));
    let n = "model/anthropic_api_key";
    let t0 = clock.now();
    let p1 = sealed(p, n, 1, SecretState::Active, t0);
    let b1 = sealed(b, n, 1, SecretState::Active, t0);
    insert_all(secrets, &[&p1, &sealed(a, n, 1, SecretState::Active, t0), &sealed(a, n, 2, SecretState::Staged, t0), &b1]).await;
    for key in [
        data_key(org(1), 1, DataKeyState::Active, t0),
        data_key(org(1), 2, DataKeyState::Active, t0),
        data_key(org(2), 1, DataKeyState::Active, t0),
    ] {
        secrets.insert_data_key(&key).await.unwrap();
    }

    assert_eq!(secrets.destroy_org(org(1)).await, Ok(4), "two secret versions and two data keys");
    assert_eq!(secrets.list_secrets(&a).await, Ok(vec![]));
    assert_eq!(secrets.next_secret_version(&a, &name(n)).await, Ok(v(1)));
    assert_eq!(secrets.active_data_key(org(1)).await, Ok(None));
    assert_eq!(secrets.data_key(org(1), 1).await, Err(SecretStoreError::NotFound));
    assert_eq!(secrets.destroy_org(org(1)).await, Ok(0));

    assert_eq!(secrets.list_secrets(&b).await, Ok(vec![b1]));
    assert_eq!(secrets.list_secrets(&p).await, Ok(vec![p1]));
    assert_eq!(secrets.active_data_key(org(2)).await.unwrap().map(|k| k.version), Some(1));
    assert_eq!(
        secrets.list_wraps(None, 10).await.unwrap().into_iter().map(|w| w.r).collect::<Vec<_>>(),
        vec![secret_ref(p, n, 1), secret_ref(b, n, 1), WrapRef::DataKey { org: org(2), version: 1 }]
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{MemSecretStore, NoSeed};

    #[tokio::test]
    async fn mem_secret_store_passes_the_contract() {
        contract_secret_store(
            |clock| async move { SecretFixture { secrets: Arc::new(MemSecretStore::new(clock)), seed: Arc::new(NoSeed) } },
        )
        .await;
    }
}
