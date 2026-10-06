//! A GitHub REST emulator serving a `SourceHostWorld` under `/api/v3` (a GitHub Enterprise style
//! path prefix): app JWT checks, scoped installation tokens, repositories (one per page, so
//! clients must follow `Link`), commits by ref or sha prefix, pull requests with paging.
#![allow(clippy::result_large_err)] // `Err` is the emulator's HTTP response

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::ids::{CommitSha, InstallationId};
use mage_core::ports::clock::Clock;
use mage_core::ports::source_host::{Access, AccountKind, RepoSelection};
use mage_core::testing::{FakeClock, SourceHostWorld, WorldInstallation, WorldPullRequest, WorldRepo};
use parking_lot::Mutex;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::Url;
use wiremock::{Request, Respond, ResponseTemplate};

use super::{bearer, verify_app_jwt};

pub const PREFIX: &str = "/api/v3";

#[derive(Clone)]
pub struct Emulator {
    clock: Arc<FakeClock>,
    /// The server's own origin: wiremock's request URLs name `localhost`.
    origin: Url,
    state: Arc<Mutex<State>>,
}

struct State {
    world: SourceHostWorld,
    tokens: HashMap<String, Grant>,
    minted: usize,
}

struct Grant {
    inst: InstallationId,
    repositories: Option<Vec<String>>,
    expires_at: OffsetDateTime,
}

fn reply(status: u16, body: Value) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(body)
}

fn message(status: u16, text: &str) -> ResponseTemplate {
    reply(status, json!({ "message": text, "documentation_url": "https://docs.github.com/rest" }))
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap()
}

fn level(access: Access) -> &'static str {
    match access {
        Access::Read => "read",
        Access::Write => "write",
    }
}

fn permissions_json(permissions: &BTreeMap<String, Access>) -> Value {
    permissions.iter().map(|(name, access)| (name.clone(), json!(level(*access)))).collect::<serde_json::Map<_, _>>().into()
}

/// Percent-decodes a path segment.
fn decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&segment[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

/// `request`'s URL on `origin` with `page` replaced.
fn page_url(origin: &Url, request: &Request, page: usize) -> String {
    let mut url = origin.join(request.url.path()).unwrap();
    url.set_query(request.url.query());
    let pairs: Vec<(String, String)> = url.query_pairs().filter(|(k, _)| k != "page").map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    url.query_pairs_mut().clear().extend_pairs(pairs).append_pair("page", &page.to_string());
    url.to_string()
}

/// One page of `items` (pages from 1) with GitHub's `Link` header.
fn paged(origin: &Url, request: &Request, items: Vec<Value>, per_page: usize, page: usize, wrap: impl FnOnce(Vec<Value>) -> Value) -> ResponseTemplate {
    let total = items.len();
    let start = (page - 1) * per_page;
    let slice: Vec<Value> = items.into_iter().skip(start).take(per_page).collect();
    let mut response = reply(200, wrap(slice));
    let last = total.div_ceil(per_page).max(1);
    let mut links = Vec::new();
    if page < last {
        links.push(format!("<{}>; rel=\"next\"", page_url(origin, request, page + 1)));
        links.push(format!("<{}>; rel=\"last\"", page_url(origin, request, last)));
    }
    if page > 1 {
        links.push(format!("<{}>; rel=\"prev\"", page_url(origin, request, page - 1)));
    }
    if !links.is_empty() {
        response = response.insert_header("link", links.join(", "));
    }
    response
}

fn query(request: &Request, name: &str) -> Option<String> {
    request.url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned())
}

fn repo_json(repo: &WorldRepo) -> Value {
    let r = &repo.remote;
    json!({
        "id": r.external_id,
        "node_id": "R_x",
        "name": r.coords.name,
        "full_name": format!("{}/{}", r.coords.owner, r.coords.name),
        "owner": { "login": r.coords.owner, "type": "Organization" },
        "default_branch": r.default_branch,
        "private": r.private,
        "archived": r.archived,
        "html_url": format!("https://github.com/{}/{}", r.coords.owner, r.coords.name),
    })
}

fn pr_json(pr: &WorldPullRequest) -> Value {
    let info = &pr.info;
    // GitHub reports a test merge commit for unmerged pull requests too.
    let merge_commit_sha = info.merge_commit_sha.map_or_else(|| "f".repeat(40), |sha| sha.to_hex());
    json!({
        "number": info.number,
        "state": if pr.open { "open" } else { "closed" },
        "title": info.title,
        "body": info.body_head,
        "head": { "ref": info.head_ref, "sha": "1".repeat(40) },
        "base": { "ref": info.base_ref, "sha": "2".repeat(40) },
        "merge_commit_sha": merge_commit_sha,
        "merged_at": info.merged_at.map(rfc3339),
        "updated_at": rfc3339(info.updated_at),
        "html_url": info.url,
        "user": { "login": info.author_login },
    })
}

