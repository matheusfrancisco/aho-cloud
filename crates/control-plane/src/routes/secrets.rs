//! Secret values: write-only. Stored sealed; responses carry names, stages and keyed
//! fingerprints, never values. Replacing a Live value with a different one is a rotation, which
//! resolves taint findings for that secret.

use std::collections::BTreeMap;

use argon2::password_hash::{PasswordHasher, SaltString};
use axum::Json;
use axum::extract::{Path, State};
use cloud_api::{SecretView, SetSecret};
use cloud_contract::ledger::Stage;

use super::ApiJson;
use crate::auth::{Caller, project_access};
use crate::error::AppError;
use crate::sites::EDGE_PASSWORD;
use crate::state::AppState;
use crate::store::SecretRec;

const MAX_VALUE_BYTES: usize = 64 * 1024;

fn valid_name(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn views(secrets: &[SecretRec]) -> Vec<SecretView> {
    let mut by_name: BTreeMap<&str, Vec<&SecretRec>> = BTreeMap::new();
    for s in secrets {
        by_name.entry(&s.name).or_default().push(s);
    }
    by_name
        .into_iter()
        .map(|(name, mut recs)| {
            recs.sort_by_key(|r| r.stage);
            // The fingerprint shown is the one the taint scan uses: Live's, else the highest stage.
            let fingerprint = recs.last().map(|r| r.fingerprint.clone()).unwrap_or_default();
            SecretView { name: name.to_owned(), stages: recs.iter().map(|r| r.stage).collect(), fingerprint }
        })
        .collect()
}

pub async fn list(State(st): State<AppState>, Caller(user): Caller, Path(p): Path<String>) -> Result<Json<Vec<SecretView>>, AppError> {
    let (project, _) = project_access(&st, &user, &p).await?;
    Ok(Json(views(&st.store.secrets(&project.slug).await?)))
}

pub async fn set(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path(p): Path<String>,
    ApiJson(body): ApiJson<SetSecret>,
) -> Result<Json<SecretView>, AppError> {
    let (project, _) = project_access(&st, &user, &p).await?;
    if !valid_name(&body.name) {
        return Err(AppError::invalid("secret names are [A-Z][A-Z0-9_]*, at most 128 characters"));
    }
    if body.value.is_empty() || body.value.len() > MAX_VALUE_BYTES {
        return Err(AppError::invalid(format!("secret values are 1–{MAX_VALUE_BYTES} bytes")));
    }
    let fingerprint = st.secrets.fingerprint(&body.value);
    let existing = st.store.secrets(&project.slug).await?;
    let previous = existing.iter().find(|s| s.name == body.name && s.stage == body.stage);
    let rotated = body.stage == Stage::Live && previous.is_some_and(|s| s.fingerprint != fingerprint);
    let edge_phc = if body.name == EDGE_PASSWORD {
        let salt = SaltString::encode_b64(&crate::crypto::random_bytes(16)).map_err(|e| AppError::internal(e.to_string()))?;
        Some(
            argon2::Argon2::default()
                .hash_password(body.value.as_bytes(), &salt)
                .map_err(|e| AppError::internal(e.to_string()))?
                .to_string(),
        )
    } else {
        None
    };
    let rec = SecretRec {
        project: project.slug.clone(),
        name: body.name.clone(),
        stage: body.stage,
        ciphertext: st.secrets.seal(&project.slug, &body.name, body.stage, &body.value)?,
        fingerprint,
        edge_phc,
        updated_at: st.now(),
    };
    st.store.put_secret(&rec).await?;
    if rotated {
        st.ledgers.get(&project.slug)?.secret_rotated(&body.name, &user.id)?;
    }
    let all = st.store.secrets(&project.slug).await?;
    views(&all).into_iter().find(|v| v.name == body.name).map(Json).ok_or_else(|| AppError::internal("secret vanished after write"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_names_are_env_style() {
        assert!(valid_name("STRIPE_KEY_2"));
        for bad in ["", "stripe", "2KEY", "KEY-1", "_KEY"] {
            assert!(!valid_name(bad), "{bad}");
        }
    }
}
