//! In-memory [`Store`]: the same rules as Postgres, behind one mutex.

use parking_lot::{Mutex, MutexGuard};
use std::collections::BTreeMap;

use async_trait::async_trait;
use cloud_contract::ledger::{Approver, Stage};
use cloud_contract::policy::ReleasePolicy;

use super::*;

#[derive(Default)]
struct Inner {
    users: BTreeMap<String, UserRec>,
    device_codes: BTreeMap<String, DeviceCodeRec>,
    tokens: BTreeMap<String, TokenRec>,
    device_keys: BTreeMap<String, DeviceKeyRec>,
    passkeys: BTreeMap<String, PasskeyRec>,
    ceremonies: BTreeMap<String, CeremonyRec>,
    orgs: BTreeMap<String, OrgRec>,
    members: BTreeMap<(String, String), MemberRec>,
    projects: BTreeMap<String, ProjectRec>,
    secrets: BTreeMap<(String, String, Stage), SecretRec>,
    blobs: BTreeMap<(String, String), BlobRec>,
    sessions: BTreeMap<(String, String, String), SessionRec>,
    releases: BTreeMap<(String, String), ReleaseMeta>,
    kv: BTreeMap<String, String>,
}

/// A [`Store`] that lives in process memory.
#[derive(Default)]
pub struct MemStore {
    inner: Mutex<Inner>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock()
    }
}

fn key2(a: &str, b: &str) -> (String, String) {
    (a.to_owned(), b.to_owned())
}

#[async_trait]
impl Store for MemStore {
    async fn create_user(&self, user: &UserRec) -> Result<(), StoreError> {
        let mut g = self.lock();
        if g.users.contains_key(&user.id) || g.users.values().any(|u| u.email == user.email) {
            return Err(StoreError::Conflict(format!("user {}", user.email)));
        }
        g.users.insert(user.id.clone(), user.clone());
        Ok(())
    }

    async fn user(&self, id: &str) -> Result<Option<UserRec>, StoreError> {
        Ok(self.lock().users.get(id).cloned())
    }

    async fn user_by_email(&self, email: &str) -> Result<Option<UserRec>, StoreError> {
        Ok(self.lock().users.values().find(|u| u.email == email).cloned())
    }

    async fn put_device_code(&self, code: &DeviceCodeRec) -> Result<(), StoreError> {
        let mut g = self.lock();
        if g.device_codes.values().any(|d| d.user_code == code.user_code) || g.device_codes.contains_key(&code.code_hash) {
            return Err(StoreError::Conflict(format!("device code {}", code.user_code)));
        }
        g.device_codes.insert(code.code_hash.clone(), code.clone());
        Ok(())
    }

    async fn device_code(&self, code_hash: &str) -> Result<Option<DeviceCodeRec>, StoreError> {
        Ok(self.lock().device_codes.get(code_hash).cloned())
    }

    async fn device_code_by_user_code(&self, user_code: &str) -> Result<Option<DeviceCodeRec>, StoreError> {
        Ok(self.lock().device_codes.values().find(|d| d.user_code == user_code).cloned())
    }