fn resolve(repo: &WorldRepo, git_ref: &str) -> Result<CommitSha, ResponseTemplate> {
    let branch = git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref);
    if let Some(sha) = repo.branches.get(branch) {
        return Ok(*sha);
    }
    let tag = git_ref.strip_prefix("refs/tags/").unwrap_or(git_ref);
    if let Some(sha) = repo.tags.get(tag) {
        return Ok(*sha);
    }
    let not_found = || message(422, &format!("No commit found for SHA: {git_ref}"));
    let hex = git_ref.to_ascii_lowercase();
    if hex.len() < 4 || hex.len() > 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(not_found());
    }
    let matches: Vec<CommitSha> = repo.known_commits().into_iter().filter(|sha| sha.to_hex().starts_with(&hex)).collect();
    match matches.as_slice() {
        [] => Err(not_found()),
        [sha] => Ok(*sha),
        _ => Err(message(422, &format!("The sha prefix {git_ref} is ambiguous"))),
    }
}

impl Emulator {
    pub fn new(clock: Arc<FakeClock>, world: SourceHostWorld, server_uri: &str) -> Self {
        Self { clock, origin: Url::parse(server_uri).unwrap(), state: Arc::new(Mutex::new(State { world, tokens: HashMap::new(), minted: 0 })) }
    }

    fn installation_json(installation: &WorldInstallation) -> Value {
        let info = &installation.info;
        json!({
            "id": info.id.0,
            "account": {
                "login": info.account_login,
                "type": match info.account_kind { AccountKind::User => "User", AccountKind::Organization => "Organization" },
            },
            "app_id": 1,
            "target_type": "Organization",
            "permissions": permissions_json(&info.permissions),
            "events": ["push"],
            "repository_selection": match info.repository_selection { RepoSelection::All => "all", RepoSelection::Selected => "selected" },
            "suspended_at": info.suspended.then_some("2025-12-01T00:00:00Z"),
        })
    }

    fn mint(&self, state: &mut State, inst: InstallationId, request: &Request) -> ResponseTemplate {
        let Some(installation) = state.world.installation(inst) else { return message(404, "Not Found") };
        if installation.info.suspended {
            return message(403, "This installation has been suspended");
        }
        let body: Value = if request.body.is_empty() { json!({}) } else { request.body_json().unwrap() };
        let repositories: Option<Vec<String>> = body.get("repositories").map(|r| serde_json::from_value(r.clone()).unwrap());
        if let Some(names) = &repositories
            && names.iter().any(|name| !installation.repos.iter().any(|r| r.remote.coords.name.eq_ignore_ascii_case(name)))
        {
            return message(422, "There is at least one repository that does not exist or is not accessible to the parent installation.");
        }
        let mut requested = BTreeMap::new();
        if let Some(Value::Object(map)) = body.get("permissions") {
            for (name, value) in map {
                let access = match value.as_str() {
                    Some("read") => Access::Read,
                    Some("write") => Access::Write,
                    _ => return message(422, &format!("Invalid permission level for {name}")),
                };
                if installation.info.permissions.get(name).is_none_or(|granted| *granted < access) {
                    return message(422, "The permissions requested are not granted to this installation.");
                }
                requested.insert(name.clone(), access);
            }
        }
        let effective = if requested.is_empty() { installation.info.permissions.clone() } else { requested };
        state.minted += 1;
        let token = format!("ghs_emu_{}", state.minted);
        let expires_at = self.clock.now() + Duration::from_secs(3600);
        state.tokens.insert(token.clone(), Grant { inst, repositories: repositories.clone(), expires_at });
        reply(201, json!({
            "token": token,
            "expires_at": rfc3339(expires_at),
            "permissions": permissions_json(&effective),
            "repository_selection": if repositories.is_some() { "selected" } else { "all" },
        }))
    }

