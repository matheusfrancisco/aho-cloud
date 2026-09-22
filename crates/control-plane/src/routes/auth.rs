//! Device flow, passkeys, `/me`.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use cloud_api::{
    DeviceApprove, DevicePoll, DevicePolled, DeviceStart, DeviceStarted, Me, Membership, PasskeyChallenge, PasskeyFinish,
    PasskeyLoginStart, PasskeyRegisterStart, Session,
};

use super::ApiJson;
use crate::approvals::device_public_key;
use crate::auth::{Caller, MaybeCaller, issue_session, user_view};
use crate::crypto::{b64url, random_bytes, random_token, token_hash};
use crate::error::AppError;
use crate::state::AppState;
use crate::store::{CeremonyKind, CeremonyRec, DeviceCodeRec, DeviceKeyRec, PasskeyRec, StoreError, UserRec};
use crate::webauthn;

/// Letters that survive being read aloud and typed (no vowels, no look-alikes).
const USER_CODE_ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";

fn user_code() -> String {
    let n = USER_CODE_ALPHABET.len();
    let limit = 256 - 256 % n;
    let mut letters = String::with_capacity(8);
    while letters.len() < 8 {
        // Rejection sampling: bytes past the last whole multiple of the alphabet would bias it.
        for b in random_bytes(16).into_iter().map(usize::from).filter(|b| *b < limit) {
            if letters.len() < 8 {
                letters.push(char::from(USER_CODE_ALPHABET[b % n]));
            }
        }
    }
    format!("{}-{}", &letters[..4], &letters[4..])
}

/// `wdjb mjht`, `WDJBMJHT` and `WDJB-MJHT` are the same code.
fn normalize_user_code(input: &str) -> String {
    let letters: String = input.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_uppercase()).collect();
    if letters.len() == 8 { format!("{}-{}", &letters[..4], &letters[4..]) } else { letters }
}

pub fn normalize_email(email: &str) -> Result<String, AppError> {
    let email = email.trim().to_ascii_lowercase();
    let valid = email.len() <= 254
        && !email.chars().any(char::is_whitespace)
        && email
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.'));
    if valid { Ok(email) } else { Err(AppError::invalid("email is not an address")) }
}