    async fn approve_device_code(&self, code_hash: &str, user_id: &str) -> Result<bool, StoreError> {
        let mut g = self.lock();
        match g.device_codes.get_mut(code_hash) {
            Some(d) if d.approved_by.is_none() => {
                d.approved_by = Some(user_id.to_owned());
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn take_device_code(&self, code_hash: &str) -> Result<Option<DeviceCodeRec>, StoreError> {
        Ok(self.lock().device_codes.remove(code_hash))
    }

    async fn put_token(&self, token: &TokenRec) -> Result<(), StoreError> {
        self.lock().tokens.insert(token.hash.clone(), token.clone());
        Ok(())
    }

    async fn token(&self, hash: &str) -> Result<Option<TokenRec>, StoreError> {
        Ok(self.lock().tokens.get(hash).cloned())
    }

    async fn put_device_key(&self, key: &DeviceKeyRec) -> Result<(), StoreError> {
        let mut g = self.lock();
        if g.device_keys.contains_key(&key.key_id) {
            return Err(StoreError::Conflict(format!("device key {}", key.key_id)));
        }
        g.device_keys.insert(key.key_id.clone(), key.clone());
        Ok(())
    }

    async fn device_key(&self, key_id: &str) -> Result<Option<DeviceKeyRec>, StoreError> {
        Ok(self.lock().device_keys.get(key_id).cloned())
    }

    async fn device_keys_for_user(&self, user_id: &str) -> Result<Vec<DeviceKeyRec>, StoreError> {
        Ok(self.lock().device_keys.values().filter(|k| k.user_id == user_id).cloned().collect())
    }

    async fn put_passkey(&self, passkey: &PasskeyRec) -> Result<(), StoreError> {
        let mut g = self.lock();
        if g.passkeys.contains_key(&passkey.credential_id) {
            return Err(StoreError::Conflict("passkey credential".into()));
        }
        g.passkeys.insert(passkey.credential_id.clone(), passkey.clone());
        Ok(())
    }

    async fn passkey(&self, credential_id: &str) -> Result<Option<PasskeyRec>, StoreError> {
        Ok(self.lock().passkeys.get(credential_id).cloned())
    }

    async fn passkeys_for_user(&self, user_id: &str) -> Result<Vec<PasskeyRec>, StoreError> {
        Ok(self.lock().passkeys.values().filter(|p| p.user_id == user_id).cloned().collect())
    }

    async fn update_sign_count(&self, credential_id: &str, old: u32, new: u32) -> Result<bool, StoreError> {
        let mut g = self.lock();
        match g.passkeys.get_mut(credential_id) {
            Some(p) if p.sign_count == old => {
                p.sign_count = new;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn put_ceremony(&self, ceremony: &CeremonyRec) -> Result<(), StoreError> {
        self.lock().ceremonies.insert(ceremony.id.clone(), ceremony.clone());
        Ok(())
    }

    async fn take_ceremony(&self, id: &str) -> Result<Option<CeremonyRec>, StoreError> {
        Ok(self.lock().ceremonies.remove(id))
    }

    async fn create_org(&self, org: &OrgRec, owner: &str) -> Result<(), StoreError> {
        let mut g = self.lock();
        if g.orgs.contains_key(&org.slug) {
            return Err(StoreError::Conflict(format!("org {}", org.slug)));
        }
        if !g.users.contains_key(owner) {
            return Err(StoreError::Backend(format!("unknown user {owner}")));
        }
        g.orgs.insert(org.slug.clone(), org.clone());
        g.members.insert(key2(&org.slug, owner), MemberRec { org: org.slug.clone(), user_id: owner.to_owned(), owner: true });
        Ok(())
    }

    async fn org(&self, slug: &str) -> Result<Option<OrgRec>, StoreError> {
        Ok(self.lock().orgs.get(slug).cloned())
    }

    async fn set_org_policy(&self, slug: &str, policy: &ReleasePolicy, approver: &Approver) -> Result<(), StoreError> {
        let mut g = self.lock();
        let org = g.orgs.get_mut(slug).ok_or_else(|| StoreError::Backend(format!("unknown org {slug}")))?;
        org.policy = policy.clone();
        org.policy_approver = Some(approver.clone());
        Ok(())
    }

    async fn put_member(&self, member: &MemberRec) -> Result<(), StoreError> {
        let mut g = self.lock();
        if !g.orgs.contains_key(&member.org) || !g.users.contains_key(&member.user_id) {
            return Err(StoreError::Backend("membership references an unknown org or user".into()));
        }
        g.members.insert(key2(&member.org, &member.user_id), member.clone());
        Ok(())
    }

    async fn member(&self, org: &str, user_id: &str) -> Result<Option<MemberRec>, StoreError> {
        Ok(self.lock().members.get(&key2(org, user_id)).cloned())
    }

    async fn members(&self, org: &str) -> Result<Vec<MemberRec>, StoreError> {
        Ok(self.lock().members.values().filter(|m| m.org == org).cloned().collect())
    }

    async fn memberships(&self, user_id: &str) -> Result<Vec<MemberRec>, StoreError> {
        Ok(self.lock().members.values().filter(|m| m.user_id == user_id).cloned().collect())
    }

    async fn create_project(&self, project: &ProjectRec) -> Result<(), StoreError> {
        let mut g = self.lock();
        if g.projects.contains_key(&project.slug) {
            return Err(StoreError::Conflict(format!("project {}", project.slug)));
        }
        if !g.orgs.contains_key(&project.org) {
            return Err(StoreError::Backend(format!("unknown org {}", project.org)));
        }
        g.projects.insert(project.slug.clone(), project.clone());
        Ok(())
    }

    async fn project(&self, slug: &str) -> Result<Option<ProjectRec>, StoreError> {
        Ok(self.lock().projects.get(slug).cloned())
    }

    async fn projects(&self) -> Result<Vec<ProjectRec>, StoreError> {
        Ok(self.lock().projects.values().cloned().collect())
    }

    async fn delete_project(&self, slug: &str, at: u64) -> Result<(), StoreError> {
        if let Some(p) = self.lock().projects.get_mut(slug) {
            p.deleted_at.get_or_insert(at);
        }
        Ok(())
    }

    async fn put_secret(&self, secret: &SecretRec) -> Result<(), StoreError> {
        let key = (secret.project.clone(), secret.name.clone(), secret.stage);
        self.lock().secrets.insert(key, secret.clone());
        Ok(())
    }

    async fn secrets(&self, project: &str) -> Result<Vec<SecretRec>, StoreError> {
        Ok(self.lock().secrets.values().filter(|s| s.project == project).cloned().collect())
    }

    async fn put_blob(&self, blob: &BlobRec) -> Result<(), StoreError> {
        self.lock().blobs.entry(key2(&blob.project, &blob.hash)).or_insert_with(|| blob.clone());
        Ok(())
    }

    async fn blob(&self, project: &str, hash: &str) -> Result<Option<BlobRec>, StoreError> {
        Ok(self.lock().blobs.get(&key2(project, hash)).cloned())
    }

    async fn archive_exists(&self, hash: &str) -> Result<bool, StoreError> {
        Ok(self.lock().blobs.values().any(|b| b.hash == hash && b.kind == BlobKind::Archive))
    }

    async fn put_session(&self, session: &SessionRec) -> Result<(), StoreError> {
        let key = (session.project.clone(), session.session_id.clone(), session.head_hash.clone());
        self.lock().sessions.entry(key).or_insert_with(|| session.clone());
        Ok(())
    }

    async fn session(&self, project: &str, session_id: &str, head_hash: &str) -> Result<Option<SessionRec>, StoreError> {
        Ok(self.lock().sessions.get(&(project.to_owned(), session_id.to_owned(), head_hash.to_owned())).cloned())
    }

    async fn sessions(&self, project: &str) -> Result<Vec<SessionRec>, StoreError> {
        Ok(self.lock().sessions.values().filter(|s| s.project == project).cloned().collect())
    }

    async fn put_release_meta(&self, meta: &ReleaseMeta) -> Result<(), StoreError> {
        self.lock().releases.insert(key2(&meta.project, &meta.release), meta.clone());
        Ok(())
    }

    async fn release_meta(&self, project: &str, release: &str) -> Result<Option<ReleaseMeta>, StoreError> {
        Ok(self.lock().releases.get(&key2(project, release)).cloned())
    }

    async fn kv_get(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self.lock().kv.get(key).cloned())
    }

    async fn kv_put(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.lock().kv.insert(key.to_owned(), value.to_owned());
        Ok(())
    }
}
