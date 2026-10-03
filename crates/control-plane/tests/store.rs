//! The same store contract against MemStore and, when `DATABASE_URL` is set, PgStore.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code: a failed unwrap is a failed test")]

use cloud_api::{SandboxSettings, SandboxState};
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

    // Sandboxes: one slug namespace with projects (deleted ones too), a per-org limit, the
    // environment replaced per name and gone with its sandbox.
    let sandbox = SandboxRec {
        slug: "dev".into(),
        org: "acme".into(),
        created_by: "u1".into(),
        created_at: 5,
        image: format!("aho.sandbox@sha256:{}", "c".repeat(64)),
        settings: SandboxSettings { egress: vec!["*.example.com".into()], port: Some(3000), auto_stop_hours: 8 },
        state: SandboxState::Starting,
        error: None,
        stops_at: Some(99),
        cpus: 2,
        mem_mib: 2048,
        disk_mib: 10240,
    };
    s.create_sandbox(&sandbox, 2).await.unwrap();
    assert_eq!(s.sandbox("dev").await.unwrap(), Some(sandbox.clone()));
    assert!(matches!(s.create_sandbox(&sandbox, 2).await, Err(StoreError::Conflict(_))));
    for taken in ["shop", "blog"] {
        let clash = SandboxRec { slug: taken.into(), ..sandbox.clone() };
        assert!(matches!(s.create_sandbox(&clash, 2).await, Err(StoreError::Conflict(_))), "project slug {taken}");
    }
    let project = ProjectRec { slug: "dev".into(), org: "acme".into(), created_at: 1, deleted_at: None };
    assert!(matches!(s.create_project(&project).await, Err(StoreError::Conflict(_))), "a sandbox slug is not a project's");
    s.create_sandbox(&SandboxRec { slug: "web".into(), ..sandbox.clone() }, 2).await.unwrap();
    assert!(matches!(s.create_sandbox(&SandboxRec { slug: "api".into(), ..sandbox.clone() }, 2).await, Err(StoreError::Limit(_))));
    assert!(s.sandbox("api").await.unwrap().is_none());
    let org2 = OrgRec { slug: "beta".into(), name: "Beta".into(), policy: ReleasePolicy::default(), policy_approver: None };
    s.create_org(&org2, "u2").await.unwrap();
    s.create_sandbox(&SandboxRec { slug: "api".into(), org: "beta".into(), created_by: "u2".into(), ..sandbox.clone() }, 2).await.unwrap();
    assert_eq!(s.org_sandboxes("acme").await.unwrap().iter().map(|x| x.slug.as_str()).collect::<Vec<_>>(), ["dev", "web"]);
    assert_eq!(s.sandboxes().await.unwrap().iter().map(|x| x.slug.as_str()).collect::<Vec<_>>(), ["api", "dev", "web"]);

    let running = SandboxRec {
        state: SandboxState::Failed,
        error: Some("oom".into()),
        stops_at: None,
        settings: SandboxSettings { egress: Vec::new(), port: None, auto_stop_hours: 1 },
        image: format!("aho.sandbox@sha256:{}", "d".repeat(64)),
        ..sandbox.clone()
    };
    s.update_sandbox(&running).await.unwrap();
    assert_eq!(s.sandbox("dev").await.unwrap(), Some(running.clone()));
    s.update_sandbox(&SandboxRec { slug: "ghost".into(), ..running.clone() }).await.unwrap();
    assert!(s.sandbox("ghost").await.unwrap().is_none(), "an update never creates");

    let var = EnvVarRec { sandbox: "dev".into(), name: "B_KEY".into(), ciphertext: vec![1, 2], fingerprint: "f1".into(), updated_at: 1 };
    s.put_sandbox_env(&var).await.unwrap();
    s.put_sandbox_env(&EnvVarRec { name: "A_KEY".into(), ..var.clone() }).await.unwrap();
    s.put_sandbox_env(&EnvVarRec { ciphertext: vec![9], fingerprint: "f2".into(), ..var.clone() }).await.unwrap();
    s.put_sandbox_env(&EnvVarRec { sandbox: "web".into(), ..var.clone() }).await.unwrap();
    let env = s.sandbox_env("dev").await.unwrap();
    assert_eq!(env.iter().map(|v| (v.name.as_str(), v.fingerprint.as_str())).collect::<Vec<_>>(), [("A_KEY", "f1"), ("B_KEY", "f2")]);
    assert!(s.delete_sandbox_env("dev", "A_KEY").await.unwrap());
    assert!(!s.delete_sandbox_env("dev", "A_KEY").await.unwrap());
    s.delete_sandbox("dev").await.unwrap();
    assert!(s.sandbox("dev").await.unwrap().is_none());
    assert!(s.sandbox_env("dev").await.unwrap().is_empty(), "the environment goes with the sandbox");
    assert_eq!(s.sandbox_env("web").await.unwrap().len(), 1);
    s.create_sandbox(&sandbox, 2).await.unwrap();
    assert!(s.sandbox_env("dev").await.unwrap().is_empty(), "a new sandbox of the same slug starts clean");
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