pub async fn device_start(State(st): State<AppState>, ApiJson(body): ApiJson<DeviceStart>) -> Result<Json<DeviceStarted>, AppError> {
    if device_public_key(&body.public_key).is_none() {
        return Err(AppError::invalid("public_key must be a base64url Ed25519 public key (32 bytes)"));
    }
    let name = body.device_name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(AppError::invalid("device_name must be 1–100 characters"));
    }
    let device_code = random_token("ahd");
    let ttl = st.cfg.device_code_ttl_secs;
    // A user-code collision is astronomically rare; retry a few times rather than fail.
    for _ in 0..4 {
        let rec = DeviceCodeRec {
            code_hash: token_hash(&device_code),
            user_code: user_code(),
            device_name: name.to_owned(),
            public_key: body.public_key.clone(),
            expires_at: st.now() + ttl,
            approved_by: None,
        };
        match st.store.put_device_code(&rec).await {
            Ok(()) => {
                return Ok(Json(DeviceStarted {
                    device_code,
                    user_code: rec.user_code,
                    verification_uri: st.cfg.verification_uri(),
                    interval_secs: st.cfg.device_poll_interval_secs,
                    expires_in_secs: u32::try_from(ttl).unwrap_or(u32::MAX),
                }));
            }
            Err(StoreError::Conflict(_)) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(AppError::internal("could not allocate a user code"))
}

pub async fn device_approve(
    State(st): State<AppState>,
    Caller(user): Caller,
    ApiJson(body): ApiJson<DeviceApprove>,
) -> Result<StatusCode, AppError> {
    let code = normalize_user_code(&body.user_code);
    let rec = st
        .store
        .device_code_by_user_code(&code)
        .await?
        .filter(|d| d.expires_at > st.now())
        .ok_or_else(|| AppError::not_found("unknown or expired code"))?;
    if !st.store.approve_device_code(&rec.code_hash, &user.id).await? {
        return Err(AppError::conflict("this code was already approved"));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn device_poll(State(st): State<AppState>, ApiJson(body): ApiJson<DevicePoll>) -> Result<Json<DevicePolled>, AppError> {
    let hash = token_hash(&body.device_code);
    let Some(rec) = st.store.device_code(&hash).await? else {
        return Ok(Json(DevicePolled::Expired));
    };
    if rec.expires_at <= st.now() {
        st.store.take_device_code(&hash).await?;
        return Ok(Json(DevicePolled::Expired));
    }
    let Some(user_id) = rec.approved_by else {
        return Ok(Json(DevicePolled::Pending));
    };
    // Redeem exactly once: whoever deletes the row gets the token.
    if st.store.take_device_code(&hash).await?.is_none() {
        return Ok(Json(DevicePolled::Expired));
    }
    let user = st.store.user(&user_id).await?.ok_or_else(|| AppError::internal("approving user vanished"))?;
    let key_id = format!("dk_{}", b64url(&random_bytes(12)));
    st.store
        .put_device_key(&DeviceKeyRec {
            key_id: key_id.clone(),
            user_id: user.id.clone(),
            name: rec.device_name,
            public_key: rec.public_key,
            created_at: st.now(),
        })
        .await?;
    let token = issue_session(&st, &user.id).await?;
    Ok(Json(DevicePolled::Approved { token, key_id, user: user_view(&user) }))
}

fn ceremony_timeout_ms(st: &AppState) -> u64 {
    st.cfg.ceremony_ttl_secs * 1000
}

pub async fn passkey_register_start(
    State(st): State<AppState>,
    MaybeCaller(caller): MaybeCaller,
    ApiJson(body): ApiJson<PasskeyRegisterStart>,
) -> Result<Json<PasskeyChallenge>, AppError> {
    let email = normalize_email(&body.email)?;
    let name = body.name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(AppError::invalid("name must be 1–100 characters"));
    }
    let (user_id, new_user, exclude) = match st.store.user_by_email(&email).await? {
        // Adding a passkey to an existing account needs that account's session.
        Some(existing) => {
            if caller.as_ref().map(|c| &c.id) != Some(&existing.id) {
                return Err(AppError::conflict("an account exists for this email; sign in to add a passkey"));
            }
            let exclude = st.store.passkeys_for_user(&existing.id).await?.into_iter().map(|p| p.credential_id).collect::<Vec<_>>();
            (existing.id, false, exclude)
        }
        None => (format!("u_{}", b64url(&random_bytes(12))), true, Vec::new()),
    };
    let challenge = random_bytes(32);
    let options = webauthn::creation_options(&st.rp, user_id.as_bytes(), &email, name, &challenge, &exclude, ceremony_timeout_ms(&st));
    let ceremony = CeremonyRec {
        id: random_token("pkc"),
        kind: CeremonyKind::Register,
        challenge,
        user_id,
        email,
        name: name.to_owned(),
        new_user,
        expires_at: st.now() + st.cfg.ceremony_ttl_secs,
    };
    st.store.put_ceremony(&ceremony).await?;
    Ok(Json(PasskeyChallenge { ceremony_id: ceremony.id, options }))
}

async fn take_ceremony(st: &AppState, id: &str, kind: CeremonyKind) -> Result<CeremonyRec, AppError> {
    st.store
        .take_ceremony(id)
        .await?
        .filter(|c| c.kind == kind && c.expires_at > st.now())
        .ok_or_else(|| AppError::unauthorized("unknown or expired passkey ceremony"))
}

pub async fn passkey_register_finish(State(st): State<AppState>, ApiJson(body): ApiJson<PasskeyFinish>) -> Result<Json<Session>, AppError> {
    let c = take_ceremony(&st, &body.ceremony_id, CeremonyKind::Register).await?;
    let cred = webauthn::verify_registration(&st.rp, &c.challenge, &body.credential).map_err(|e| AppError::unauthorized(e.to_string()))?;
    let user = if c.new_user {
        let user = UserRec { id: c.user_id.clone(), email: c.email.clone(), name: c.name.clone() };
        st.store.create_user(&user).await?;
        user
    } else {
        st.store.user(&c.user_id).await?.ok_or_else(|| AppError::unauthorized("account no longer exists"))?
    };
    st.store
        .put_passkey(&PasskeyRec {
            credential_id: b64url(&cred.id),
            user_id: user.id.clone(),
            public_key: cred.public_key,
            sign_count: cred.sign_count,
            created_at: st.now(),
        })
        .await?;
    let token = issue_session(&st, &user.id).await?;
    Ok(Json(Session { token, user: user_view(&user) }))
}

pub async fn passkey_login_start(
    State(st): State<AppState>,
    ApiJson(body): ApiJson<PasskeyLoginStart>,
) -> Result<Json<PasskeyChallenge>, AppError> {
    let email = normalize_email(&body.email)?;
    // Unknown emails get a well-formed challenge that can never succeed, so the response does
    // not reveal whether an account exists.
    let (user_id, allow) = match st.store.user_by_email(&email).await? {
        Some(u) => {
            let allow = st.store.passkeys_for_user(&u.id).await?.into_iter().map(|p| p.credential_id).collect::<Vec<_>>();
            (u.id, allow)
        }
        None => (String::new(), vec![b64url(&random_bytes(16))]),
    };
    let challenge = random_bytes(32);
    let options = webauthn::request_options(&st.rp, &challenge, &allow, ceremony_timeout_ms(&st));
    let ceremony = CeremonyRec {
        id: random_token("pkc"),
        kind: CeremonyKind::Login,
        challenge,
        user_id,
        email,
        name: String::new(),
        new_user: false,
        expires_at: st.now() + st.cfg.ceremony_ttl_secs,
    };
    st.store.put_ceremony(&ceremony).await?;
    Ok(Json(PasskeyChallenge { ceremony_id: ceremony.id, options }))
}

pub async fn passkey_login_finish(State(st): State<AppState>, ApiJson(body): ApiJson<PasskeyFinish>) -> Result<Json<Session>, AppError> {
    let c = take_ceremony(&st, &body.ceremony_id, CeremonyKind::Login).await?;
    let a = webauthn::parse_assertion(&body.credential).map_err(|e| AppError::unauthorized(e.to_string()))?;
    let cred_id = b64url(&a.credential_id);
    let pk = st
        .store
        .passkey(&cred_id)
        .await?
        .filter(|p| !c.user_id.is_empty() && p.user_id == c.user_id)
        .ok_or_else(|| AppError::unauthorized("passkey is not registered to this account"))?;
    if a.user_handle.as_deref().is_some_and(|h| h != pk.user_id.as_bytes()) {
        return Err(AppError::unauthorized("assertion is for another account"));
    }
    let count = webauthn::verify_assertion(&st.rp, &c.challenge, &a, &pk.public_key, pk.sign_count)
        .map_err(|e| AppError::unauthorized(e.to_string()))?;
    if !st.store.update_sign_count(&cred_id, pk.sign_count, count).await? {
        return Err(AppError::unauthorized("passkey used concurrently; sign in again"));
    }
    let user = st.store.user(&pk.user_id).await?.ok_or_else(|| AppError::unauthorized("account no longer exists"))?;
    let token = issue_session(&st, &user.id).await?;
    Ok(Json(Session { token, user: user_view(&user) }))
}

pub async fn me(State(st): State<AppState>, Caller(user): Caller) -> Result<Json<Me>, AppError> {
    let orgs = st.store.memberships(&user.id).await?.into_iter().map(|m| Membership { org: m.org, owner: m.owner }).collect();
    Ok(Json(Me { user: user_view(&user), orgs }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_codes_are_formatted_and_normalized() {
        let c = user_code();
        assert_eq!(c.len(), 9);
        assert!(c.chars().all(|ch| ch == '-' || USER_CODE_ALPHABET.contains(&(ch as u8))));
        assert_eq!(normalize_user_code("wdjb mjht"), "WDJB-MJHT");
        assert_eq!(normalize_user_code("WDJB-MJHT"), "WDJB-MJHT");
    }

    #[test]
    fn emails_are_lowercased_and_checked() {
        assert_eq!(normalize_email(" Ana@Example.COM ").unwrap(), "ana@example.com");
        for bad in ["", "ana", "@x.io", "ana@x", "ana@.x.io", "a b@x.io"] {
            assert!(normalize_email(bad).is_err(), "{bad}");
        }
    }
}
