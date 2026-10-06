//! `IndexerLink` over HTTP (knowledge.md "`mage_server::adapters::driven::indexer_link`"):
//! `reqwest` client to `server.knowledge.indexer_url`, JSON DTOs of `mage_core::wire::indexer`,
//! a fresh `mgi_` token per request from an injected minter (keeps this adapter independent of key
//! management), 6 s timeout, one retry on connect errors only. Status mapping: 400 → `Invalid`,
//! 404 → `NotFound`, 409 → `Conflict`, 503 → `Unavailable`; a refused token (401/403) or any other
//! status is `Unavailable` for the caller.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::{CommitSha, LineRange, OrgId, RepoId, RepoPath, RepoVersionKey};
use mage_core::wire::indexer::{
    BlameHunk, BlameRequest, BlameResponse, CommitInfo, DiffRequest, DiffResponse, FileDiff, IndexerError, LogQuery, LogRequest,
    LogResponse, SearchRequest, TextHits, TextQuery, TreeDiffOptions, routes,
};
use reqwest::StatusCode;
use serde::Serialize;
use serde::de::DeserializeOwned;
use url::Url;

use crate::ports::indexer_link::{IndexerLink, IndexerLinkError};

/// Mints the `mgi_` token of one call: claims `{org, repo}`, `exp ≤ iat + 60 s`, fresh `jti`.
/// Called once per HTTP attempt (the indexer rejects replayed `jti`s).
pub type MintIndexerToken = Arc<dyn Fn(OrgId, RepoId) -> Result<String, IndexerLinkError> + Send + Sync>;

/// knowledge.md: every `IndexerLink` call times out at 6 s.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(6);

pub struct HttpIndexerLink {
    client: reqwest::Client,
    base: Url,
    mint: MintIndexerToken,
}

impl HttpIndexerLink {
    /// `base` is the indexer's internal origin (`http://10.x.x.x:8081`); routes are appended.
    pub fn new(base: Url, mint: MintIndexerToken, timeout: Duration) -> Result<Self, IndexerLinkError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(2)))
            .build()
            .map_err(|e| IndexerLinkError::Unavailable(format!("http client: {e}")))?;
        Ok(Self { client, base, mint })
    }

    fn url(&self, route: &str) -> Result<Url, IndexerLinkError> {
        self.base.join(route).map_err(|e| IndexerLinkError::Invalid(format!("indexer url: {e}")))
    }

    async fn post<B: Serialize + Sync, R: DeserializeOwned>(&self, org: OrgId, repo: RepoId, route: &str, body: &B) -> Result<R, IndexerLinkError> {
        let url = self.url(route)?;
        let mut retried = false;
        loop {
            let token = (self.mint)(org, repo)?;
            let sent = self.client.post(url.clone()).bearer_auth(token).json(body).send().await;
            match sent {
                Ok(response) => return decode(response).await,
                Err(e) if e.is_connect() && !retried => retried = true,
                Err(e) if e.is_timeout() => return Err(IndexerLinkError::Timeout),
                Err(e) => return Err(IndexerLinkError::Unavailable(format!("indexer request failed: {e}"))),
            }
        }
    }
}

async fn decode<R: DeserializeOwned>(response: reqwest::Response) -> Result<R, IndexerLinkError> {
    let status = response.status();
    if status.is_success() {
        return response.json::<R>().await.map_err(|e| {
            if e.is_timeout() { IndexerLinkError::Timeout } else { IndexerLinkError::Unavailable(format!("indexer response: {e}")) }
        });
    }
    let text = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<IndexerError>(&text).map(|e| e.message).unwrap_or_else(|_| truncate(&text, 512));
    Err(match status {
        StatusCode::BAD_REQUEST => IndexerLinkError::Invalid(message),
        StatusCode::NOT_FOUND => IndexerLinkError::NotFound,
        StatusCode::CONFLICT => IndexerLinkError::Conflict,
        StatusCode::SERVICE_UNAVAILABLE => IndexerLinkError::Unavailable(message),
        StatusCode::GATEWAY_TIMEOUT => IndexerLinkError::Timeout,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => IndexerLinkError::Unavailable(format!("indexer refused the service token: {message}")),
        other => IndexerLinkError::Unavailable(format!("indexer answered {other}: {message}")),
    })
}

