//! Scripted `IdentityProvider` for login, link and invite tests.
//!
//! `authorize_url` records the params and returns `https://idp.test/{kind}/authorize?…`;
//! `exchange` records `(code, verifier, redirect_uri)` and answers the result scripted for that
//! code once (codes are one-time, like real providers; unknown codes are `Denied`);
//! `lookup_login` answers from the logins added with [`FakeIdentityProvider::add_login`].

use std::collections::HashMap;

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::ports::auth::{AuthorizeParams, ExternalIdentity, IdentityError, IdentityProvider, IdentityProviderKind};

/// One recorded code exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedExchange {
    pub code: String,
    pub pkce_verifier: String,
    pub redirect_uri: String,
}

#[derive(Default)]
struct State {
    codes: HashMap<String, Result<ExternalIdentity, IdentityError>>,
    logins: HashMap<String, ExternalIdentity>,
    authorizations: Vec<AuthorizeParams>,
    exchanges: Vec<RecordedExchange>,
}

pub struct FakeIdentityProvider {
    kind: IdentityProviderKind,
    state: Mutex<State>,
}

impl FakeIdentityProvider {
    pub fn new(kind: IdentityProviderKind) -> FakeIdentityProvider {
        FakeIdentityProvider { kind, state: Mutex::new(State::default()) }
    }

    /// An identity of this provider (`login` doubles as display name; email optional).
    pub fn identity(&self, subject: &str, login: &str, email: Option<&str>) -> ExternalIdentity {
        ExternalIdentity {
            provider: self.kind,
            subject: subject.to_owned(),
            login: login.to_owned(),
            name: Some(login.to_owned()),
            email: email.map(str::to_owned),
            avatar_url: None,
        }
    }

    /// The next exchange of `code` answers `result` (once).
    pub fn on_code(&self, code: &str, result: Result<ExternalIdentity, IdentityError>) {
        self.state.lock().codes.insert(code.to_owned(), result);
    }

    /// `lookup_login(login)` (case-insensitive) answers `identity`.
    pub fn add_login(&self, identity: ExternalIdentity) {
        self.state.lock().logins.insert(identity.login.to_ascii_lowercase(), identity);
    }

    pub fn authorizations(&self) -> Vec<AuthorizeParams> {
        self.state.lock().authorizations.clone()
    }

    pub fn exchanges(&self) -> Vec<RecordedExchange> {
        self.state.lock().exchanges.clone()
    }
}

#[async_trait]
impl IdentityProvider for FakeIdentityProvider {
    fn kind(&self) -> IdentityProviderKind {
        self.kind
    }

    fn authorize_url(&self, params: &AuthorizeParams) -> String {
        self.state.lock().authorizations.push(params.clone());
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("state", &params.state)
            .append_pair("code_challenge", &params.pkce_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("redirect_uri", &params.redirect_uri)
            .finish();
        format!("https://idp.test/{}/authorize?{query}", self.kind.as_str())
    }

    async fn exchange(&self, code: &str, pkce_verifier: &str, redirect_uri: &str) -> Result<ExternalIdentity, IdentityError> {
        let mut state = self.state.lock();
        state.exchanges.push(RecordedExchange { code: code.to_owned(), pkce_verifier: pkce_verifier.to_owned(), redirect_uri: redirect_uri.to_owned() });
        state.codes.remove(code).unwrap_or(Err(IdentityError::Denied))
    }

    async fn lookup_login(&self, login: &str) -> Result<Option<ExternalIdentity>, IdentityError> {
        Ok(self.state.lock().logins.get(&login.to_ascii_lowercase()).cloned())
    }
}
