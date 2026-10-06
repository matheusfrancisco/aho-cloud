//! Google Application Default Credentials: an OAuth access token (scope `cloud-platform`) for the
//! server's own Google API calls (Vertex here; the vault's KMS KEK uses its own instance).
//!
//! Sources, in ADC order:
//! 1. `GOOGLE_APPLICATION_CREDENTIALS` naming a credentials file;
//! 2. the gcloud well-known file (`gcloud auth application-default login`), local dev;
//! 3. the metadata server of the attached service account (Cloud Run, GCE; `GCE_METADATA_HOST`
//!    overrides the host).
//!
//! Credentials files must be `authorized_user` (gcloud login). Service-account key files are
//! refused: server.md "Google APIs … ADC of the attached service account; no key files anywhere".
//! Each instance caches its token until 5 minutes before expiry; concurrent callers share one
//! refresh.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::Mutex;
use url::Url;

/// OAuth scope requested for every token.
pub const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const DEFAULT_METADATA_HOST: &str = "metadata.google.internal";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
/// Refresh this long before the token's expiry.
const EARLY_REFRESH: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdcError {
    /// No usable credential source, or an unreadable / unsupported credentials file.
    #[error("application default credentials: {0}")]
    Config(String),
    /// The token endpoint could not be reached or refused the request.
    #[error("access token request failed: {0}")]
    Unavailable(String),
}

enum Credential {
    MetadataServer { token_url: Url },
    AuthorizedUser { client_id: String, client_secret: String, refresh_token: String, token_uri: Url },
}

struct Cached {
    token: String,
    refresh_at: Instant,
}

pub struct AdcTokenSource {
    client: reqwest::Client,
    credential: Credential,
    cache: Mutex<Option<Cached>>,
}

#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(rename = "type")]
    kind: String,
    client_id: Option<String>,
    client_secret: Option<String>,
    refresh_token: Option<String>,
    token_uri: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

impl AdcTokenSource {
    /// Resolves the credential in ADC order (see module docs).
    pub fn from_environment() -> Result<AdcTokenSource, AdcError> {
        if let Some(path) = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS").filter(|p| !p.is_empty()) {
            return AdcTokenSource::from_credentials_file(Path::new(&path));
        }
        if let Some(path) = well_known_file().filter(|p| p.is_file()) {
            return AdcTokenSource::from_credentials_file(&path);
        }
        let host = std::env::var("GCE_METADATA_HOST").ok().filter(|h| !h.is_empty());
        let base = Url::parse(&format!("http://{}/", host.as_deref().unwrap_or(DEFAULT_METADATA_HOST)))
            .map_err(|e| AdcError::Config(format!("GCE_METADATA_HOST: {e}")))?;
        AdcTokenSource::metadata_server(base)
    }

    /// Tokens of the attached service account from the metadata server at `base`
    /// (`http://metadata.google.internal/` in production).
    pub fn metadata_server(base: Url) -> Result<AdcTokenSource, AdcError> {
        let mut token_url = base
            .join("computeMetadata/v1/instance/service-accounts/default/token")
            .map_err(|e| AdcError::Config(format!("metadata server url: {e}")))?;
        token_url.query_pairs_mut().append_pair("scopes", CLOUD_PLATFORM_SCOPE);
        AdcTokenSource::with(Credential::MetadataServer { token_url })
    }

    /// An `authorized_user` credentials file (gcloud's application-default login).
    pub fn from_credentials_file(path: &Path) -> Result<AdcTokenSource, AdcError> {
        let raw = std::fs::read(path).map_err(|e| AdcError::Config(format!("{}: {e}", path.display())))?;
        AdcTokenSource::from_credentials_json(&raw)
    }

    pub fn from_credentials_json(raw: &[u8]) -> Result<AdcTokenSource, AdcError> {
        let file: CredentialsFile =
            serde_json::from_slice(raw).map_err(|e| AdcError::Config(format!("credentials file is not valid JSON: {e}")))?;
        match file.kind.as_str() {
            "authorized_user" => {
                let field = |v: Option<String>, name: &str| {
                    v.filter(|s| !s.is_empty()).ok_or_else(|| AdcError::Config(format!("authorized_user credentials lack {name}")))
                };
                let token_uri = file.token_uri.unwrap_or_else(|| DEFAULT_TOKEN_URI.to_owned());
                let token_uri = Url::parse(&token_uri).map_err(|e| AdcError::Config(format!("token_uri: {e}")))?;
                AdcTokenSource::with(Credential::AuthorizedUser {
                    client_id: field(file.client_id, "client_id")?,
                    client_secret: field(file.client_secret, "client_secret")?,
                    refresh_token: field(file.refresh_token, "refresh_token")?,
                    token_uri,
                })
            }
            "service_account" => Err(AdcError::Config(
                "service-account key files are not accepted; attach the service account to the instance or use \
                 `gcloud auth application-default login` locally"
                    .into(),
            )),
            other => Err(AdcError::Config(format!("credentials type {other:?} is not accepted (expected authorized_user)"))),
        }
    }

    fn with(credential: Credential) -> Result<AdcTokenSource, AdcError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| AdcError::Config(format!("http client: {e}")))?;
        Ok(AdcTokenSource { client, credential, cache: Mutex::new(None) })
    }

    /// A bearer token valid for at least the next few minutes (cached).
    pub async fn access_token(&self) -> Result<String, AdcError> {
        let mut cache = self.cache.lock().await;
        if let Some(cached) = cache.as_ref().filter(|c| Instant::now() < c.refresh_at) {
            return Ok(cached.token.clone());
        }
        let fetched_at = Instant::now();
        let response = self.fetch().await?;
        let lifetime = Duration::from_secs(response.expires_in);
        // A token that lives less than the early-refresh window is used for this call only.
        let refresh_at = fetched_at + lifetime.saturating_sub(EARLY_REFRESH);
        *cache = Some(Cached { token: response.access_token.clone(), refresh_at });
        Ok(response.access_token)
    }

    async fn fetch(&self) -> Result<TokenResponse, AdcError> {
        let request = match &self.credential {
            Credential::MetadataServer { token_url } => self.client.get(token_url.clone()).header("Metadata-Flavor", "Google"),
            Credential::AuthorizedUser { client_id, client_secret, refresh_token, token_uri } => {
                self.client.post(token_uri.clone()).form(&[
                    ("grant_type", "refresh_token"),
                    ("client_id", client_id.as_str()),
                    ("client_secret", client_secret.as_str()),
                    ("refresh_token", refresh_token.as_str()),
                ])
            }
        };
        let response = request.send().await.map_err(|e| AdcError::Unavailable(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            // The body may echo request details; only the status is reported.
            return Err(AdcError::Unavailable(format!("token endpoint answered {status}")));
        }
        let body: TokenResponse = response.json().await.map_err(|e| AdcError::Unavailable(format!("token response: {e}")))?;
        if body.access_token.is_empty() {
            return Err(AdcError::Unavailable("token endpoint returned an empty access token".into()));
        }
        Ok(body)
    }
}

/// `$HOME/.config/gcloud/application_default_credentials.json` (or under `CLOUDSDK_CONFIG`).
fn well_known_file() -> Option<PathBuf> {
    let dir = match std::env::var_os("CLOUDSDK_CONFIG").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").filter(|h| !h.is_empty())?).join(".config").join("gcloud"),
    };
    Some(dir.join("application_default_credentials.json"))
}
