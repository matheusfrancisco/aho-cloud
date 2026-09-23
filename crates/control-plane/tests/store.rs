//! The same store contract against MemStore and, when `DATABASE_URL` is set, PgStore.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code: a failed unwrap is a failed test")]

use cloud_contract::ledger::{Approver, ProofMethod, Stage};
use cloud_contract::policy::ReleasePolicy;
use cloud_control_plane::store::*;

fn user(id: &str, email: &str) -> UserRec {
    UserRec { id: id.into(), email: email.into(), name: id.into() }
}

async fn contract(s: &dyn Store) {
    // Users: unique id and email.
    s.create_user(&user("u1", "a@x.io")).await.unwrap();
    s.create_user(&user("u2", "b@x.io")).await.unwrap();
    assert!(matches!(s.create_user(&user("u3", "a@x.io")).await, Err(StoreError::Conflict(_))));
    assert_eq!(s.user_by_email("a@x.io").await.unwrap().unwrap().id, "u1");
    assert!(s.user("nobody").await.unwrap().is_none());

    // Device codes: approved once, redeemed once.
    let code = DeviceCodeRec {
        code_hash: "h1".into(),
        user_code: "BCDF-GHJK".into(),
        device_name: "laptop".into(),
        public_key: "pk".into(),
        expires_at: 100,
        approved_by: None,
    };
    s.put_device_code(&code).await.unwrap();
    assert!(
        matches!(s.put_device_code(&DeviceCodeRec { code_hash: "h2".into(), ..code.clone() }).await, Err(StoreError::Conflict(_))),
        "user codes are unique"
    );
    assert_eq!(s.device_code_by_user_code("BCDF-GHJK").await.unwrap().unwrap().code_hash, "h1");
    assert!(s.approve_device_code("h1", "u1").await.unwrap());
    assert!(!s.approve_device_code("h1", "u2").await.unwrap());
    assert_eq!(s.device_code("h1").await.unwrap().unwrap().approved_by.as_deref(), Some("u1"));
    assert!(s.take_device_code("h1").await.unwrap().is_some());
    assert!(s.take_device_code("h1").await.unwrap().is_none());

    // Tokens and keys.
    s.put_token(&TokenRec { hash: "t1".into(), user_id: "u1".into(), expires_at: 50 }).await.unwrap();
    assert_eq!(s.token("t1").await.unwrap().unwrap().expires_at, 50);
    let key = DeviceKeyRec { key_id: "dk1".into(), user_id: "u1".into(), name: "laptop".into(), public_key: "pk".into(), created_at: 1 };
    s.put_device_key(&key).await.unwrap();
    assert!(matches!(s.put_device_key(&key).await, Err(StoreError::Conflict(_))));
    assert_eq!(s.device_keys_for_user("u1").await.unwrap(), vec![key]);

    // Passkeys: unique credential, compare-and-set counter.
    let pk = PasskeyRec { credential_id: "c1".into(), user_id: "u1".into(), public_key: vec![4, 1, 2], sign_count: 5, created_at: 1 };
    s.put_passkey(&pk).await.unwrap();
    assert!(matches!(s.put_passkey(&PasskeyRec { user_id: "u2".into(), ..pk.clone() }).await, Err(StoreError::Conflict(_))));
    assert!(s.update_sign_count("c1", 5, 6).await.unwrap());
    assert!(!s.update_sign_count("c1", 5, 7).await.unwrap(), "stale counter");
    assert_eq!(s.passkey("c1").await.unwrap().unwrap().sign_count, 6);
    assert_eq!(s.passkeys_for_user("u1").await.unwrap().len(), 1);

    // Ceremonies are consumed.
    let c = CeremonyRec {
        id: "cer".into(),
        kind: CeremonyKind::Login,
        challenge: vec![1; 32],
        user_id: "u1".into(),
        email: "a@x.io".into(),
        name: String::new(),
        new_user: false,
        expires_at: 9,
    };
    s.put_ceremony(&c).await.unwrap();
    assert_eq!(s.take_ceremony("cer").await.unwrap(), Some(c));
    assert!(s.take_ceremony("cer").await.unwrap().is_none());

    // Orgs and members.
    let org = OrgRec { slug: "acme".into(), name: "Acme".into(), policy: ReleasePolicy::default(), policy_approver: None };
    s.create_org(&org, "u1").await.unwrap();
    assert!(matches!(s.create_org(&org, "u2").await, Err(StoreError::Conflict(_))));
    assert!(s.member("acme", "u1").await.unwrap().unwrap().owner);
    s.put_member(&MemberRec { org: "acme".into(), user_id: "u2".into(), owner: false }).await.unwrap();
    s.put_member(&MemberRec { org: "acme".into(), user_id: "u2".into(), owner: true }).await.unwrap();
    assert!(s.member("acme", "u2").await.unwrap().unwrap().owner, "upsert");
    assert_eq!(s.members("acme").await.unwrap().len(), 2);
    assert_eq!(s.memberships("u2").await.unwrap().len(), 1);
    let approver =
        Approver { user_id: "u1".into(), email: "a@x.io".into(), owner: true, method: ProofMethod::DeviceKey, signature: "sig".into() };
    let policy = ReleasePolicy { non_widening: 2, ..ReleasePolicy::default() };
    s.set_org_policy("acme", &policy, &approver).await.unwrap();
    let stored = s.org("acme").await.unwrap().unwrap();
    assert_eq!((stored.policy, stored.policy_approver), (policy, Some(approver)));

    // Projects: slugs retire on delete.
    let p = ProjectRec { slug: "shop".into(), org: "acme".into(), created_at: 1, deleted_at: None };
    s.create_project(&p).await.unwrap();
    s.delete_project("shop", 7).await.unwrap();
    s.delete_project("shop", 9).await.unwrap();
    assert_eq!(s.project("shop").await.unwrap().unwrap().deleted_at, Some(7), "first deletion time sticks");
    assert!(matches!(s.create_project(&p).await, Err(StoreError::Conflict(_))));
    s.create_project(&ProjectRec { slug: "blog".into(), ..p.clone() }).await.unwrap();
    assert_eq!(s.projects().await.unwrap().len(), 2);

    // Secrets replace per (project, name, stage).
    let secret = SecretRec {
        project: "blog".into(),
        name: "K".into(),
        stage: Stage::Live,
        ciphertext: vec![1, 2, 3],
        fingerprint: "f1".into(),
        edge_phc: None,
        updated_at: 1,
    };
    s.put_secret(&secret).await.unwrap();
    s.put_secret(&SecretRec { ciphertext: vec![9], fingerprint: "f2".into(), ..secret.clone() }).await.unwrap();
    s.put_secret(&SecretRec { stage: Stage::Staging, ..secret.clone() }).await.unwrap();
    let all = s.secrets("blog").await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all.iter().find(|x| x.stage == Stage::Live).unwrap().fingerprint, "f2");

    // Uploads.
    s.put_blob(&BlobRec { project: "blog".into(), hash: "blake3:aa".into(), kind: BlobKind::Archive, size: 3, created_at: 1 })
        .await
        .unwrap();
    assert!(s.archive_exists("blake3:aa").await.unwrap());
    assert!(!s.archive_exists("blake3:bb").await.unwrap());
    assert!(s.blob("shop", "blake3:aa").await.unwrap().is_none(), "per project");
    let sess = SessionRec {
        project: "blog".into(),
        session_id: "s1".into(),
        head_hash: "h".into(),
        blob_hash: "blake3:aa".into(),
        entries: 2,
        created_at: 1,
    };
    s.put_session(&sess).await.unwrap();
    s.put_session(&SessionRec { head_hash: "h2".into(), ..sess.clone() }).await.unwrap();
    assert_eq!(s.session("blog", "s1", "h").await.unwrap(), Some(sess));
    assert!(s.session("blog", "s1", "zz").await.unwrap().is_none());
    assert_eq!(s.sessions("blog").await.unwrap().len(), 2);

    // Release meta and kv.
    let meta = ReleaseMeta {
        project: "blog".into(),
        release: "r1".into(),
        edge_toml: None,
        build: BuildState::Pending,
        build_log: String::new(),
        created_at: 1,
    };
    s.put_release_meta(&meta).await.unwrap();
    s.put_release_meta(&ReleaseMeta { build: BuildState::Failed { reason: "x".into() }, ..meta.clone() }).await.unwrap();
    assert_eq!(s.release_meta("blog", "r1").await.unwrap().unwrap().build, BuildState::Failed { reason: "x".into() });
    s.kv_put("k", "v1").await.unwrap();
    s.kv_put("k", "v2").await.unwrap();
    assert_eq!(s.kv_get("k").await.unwrap().as_deref(), Some("v2"));
    assert!(s.kv_get("missing").await.unwrap().is_none());
}

#[tokio::test]
async fn mem_store_keeps_the_contract() {
    contract(&MemStore::new()).await;
}

#[tokio::test]
async fn pg_store_keeps_the_contract() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping pg_store_keeps_the_contract: DATABASE_URL is not set");
        return;
    };
    let schema = format!("cp_test_{}", uuid::Uuid::new_v4().simple());
    let store = PgStore::connect_schema(&url, &schema).await.unwrap();
    contract(&store).await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE"))).execute(store.pool()).await.unwrap();
}