fn truncate(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[async_trait]
impl IndexerLink for HttpIndexerLink {
    async fn search(&self, org: OrgId, key: &RepoVersionKey, q: &TextQuery, include_generated: bool) -> Result<TextHits, IndexerLinkError> {
        q.check_limits().map_err(IndexerLinkError::Invalid)?;
        let body = SearchRequest { org_id: org, repo_id: key.repo_id, sha: key.sha, query: q.clone(), include_generated };
        self.post(org, key.repo_id, routes::SEARCH, &body).await
    }

    async fn blame(&self, org: OrgId, key: &RepoVersionKey, path: &RepoPath, lines: LineRange) -> Result<Vec<BlameHunk>, IndexerLinkError> {
        let body = BlameRequest::new(org, key.repo_id, key.sha, path.clone(), lines);
        body.lines().map_err(IndexerLinkError::Invalid)?;
        let response: BlameResponse = self.post(org, key.repo_id, routes::BLAME, &body).await?;
        Ok(response.hunks)
    }

    async fn diff(&self, org: OrgId, repo: RepoId, from: CommitSha, to: CommitSha, opts: &TreeDiffOptions) -> Result<Vec<FileDiff>, IndexerLinkError> {
        let body = DiffRequest { org_id: org, repo_id: repo, from, to, opts: opts.clone() };
        let response: DiffResponse = self.post(org, repo, routes::DIFF, &body).await?;
        Ok(response.files)
    }

    async fn log(&self, org: OrgId, repo: RepoId, q: &LogQuery) -> Result<Vec<CommitInfo>, IndexerLinkError> {
        let body = LogRequest { org_id: org, repo_id: repo, query: q.clone() };
        let response: LogResponse = self.post(org, repo, routes::LOG, &body).await?;
        Ok(response.commits)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use mage_core::wire::indexer::{IndexerErrorCode, PatternKind, TextHit};
    use serde_json::json;
    use time::macros::datetime;
    use uuid::Uuid;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn org() -> OrgId {
        OrgId::from_uuid(Uuid::from_u128(1))
    }

    fn repo() -> RepoId {
        RepoId::from_uuid(Uuid::from_u128(2))
    }

    fn key() -> RepoVersionKey {
        RepoVersionKey::new(repo(), CommitSha::from_bytes([0xab; 20]))
    }

    fn rp(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Minter counting calls; token `mgi_test.<n>`.
    fn minter() -> (MintIndexerToken, Arc<AtomicU32>) {
        let count = Arc::new(AtomicU32::new(0));
        let seen = count.clone();
        let mint: MintIndexerToken = Arc::new(move |o, r| {
            assert_eq!((o, r), (org(), repo()), "token claims carry the call's org and repo");
            let n = seen.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(format!("mgi_test.{n}"))
        });
        (mint, count)
    }

    async fn link(server: &MockServer) -> (HttpIndexerLink, Arc<AtomicU32>) {
        let (mint, count) = minter();
        let base = Url::parse(&server.uri()).unwrap_or_else(|e| panic!("{e}"));
        (HttpIndexerLink::new(base, mint, Duration::from_secs(2)).unwrap_or_else(|e| panic!("{e}")), count)
    }

    #[tokio::test]
    async fn search_posts_the_wire_request_with_a_bearer_token() {
        let server = MockServer::start().await;
        let hits = TextHits {
            hits: vec![TextHit { path: rp("api/conn.go"), line: 12, col: 3, text: "return nil".into(), before: vec![], after: vec![] }],
            next: Some((rp("api/conn.go"), 12)),
            truncated: None,
            files_scanned: 9,
        };
        Mock::given(method("POST"))
            .and(path(routes::SEARCH))
            .and(header("authorization", "Bearer mgi_test.1"))
            .and(body_partial_json(json!({
                "org_id": org().to_string(),
                "repo_id": repo().to_string(),
                "sha": key().sha.to_hex(),
                "include_generated": false,
                "query": { "pattern": "nil", "kind": "literal" }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(&hits))
            .expect(1)
            .mount(&server)
            .await;
        let (link, minted) = link(&server).await;
        let got = link.search(org(), &key(), &TextQuery::new("nil", PatternKind::Literal), false).await;
        assert_eq!(got, Ok(hits));
        assert_eq!(minted.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn statuses_map_to_link_errors() {
        let cases = [
            (400, IndexerErrorCode::Invalid, IndexerLinkError::Invalid("bad regex".into())),
            (404, IndexerErrorCode::NotFound, IndexerLinkError::NotFound),
            (503, IndexerErrorCode::Unavailable, IndexerLinkError::Unavailable("bad regex".into())),
        ];
        for (status, code, want) in cases {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path(routes::LOG))
                .respond_with(ResponseTemplate::new(status).set_body_json(IndexerError { code, message: "bad regex".into() }))
                .mount(&server)
                .await;
            let (link, _) = link(&server).await;
            let q = LogQuery { include: vec![key().sha], exclude: vec![], path_prefix: None, since: None, skip: 0, max: 3, with_files: false };
            assert_eq!(link.log(org(), repo(), &q).await, Err(want), "status {status}");
        }
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
        let (link, _) = link(&server).await;
        let got = link.blame(org(), &key(), &rp("a.go"), LineRange::new(1, 2).unwrap_or_else(|e| panic!("{e}"))).await;
        assert!(matches!(got, Err(IndexerLinkError::Unavailable(_))), "{got:?}");
    }

    #[tokio::test]
    async fn slow_indexer_times_out() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(routes::DIFF))
            .respond_with(ResponseTemplate::new(200).set_body_json(DiffResponse { files: vec![] }).set_delay(Duration::from_millis(1500)))
            .mount(&server)
            .await;
        let (mint, _) = minter();
        let base = Url::parse(&server.uri()).unwrap_or_else(|e| panic!("{e}"));
        let link = HttpIndexerLink::new(base, mint, Duration::from_millis(300)).unwrap_or_else(|e| panic!("{e}"));
        let opts = TreeDiffOptions { paths: vec![], hunks: false, context: 0, max_bytes_per_file: 0, renames: true };
        let got = link.diff(org(), repo(), CommitSha::from_bytes([1; 20]), key().sha, &opts).await;
        assert_eq!(got, Err(IndexerLinkError::Timeout));
    }

    #[tokio::test]
    async fn connect_errors_retry_once_with_a_fresh_token() {
        // Nothing listens on this port: both attempts fail to connect.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
        let addr = listener.local_addr().unwrap_or_else(|e| panic!("{e}"));
        drop(listener);
        let (mint, minted) = minter();
        let base = Url::parse(&format!("http://{addr}")).unwrap_or_else(|e| panic!("{e}"));
        let link = HttpIndexerLink::new(base, mint, Duration::from_secs(1)).unwrap_or_else(|e| panic!("{e}"));
        let q = LogQuery { include: vec![key().sha], exclude: vec![], path_prefix: None, since: None, skip: 0, max: 1, with_files: false };
        let got = link.log(org(), repo(), &q).await;
        assert!(matches!(got, Err(IndexerLinkError::Unavailable(_))), "{got:?}");
        assert_eq!(minted.load(Ordering::SeqCst), 2, "one retry, each attempt with its own token");
    }

    #[tokio::test]
    async fn blame_and_requests_beyond_limits_never_reach_the_indexer() {
        let server = MockServer::start().await;
        let hunk = BlameHunk {
            start: 1,
            lines: 2,
            commit: CommitSha::from_bytes([3; 20]),
            author_name: "Ada".into(),
            author_time: datetime!(2026-01-02 03:04:05 UTC),
            subject: "fix".into(),
            orig_path: rp("a.go"),
            orig_start: 1,
        };
        Mock::given(method("POST"))
            .and(path(routes::BLAME))
            .and(body_partial_json(json!({ "path": "a.go", "start_line": 1, "end_line": 2 })))
            .respond_with(ResponseTemplate::new(200).set_body_json(BlameResponse { hunks: vec![hunk.clone()] }))
            .expect(1)
            .mount(&server)
            .await;
        let (link, minted) = link(&server).await;
        let got = link.blame(org(), &key(), &rp("a.go"), LineRange::new(1, 2).unwrap_or_else(|e| panic!("{e}"))).await;
        assert_eq!(got, Ok(vec![hunk]));
        let too_many = LineRange::new(1, 201).unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(link.blame(org(), &key(), &rp("a.go"), too_many).await, Err(IndexerLinkError::Invalid(_))));
        let empty = TextQuery::new("", PatternKind::Literal);
        assert!(matches!(link.search(org(), &key(), &empty, false).await, Err(IndexerLinkError::Invalid(_))));
        assert_eq!(minted.load(Ordering::SeqCst), 1, "invalid requests mint no token");
    }
}