    /// The installation and repo filter of a valid installation token.
    fn grant<'a>(&self, state: &'a State, request: &Request) -> Result<(&'a WorldInstallation, Option<&'a [String]>), ResponseTemplate> {
        let grant = bearer(request)
            .and_then(|token| state.tokens.get(&token))
            .filter(|g| g.expires_at > self.clock.now())
            .ok_or_else(|| message(401, "Bad credentials"))?;
        let installation = state.world.installation(grant.inst).ok_or_else(|| message(401, "Bad credentials"))?;
        Ok((installation, grant.repositories.as_deref()))
    }

    fn visible_repo<'a>(installation: &'a WorldInstallation, allowed: Option<&[String]>, owner: &str, name: &str) -> Option<&'a WorldRepo> {
        let repo = installation.repos.iter().find(|r| r.remote.coords.owner.eq_ignore_ascii_case(owner) && r.remote.coords.name.eq_ignore_ascii_case(name))?;
        let visible = allowed.is_none_or(|names| names.iter().any(|n| n.eq_ignore_ascii_case(&repo.remote.coords.name)));
        visible.then_some(repo)
    }

    fn pulls(&self, request: &Request, repo: &WorldRepo) -> ResponseTemplate {
        let state = query(request, "state").unwrap_or_else(|| "open".into());
        let sort = query(request, "sort").unwrap_or_else(|| "created".into());
        let direction = query(request, "direction").unwrap_or_else(|| "desc".into());
        let per_page: usize = query(request, "per_page").map_or(30, |p| p.parse().unwrap());
        let page: usize = query(request, "page").map_or(1, |p| p.parse().unwrap());
        if per_page == 0 || per_page > 100 || page == 0 {
            return message(422, "Invalid paging");
        }
        let mut prs: Vec<&WorldPullRequest> = repo
            .pull_requests
            .iter()
            .filter(|pr| match state.as_str() {
                "open" => pr.open,
                "closed" => !pr.open,
                _ => true,
            })
            .collect();
        match sort.as_str() {
            "updated" => prs.sort_by_key(|pr| (pr.info.updated_at, pr.info.number)),
            _ => prs.sort_by_key(|pr| pr.info.number),
        }
        if direction == "desc" {
            prs.reverse();
        }
        paged(&self.origin, request, prs.into_iter().map(pr_json).collect(), per_page, page, Value::Array)
    }

    fn route(&self, request: &Request) -> ResponseTemplate {
        let Some(path) = request.url.path().strip_prefix(PREFIX) else { return message(404, "Not Found") };
        let segments: Vec<String> = path.trim_start_matches('/').split('/').map(decode).collect();
        let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
        let method = request.method.as_str();
        let mut state = self.state.lock();

        if let ["app", "installations", id, rest @ ..] = segments.as_slice() {
            let now = self.clock.now();
            if let Err(e) = bearer(request).ok_or_else(|| "no bearer".to_owned()).and_then(|jwt| verify_app_jwt(&jwt, now)) {
                return message(401, &format!("A JSON web token could not be decoded: {e}"));
            }
            let Ok(inst) = id.parse().map(InstallationId) else { return message(404, "Not Found") };
            return match (method, rest) {
                ("GET", []) => state.world.installation(inst).map_or_else(|| message(404, "Not Found"), |i| reply(200, Self::installation_json(i))),
                ("POST", ["access_tokens"]) => self.mint(&mut state, inst, request),
                _ => message(404, "Not Found"),
            };
        }

        let (installation, allowed) = match self.grant(&state, request) {
            Ok(grant) => grant,
            Err(response) => return response,
        };
        match (method, segments.as_slice()) {
            ("GET", ["installation", "repositories"]) => {
                let page: usize = query(request, "page").map_or(1, |p| p.parse().unwrap());
                let repos: Vec<Value> = installation
                    .repos
                    .iter()
                    .filter(|r| allowed.is_none_or(|names| names.iter().any(|n| n.eq_ignore_ascii_case(&r.remote.coords.name))))
                    .map(repo_json)
                    .collect();
                let total = repos.len();
                // One repo per page whatever `per_page` says: the client must follow `Link`.
                paged(&self.origin, request, repos, 1, page, |items| json!({ "total_count": total, "repositories": items }))
            }
            ("GET", ["repos", owner, name, rest @ ..]) => {
                let Some(repo) = Self::visible_repo(installation, allowed, owner, name) else { return message(404, "Not Found") };
                match rest {
                    [] => reply(200, repo_json(repo)),
                    ["pulls"] => self.pulls(request, repo),
                    ["commits", git_ref @ ..] if !git_ref.is_empty() => match resolve(repo, &git_ref.join("/")) {
                        Ok(sha) => reply(200, json!({ "sha": sha.to_hex(), "commit": { "message": "m" }, "files": [] })),
                        Err(response) => response,
                    },
                    _ => message(404, "Not Found"),
                }
            }
            _ => message(404, "Not Found"),
        }
    }
}

impl Respond for Emulator {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let header = |name: &str| request.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
        if header("accept") != "application/vnd.github+json" || header("x-github-api-version") != "2022-11-28" || header("user-agent") != "ahomage" {
            return message(400, "missing GitHub API headers");
        }
        self.route(request)
    }
}
