use cloud_contract::diff::{Category, Direction, diff, fits_within};
use cloud_contract::draft::{Draft, DraftError, Evidence, Strength};
use cloud_contract::edge::{EdgeConfig, EdgeConfigError, Mode};
use cloud_contract::ledger::Stage;
use cloud_contract::manifest::*;
use cloud_contract::policy::{OnCallAction, ReleasePolicy, StandingOrder};
use cloud_contract::principal::{ClaimsError, PrincipalClaims, audience, issuer};
use cloud_contract::{RefusalCode, validate};
use serde_json::json;

fn sample() -> Manifest {
    Manifest::from_json(include_str!("fixtures/web-api.json")).unwrap_or_else(|e| panic!("fixture: {e}"))
}

fn codes(m: &Manifest) -> Vec<RefusalCode> {
    validate(m).err().unwrap_or_default().into_iter().map(|r| r.code).collect()
}

#[test]
fn fixture_is_accepted() {
    assert_eq!(validate(&sample()), Ok(()));
}

#[test]
fn unknown_fields_are_rejected_at_parse() {
    let mut v: serde_json::Value = serde_json::from_str(include_str!("fixtures/web-api.json")).unwrap_or_default();
    v["replicas"] = json!(3);
    assert!(serde_json::from_value::<Manifest>(v).is_err());
}

#[test]
fn every_refusal_is_reported_at_once() {
    let mut m = sample();
    m.api_version = "v0".into();
    m.project = "Acme".into();
    m.budget.ceiling_usd_month = 0;
    m.owner.email = "nobody".into();
    let got = codes(&m);
    for want in [RefusalCode::ApiVersion, RefusalCode::ProjectSlug, RefusalCode::BudgetZero, RefusalCode::OwnerEmail] {
        assert!(got.contains(&want), "{want:?} missing from {got:?}");
    }
}

#[test]
fn prebuilt_goes_with_oci_only() {
    let mut m = sample();
    m.build = Build::Prebuilt;
    assert_eq!(codes(&m), vec![RefusalCode::SourceBuildMismatch]);
    m.source = Source::Oci { digest: format!("sha256:{}", "a".repeat(64)) };
    assert_eq!(validate(&m), Ok(()));
    m.source = Source::Oci { digest: "latest".into() };
    assert_eq!(codes(&m), vec![RefusalCode::SourceDigest]);
}

#[test]
fn branch_names_are_not_commits() {
    let mut m = sample();
    m.source = Source::Git { repo: "r".into(), commit: "main".into() };
    assert_eq!(codes(&m), vec![RefusalCode::SourceDigest]);
}

#[test]
fn public_process_needs_a_port_and_health_needs_a_target() {
    let mut m = sample();
    m.processes[0].port = None;
    let got = codes(&m);
    assert!(got.contains(&RefusalCode::PublicWithoutPort));
    assert!(got.contains(&RefusalCode::HealthPath));
}

#[test]
fn duplicate_process_names_are_refused() {
    let mut m = sample();
    let dup = m.processes[0].clone();
    m.processes.push(dup);
    assert_eq!(codes(&m), vec![RefusalCode::DuplicateProcess]);
}

#[test]
fn secret_reach_must_be_inside_egress() {
    let mut m = sample();
    m.secrets[0].reach.push("evil.example.com".into());
    let refusals = validate(&m).err().unwrap_or_default();
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0].code, RefusalCode::SecretReachNotInEgress);
    assert_eq!(refusals[0].field, "/secrets/0/reach/1");
}

#[test]
fn wildcard_egress_covers_subdomains_only() {
    use cloud_contract::validate::host_covered;
    assert!(host_covered("*.sentry.io", "o1.ingest.sentry.io"));
    assert!(host_covered("*.sentry.io", "a.sentry.io"));
    assert!(!host_covered("*.sentry.io", "sentry.io"));
    assert!(!host_covered("*.sentry.io", "evilsentry.io"));
    assert!(!host_covered("api.stripe.com", "api.stripe.com.evil.io"));
}

#[test]
fn secret_names_and_dates_are_checked() {
    let mut m = sample();
    m.secrets[0].name = "stripe-key".into();
    m.secrets[1].expires = Some("31/01/2027".into());
    let got = codes(&m);
    assert!(got.contains(&RefusalCode::SecretName));
    assert!(got.contains(&RefusalCode::SecretExpires));
}

#[test]
fn regulated_data_is_refused_and_pii_needs_a_region() {
    let mut m = sample();
    m.data_class = DataClass::Payments;
    assert_eq!(codes(&m), vec![RefusalCode::DataClassUnsupported]);
    m.data_class = DataClass::Pii;
    m.region = None;
    assert_eq!(codes(&m), vec![RefusalCode::RegionRequired]);
    m.data_class = DataClass::None;
    assert_eq!(validate(&m), Ok(()));
}

