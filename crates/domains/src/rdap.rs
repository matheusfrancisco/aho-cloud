//! Domain registration expiry over RDAP (RFC 9083), via the `rdap.org` bootstrap redirector.

use std::time::Duration;

use futures::future::BoxFuture;
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::resolver::normalize_name;

/// Public RDAP bootstrap service; redirects to the registry's server for the TLD.
pub const RDAP_ORG: &str = "https://rdap.org";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// When a registered domain expires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expiry {
    /// The registered domain that answered (e.g. `acme.com` for `shop.acme.com`).
    pub domain: String,
    /// Unix seconds.
    pub expires_at: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum RdapError {
    #[error("RDAP request for {domain}: {reason}")]
    Http { domain: String, reason: String },
    #[error("RDAP response for {domain}: {reason}")]
    Parse { domain: String, reason: String },
}

/// Registration expiry lookups.
pub trait Rdap: Send + Sync {
    /// Expiry of the registered domain containing `hostname`; `None` when no registry knows it or
    /// the registry publishes no expiration event.
    fn expiry<'a>(&'a self, hostname: &'a str) -> BoxFuture<'a, Result<Option<Expiry>, RdapError>>;
}

#[derive(Deserialize)]
struct DomainObject {
    #[serde(default)]
    events: Vec<Event>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Event {
    event_action: String,
    #[serde(default)]
    event_date: Option<String>,
}

/// The `expiration` event of an RDAP domain object, as Unix seconds.
pub fn parse_expiration(domain: &str, body: &[u8]) -> Result<Option<u64>, RdapError> {
    let parse_err = |reason: String| RdapError::Parse { domain: domain.to_owned(), reason };
    let object: DomainObject = serde_json::from_slice(body).map_err(|e| parse_err(e.to_string()))?;
    let Some(date) = object.events.into_iter().find(|e| e.event_action.eq_ignore_ascii_case("expiration")).and_then(|e| e.event_date)
    else {
        return Ok(None);
    };
    let at = OffsetDateTime::parse(date.trim(), &Rfc3339).map_err(|e| parse_err(format!("eventDate `{date}`: {e}")))?;
    Ok(Some(u64::try_from(at.unix_timestamp()).unwrap_or(0)))
}

/// [`Rdap`] over HTTPS. Without a public-suffix list it walks up from the hostname
/// (`a.shop.acme.com`, `shop.acme.com`, `acme.com`) until a registry answers; it never queries
/// a bare TLD.
#[derive(Clone, Debug)]
pub struct RdapClient {
    http: reqwest::Client,
    base: String,
}

impl Default for RdapClient {
    fn default() -> Self {
        Self::new(RDAP_ORG)
    }
}

impl RdapClient {
    /// `base` is the RDAP service root; lookups hit `<base>/domain/<name>`.
    pub fn new(base: &str) -> Self {
        Self { http: reqwest::Client::new(), base: base.trim_end_matches('/').to_owned() }
    }

    /// Uses a preconfigured client (timeouts, proxy).
    pub fn with_client(http: reqwest::Client, base: &str) -> Self {
        Self { http, base: base.trim_end_matches('/').to_owned() }
    }

    async fn lookup(&self, domain: &str) -> Result<Option<Vec<u8>>, RdapError> {
        let http_err = |reason: String| RdapError::Http { domain: domain.to_owned(), reason };
        let response = self
            .http
            .get(format!("{}/domain/{domain}", self.base))
            .header(reqwest::header::ACCEPT, "application/rdap+json, application/json")
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| http_err(e.to_string()))?;
        match response.status() {
            reqwest::StatusCode::NOT_FOUND => Ok(None),
            s if s.is_success() => Ok(Some(response.bytes().await.map_err(|e| http_err(e.to_string()))?.to_vec())),
            s => Err(http_err(format!("status {s}"))),
        }
    }
}

impl Rdap for RdapClient {
    fn expiry<'a>(&'a self, hostname: &'a str) -> BoxFuture<'a, Result<Option<Expiry>, RdapError>> {
        Box::pin(async move {
            let name = normalize_name(hostname);
            let mut candidate = name.as_str();
            while candidate.contains('.') {
                if let Some(body) = self.lookup(candidate).await? {
                    return Ok(parse_expiration(candidate, &body)?.map(|expires_at| Expiry { domain: candidate.to_owned(), expires_at }));
                }
                candidate = match candidate.split_once('.') {
                    Some((_, rest)) => rest,
                    None => break,
                };
            }
            Ok(None)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiration_event_is_found_among_others_and_fractional_seconds_parse() {
        let body = br#"{"objectClassName":"domain","events":[
            {"eventAction":"registration","eventDate":"1997-09-15T04:00:00Z"},
            {"eventAction":"expiration","eventDate":"2028-09-14T04:00:00.000Z"}]}"#;
        assert_eq!(parse_expiration("acme.com", body).unwrap(), Some(1_852_516_800));
    }

    #[test]
    fn missing_expiration_is_none_and_garbage_is_an_error() {
        assert_eq!(parse_expiration("x.com", br#"{"events":[]}"#).unwrap(), None);
        assert_eq!(parse_expiration("x.com", br#"{}"#).unwrap(), None);
        assert!(parse_expiration("x.com", b"<html>").is_err());
        assert!(parse_expiration("x.com", br#"{"events":[{"eventAction":"expiration","eventDate":"soon"}]}"#).is_err());
    }
}
