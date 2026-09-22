//! API views derived from ledger state: a release as the approval card shows it, a project.

use std::collections::BTreeMap;

use cloud_api::{ApprovalView, Project, Release, ReleaseStatus, StageHead, User};
use cloud_contract::ledger::Stage;
use cloud_contract::manifest::Manifest;
use cloud_contract::validate::host_covered;
use cloud_domains::Registry;
use cloud_ledger::LedgerState;

use crate::store::{BuildState, ReleaseMeta, UserRec};

/// Observed hosts the manifest's egress list does not cover (same rule as the gate's
/// `host_covered`), lowercased, sorted and deduplicated.
pub fn undeclared_egress(manifest: &Manifest, observed: &[String]) -> Vec<String> {
    let mut out: Vec<String> = observed
        .iter()
        .map(|h| h.trim().trim_end_matches('.').to_ascii_lowercase())
        .filter(|h| !manifest.egress.iter().any(|p| host_covered(p, h)))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The stage whose approvals a release is gathering next.
fn next_approval_stage(highest: Option<Stage>) -> Stage {
    match highest {
        None | Some(Stage::Preview) => Stage::Staging,
        Some(Stage::Staging) | Some(Stage::Live) => Stage::Live,
    }
}

/// The API view of release `id`, or `None` when the ledger has no such release.
pub fn release(
    project: &str,
    id: &str,
    state: &LedgerState,
    meta: Option<&ReleaseMeta>,
    users: &BTreeMap<String, UserRec>,
) -> Option<Release> {
    let rs = state.releases.get(id)?;
    let stage = rs.activations.iter().map(|(s, _)| *s).max();
    let active_now = state.active.values().any(|r| r == id);
    let status = if rs.rejected.is_some() {
        ReleaseStatus::Rejected
    } else if rs.image_digest.is_none() {
        match meta.map(|m| &m.build) {
            Some(BuildState::Failed { .. }) => ReleaseStatus::BuildFailed,
            _ => ReleaseStatus::Building,
        }
    } else if !rs.taint.is_empty() {
        ReleaseStatus::Tainted
    } else if active_now {
        ReleaseStatus::Active
    } else if stage.is_some() {
        ReleaseStatus::Superseded
    } else {
        ReleaseStatus::AwaitingApproval
    };
    let diff = state.diff_against_live(id).unwrap_or_default();
    let target = next_approval_stage(stage);
    let requirement = state.requirement(id, target).unwrap_or_else(|| state.policy.requirement(&diff));
    let approvals = rs
        .approvals
        .iter()
        .map(|(stage, a)| ApprovalView {
            stage: *stage,
            user: users.get(&a.user_id).map(crate::auth::user_view).unwrap_or_else(|| User {
                id: a.user_id.clone(),
                email: a.email.clone(),
                name: a.email.clone(),
            }),
            owner: a.owner,
        })
        .collect();
    let undeclared = rs.observed.as_ref().map(|(_, egress)| undeclared_egress(&rs.manifest, egress)).unwrap_or_default();
    Some(Release {
        id: id.to_owned(),
        project: project.to_owned(),
        manifest_hash: rs.manifest_hash.clone(),
        manifest: rs.manifest.clone(),
        provenance: rs.provenance.clone(),
        image_digest: rs.image_digest.clone(),
        stage,
        status,
        diff,
        requirement,
        approvals,
        taint: rs.taint.clone(),
        undeclared_egress: undeclared,
    })
}

/// The API view of a project.
pub fn project(slug: &str, org: &str, state: &LedgerState, registry: &Registry) -> Project {
    let active = state.active.iter().map(|(stage, release)| StageHead { stage: *stage, release: release.clone() }).collect();
    let domains = match registry.edge() {
        Some(edge) => registry.for_project(slug).into_iter().map(|b| registry.view(b, edge)).collect(),
        None => Vec::new(),
    };
    Project { slug: slug.to_owned(), org: org.to_owned(), active, domains }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undeclared_egress_is_what_the_manifest_does_not_cover() {
        let manifest: Manifest = serde_json::from_value(serde_json::json!({
            "api_version": "deploy.aho.cloud/v1", "project": "shop",
            "source": {"kind": "oci", "digest": format!("sha256:{}", "a".repeat(64))}, "build": {"kind": "prebuilt"},
            "processes": [{"name": "web", "health": {"kind": "command", "argv": ["true"]}}],
            "auth": {"mode": "none"}, "egress": ["api.stripe.com", "*.sentry.io"], "data_class": "none",
            "owner": {"name": "A", "email": "a@x.io"}, "budget": {"ceiling_usd_month": 1, "degrade": "sleep"}
        }))
        .unwrap();
        let observed = ["API.stripe.com.", "o1.sentry.io", "sentry.io", "evil.test", "evil.test", "stripe.com"].map(String::from);
        assert_eq!(undeclared_egress(&manifest, &observed), ["evil.test", "sentry.io", "stripe.com"]);
    }
}