#[test]
fn hostnames_reject_schemes_uppercase_and_single_labels() {
    use cloud_contract::validate::is_hostname;
    assert!(is_hostname("shop.acme.com", false));
    assert!(!is_hostname("https://shop.acme.com", false));
    assert!(!is_hostname("Shop.acme.com", false));
    assert!(!is_hostname("localhost", false));
    assert!(!is_hostname("*.acme.com", false));
    assert!(is_hostname("*.acme.com", true));
    assert!(!is_hostname("-a.acme.com", false));
}

#[test]
fn custom_oidc_needs_https_issuer() {
    let mut m = sample();
    m.auth = Auth::Oidc { provider: OidcProvider::Custom { issuer: "http://idp".into(), client_id: "x".into() } };
    assert_eq!(codes(&m), vec![RefusalCode::OidcIssuer]);
}

#[test]
fn hash_ignores_key_order_but_not_content() {
    let a = sample();
    let mut v: serde_json::Value = serde_json::to_value(&a).unwrap_or_default();
    // Round-trip through a differently ordered document.
    let reordered: Manifest = serde_json::from_str(&serde_json::to_string_pretty(&v).unwrap_or_default()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(a.hash(), reordered.hash());
    v["budget"]["ceiling_usd_month"] = json!(51);
    let changed: Manifest = serde_json::from_value(v).unwrap_or_else(|e| panic!("{e}"));
    assert_ne!(a.hash(), changed.hash());
    assert!(a.hash().starts_with("blake3:"));
}

// ---- diff -----------------------------------------------------------------------------------

#[test]
fn first_release_widens_everything_it_declares() {
    let d = diff(None, &sample());
    let widened = d.widened();
    for c in
        [Category::Process, Category::Exposure, Category::Egress, Category::Secret, Category::Domain, Category::Auth, Category::DataClass]
    {
        assert!(widened.contains(&c), "{c:?}");
    }
}

#[test]
fn code_only_change_widens_nothing() {
    let a = sample();
    let mut b = a.clone();
    b.source = Source::Git { repo: "https://github.com/acme/shop".into(), commit: "f".repeat(40) };
    let d = diff(Some(&a), &b);
    assert!(!d.is_widening());
    assert_eq!(d.changes.len(), 1);
    assert_eq!(d.changes[0].category, Category::Code);
}

#[test]
fn identical_manifests_have_an_empty_diff() {
    assert!(diff(Some(&sample()), &sample()).is_empty());
}

#[test]
fn new_egress_host_and_secret_reach_widen_removals_narrow() {
    let a = sample();
    let mut b = a.clone();
    b.egress.push("api.openai.com".into());
    b.secrets[1].reach.push("api.openai.com".into());
    b.secrets.remove(0);
    let d = diff(Some(&a), &b);
    let dirs: Vec<(Category, Direction)> = d.changes.iter().map(|c| (c.category, c.direction)).collect();
    assert!(dirs.contains(&(Category::Egress, Direction::Widen)));
    assert!(dirs.contains(&(Category::SecretReach, Direction::Widen)));
    assert!(dirs.contains(&(Category::Secret, Direction::Narrow)));
    assert!(d.changes.iter().any(|c| c.detail == "egress + api.openai.com"));
}

#[test]
fn going_internal_narrows_going_public_widens() {
    let a = sample();
    let mut b = a.clone();
    b.processes[0].expose = Expose::Internal;
    let d = diff(Some(&a), &b);
    assert_eq!(d.changes.len(), 1);
    assert_eq!((d.changes[0].category, d.changes[0].direction), (Category::Exposure, Direction::Narrow));
    let back = diff(Some(&b), &a);
    assert_eq!((back.changes[0].category, back.changes[0].direction), (Category::Exposure, Direction::Widen));
}

#[test]
fn data_class_and_budget_follow_their_order() {
    let a = sample();
    let mut b = a.clone();
    b.data_class = DataClass::None;
    b.budget.ceiling_usd_month = 10;
    assert!(!diff(Some(&a), &b).is_widening());
    assert!(diff(Some(&b), &a).is_widening());
}

#[test]
fn dropping_auth_widens_adding_it_narrows() {
    let a = sample();
    let mut b = a.clone();
    b.auth = Auth::None;
    assert!(diff(Some(&a), &b).widened().contains(&Category::Auth));
    assert!(!diff(Some(&b), &a).is_widening());
}

#[test]
fn rollback_fits_only_when_it_widens_nothing_over_the_ceiling() {
    let live = sample();
    let mut older = live.clone();
    older.egress.retain(|h| h == "api.stripe.com");
    assert!(fits_within(&older, &live));
    let mut wider = live.clone();
    wider.egress.push("pastebin.com".into());
    assert!(!fits_within(&wider, &live));
}

// ---- policy ---------------------------------------------------------------------------------

#[test]
fn quorum_depends_on_what_widens() {
    let p = ReleasePolicy::default();
    let a = sample();
    let mut code = a.clone();
    code.source = Source::Git { repo: "r".into(), commit: "e".repeat(40) };
    let r = p.requirement(&diff(Some(&a), &code));
    assert_eq!((r.approvals, r.owner_required), (1, false));
    let mut egress = a.clone();
    egress.egress.push("api.openai.com".into());
    let r = p.requirement(&diff(Some(&a), &egress));
    assert_eq!((r.approvals, r.owner_required), (2, false));
    let mut domain = a.clone();
    domain.domains.push(Domain { hostname: "www.acme.com".into(), verification: DomainMode::Cname });
    assert!(p.requirement(&diff(Some(&a), &domain)).owner_required);
}

#[test]
fn zero_approval_policies_still_need_one_human() {
    let p = ReleasePolicy { non_widening: 0, widening: 0, owner_only: Default::default() };
    assert_eq!(p.requirement(&diff(Some(&sample()), &sample())).approvals, 1);
}

#[test]
fn standing_orders_permit_only_their_own_verb_within_bounds() {
    let scale = StandingOrder::Scale { min: 1, max: 4 };
    assert!(scale.permits(&OnCallAction::Scale { replicas: 4 }));
    assert!(!scale.permits(&OnCallAction::Scale { replicas: 5 }));
    assert!(!scale.permits(&OnCallAction::Restart));
    assert!(StandingOrder::RollbackWithinCeiling.permits(&OnCallAction::Rollback { fits_within_ceiling: true }));
    assert!(!StandingOrder::RollbackWithinCeiling.permits(&OnCallAction::Rollback { fits_within_ceiling: false }));
}

// ---- edge.toml ------------------------------------------------------------------------------

const EDGE: &str = r#"
default = "edge"

[[route]]
path = "/webhooks/stripe"
mode = "public"
verify = { kind = "stripe", secret = "STRIPE_WEBHOOK_SECRET" }

[[route]]
path = "/api/**"
mode = "bearer"

[[route]]
path = "/assets/*"
mode = "public"
"#;

#[test]
fn first_matching_route_wins_and_default_covers_the_rest() {
    let cfg = EdgeConfig::parse(EDGE).unwrap_or_else(|e| panic!("{e}"));
    let routes = cfg.compile().unwrap_or_else(|e| panic!("{e}"));
    let hook = routes.resolve("/webhooks/stripe?x=1");
    assert_eq!(hook.mode, Mode::Public);
    assert!(hook.verify.is_some());
    assert_eq!(routes.resolve("/api/v1/orders/9").mode, Mode::Bearer);
    assert_eq!(routes.resolve("/assets/app.js").mode, Mode::Public);
    // `*` does not cross segments.
    assert_eq!(routes.resolve("/assets/js/app.js").mode, Mode::Edge);
    assert_eq!(routes.resolve("/admin").route, None);
}

#[test]
fn verify_needs_public_mode_and_paths_are_absolute() {
    let bad = EdgeConfig::parse("[[route]]\npath = \"/x\"\nmode = \"edge\"\nverify = { kind = \"github\", secret = \"S\" }\n")
        .unwrap_or_default();
    assert!(matches!(bad.compile(), Err(EdgeConfigError::VerifyWithoutPublic { index: 0, .. })));
    let rel = EdgeConfig::parse("[[route]]\npath = \"x/*\"\nmode = \"public\"\n").unwrap_or_default();
    assert!(matches!(rel.compile(), Err(EdgeConfigError::Relative { .. })));
}

#[test]
fn edge_toml_is_checked_against_the_manifest() {
    let cfg = EdgeConfig::parse(EDGE).unwrap_or_default();
    assert_eq!(cfg.check_against(&sample()), Ok(()));
    let mut m = sample();
    m.secrets.retain(|s| s.name != "STRIPE_WEBHOOK_SECRET");
    m.auth = Auth::None;
    let errs = cfg.check_against(&m).err().unwrap_or_default();
    assert!(errs.iter().any(|e| matches!(e, EdgeConfigError::UnknownSecret { .. })));
    assert!(errs.iter().any(|e| matches!(e, EdgeConfigError::ModeNeedsAuth { mode: Mode::Edge, .. })));
}

#[test]
fn implied_edge_config_follows_auth() {
    let mut m = sample();
    assert_eq!(EdgeConfig::implied_by(&m).default, Mode::Edge);
    m.auth = Auth::AppOwned;
    assert_eq!(EdgeConfig::implied_by(&m).default, Mode::Passthrough);
}

// ---- principal ------------------------------------------------------------------------------

fn claims(now: u64) -> PrincipalClaims {
    PrincipalClaims {
        iss: issuer("acme-shop"),
        aud: audience("acme-shop", Stage::Live),
        sub: "google:123".into(),
        email: Some("u@acme.com".into()),
        email_verified: true,
        groups: vec![],
        amr: vec!["oidc".into()],
        sid: "s1".into(),
        iat: now,
        exp: now + 60,
    }
}

#[test]
fn claims_bind_project_stage_and_time() {
    let c = claims(1000);
    assert_eq!(c.check("acme-shop", Stage::Live, 1030), Ok(()));
    assert!(matches!(c.check("other", Stage::Live, 1030), Err(ClaimsError::Issuer(..))));
    assert!(matches!(c.check("acme-shop", Stage::Staging, 1030), Err(ClaimsError::Audience(..))));
    assert_eq!(c.check("acme-shop", Stage::Live, 1100), Err(ClaimsError::Expired));
    assert_eq!(c.check("acme-shop", Stage::Live, 900), Err(ClaimsError::NotYetValid));
    let mut long = claims(1000);
    long.exp = 5000;
    assert_eq!(long.check("acme-shop", Stage::Live, 1001), Err(ClaimsError::TooLong));
}

// ---- draft ----------------------------------------------------------------------------------

fn ev(uri: &str) -> Evidence {
    Evidence { uri: uri.into(), note: String::new() }
}

#[test]
fn draft_assembles_a_manifest_once_weak_fields_are_confirmed() {
    let full: serde_json::Value = serde_json::from_str(include_str!("fixtures/web-api.json")).unwrap_or_default();
    let mut d = Draft::default();
    let serde_json::Value::Object(top) = &full else { panic!("object") };
    for (k, v) in top {
        if k == "processes" {
            continue;
        }
        d.declare(&format!("/{k}"), v.clone());
    }
    d.offer("/processes/0/name", json!("web"), Strength::Chained, ev("aho-log:s#1"));
    d.offer("/processes/0/port", json!(8080), Strength::Observed, ev("net:flow"));
    d.offer("/processes/0/expose", json!("public"), Strength::Inferred, ev("file:Dockerfile:3"));
    d.declare("/processes/0/health", json!({"kind": "http", "path": "/healthz"}));

    match d.into_manifest() {
        Err(DraftError::Unconfirmed(fields)) => assert_eq!(fields, vec!["/processes/0/expose", "/processes/0/port"]),
        other => panic!("expected unconfirmed, got {other:?}"),
    }
    assert!(d.confirm("/processes/0/port"));
    assert!(d.confirm("/processes/0/expose"));
    assert!(!d.confirm("/nope"));
    let m = d.into_manifest().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(m.processes.len(), 1);
    assert_eq!(m.processes[0].port, Some(8080));
    assert_eq!(validate(&m), Ok(()));
}

#[test]
fn stronger_evidence_is_not_overwritten_by_weaker() {
    let mut d = Draft::default();
    d.offer("/egress/0", json!("api.stripe.com"), Strength::Chained, ev("a"));
    d.offer("/egress/0", json!("registry.npmjs.org"), Strength::Inferred, ev("b"));
    assert_eq!(d.fields["/egress/0"].value, json!("api.stripe.com"));
    d.offer("/egress/0", json!("api.stripe.com"), Strength::Chained, ev("c"));
    assert_eq!(d.fields["/egress/0"].evidence.len(), 2);
}

#[test]
fn declared_value_keeps_the_evidence_it_corrected() {
    let mut d = Draft::default();
    d.offer("/project", json!("shop"), Strength::Inferred, ev("file:package.json:2"));
    d.declare("/project", json!("acme-shop"));
    let f = &d.fields["/project"];
    assert_eq!((f.strength, f.confirmed, f.evidence.len()), (Strength::Declared, true, 1));
}

#[test]
fn conflicting_pointers_are_an_error() {
    let mut d = Draft::default();
    d.declare("/owner", json!("ana"));
    d.declare("/owner/email", json!("a@b.co"));
    assert!(matches!(d.to_value(), Err(DraftError::Conflict(..))));
    let mut bad = Draft::default();
    bad.declare("owner", json!(1));
    assert!(matches!(bad.to_value(), Err(DraftError::Pointer(_))));
}
