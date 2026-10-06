//! 3.6 Target sha resolution (intake.md): ticket override → description directive → customer
//! environment deployment → default branch head (assumed), only upgrading an existing target.
//! Plus [`DeploymentChain`], the ordered `DeploymentResolver`s (`[pg, github]`).

use std::sync::Arc;

use mage_core::domain::{CommitSha, Deployment, OrgId, Repo, TargetOrigin, TicketTarget};
use mage_core::ports::source_host::{RepoCoords, SourceHostError};
use mage_core::ports::{RepoStore, SourceHost};
use time::OffsetDateTime;

use crate::app::intake::error::IntakeError;
use crate::ports::deployments::{DeploymentError, DeploymentResolver};

/// `Vec<Arc<dyn DeploymentResolver>>` asked in order; the first `Some` wins and an `Err` stops
/// the walk (an unknown answer never falls through to a weaker source).
pub struct DeploymentChain {
    resolvers: Vec<Arc<dyn DeploymentResolver>>,
}

impl DeploymentChain {
    pub fn new(resolvers: Vec<Arc<dyn DeploymentResolver>>) -> Self {
        Self { resolvers }
    }

    pub async fn deployed_sha(&self, org: OrgId, repo: &Repo, environment: &str, as_of: OffsetDateTime) -> Result<Option<Deployment>, DeploymentError> {
        for resolver in &self.resolvers {
            if let Some(d) = resolver.deployed_sha(org, repo, environment, as_of).await? {
                return Ok(Some(d));
            }
        }
        Ok(None)
    }
}

/// A `mage-sha:` / `mage-ref:` line of the description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Directive {
    /// 7–40 lowercase hex characters.
    Sha(String),
    /// Tag or branch name.
    Ref(String),
}

impl Directive {
    fn git_ref(&self) -> &str {
        match self {
            Self::Sha(s) | Self::Ref(s) => s,
        }
    }
}

/// First directive on its own line (surrounding whitespace allowed, case-insensitive keyword).
pub fn parse_directive(description: &str) -> Option<Directive> {
    description.lines().find_map(|line| {
        let line = line.trim();
        let (key, value) = line.split_once(':')?;
        let value = value.trim();
        if value.is_empty() || value.contains(char::is_whitespace) {
            return None;
        }
        match key.trim().to_ascii_lowercase().as_str() {
            "mage-sha" if (7..=40).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_hexdigit()) => {
                Some(Directive::Sha(value.to_ascii_lowercase()))
            }
            "mage-ref" if valid_ref_name(value) => Some(Directive::Ref(value.to_owned())),
            _ => None,
        }
    })
}

/// Conservative git ref-name check (no `..`, control chars, `~^:?*[\`, leading `-` or `/`).
fn valid_ref_name(name: &str) -> bool {
    name.len() <= 255
        && !name.starts_with(['-', '/', '.'])
        && !name.ends_with(['/', '.'])
        && !name.contains("..")
        && !name.contains("@{")
        && !name.ends_with(".lock")
        && name.chars().all(|c| !c.is_control() && !matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
}

/// Everything 3.6 needs about the ticket being ingested.
pub struct TargetInput<'a> {
    pub org: OrgId,
    /// Current target of an existing ticket.
    pub existing: Option<&'a TicketTarget>,
    /// Routed repo (3.5); `None` = no route.
    pub repo: Option<&'a Repo>,
    pub description: &'a str,
    pub directives: bool,
    /// Customer environment or the config default, aliases applied.
    pub environment: &'a str,
    /// The issue's `created_at`: what was deployed when the customer reported.
    pub as_of: OffsetDateTime,
}

fn rank(origin: &TargetOrigin) -> u8 {
    match origin {
        TargetOrigin::DefaultHead => 0,
        TargetOrigin::Deployment { .. } => 1,
        TargetOrigin::Directive => 2,
        TargetOrigin::Override => 3,
    }
}

pub struct TargetResolver {
    source: Arc<dyn SourceHost>,
    repos: Arc<dyn RepoStore>,
    chain: Arc<DeploymentChain>,
}

impl TargetResolver {
    pub fn new(source: Arc<dyn SourceHost>, repos: Arc<dyn RepoStore>, chain: Arc<DeploymentChain>) -> Self {
        Self { source, repos, chain }
    }

    /// Order and rules of intake.md 3.6. `Err` = the answer is unknown (GitHub or a deployment
    /// source is unreachable): the job retries and never yields an assumed target instead.
    pub async fn resolve(&self, input: &TargetInput<'_>) -> Result<Option<TicketTarget>, IntakeError> {
        // 1a. Override is sticky: only the UI changes it.
        if let Some(t) = input.existing.filter(|t| t.origin == TargetOrigin::Override) {
            return Ok(Some(t.clone()));
        }
        let Some(repo) = input.repo else { return Ok(None) };
        // An existing target only constrains the new one inside the same repo.
        let existing = input.existing.filter(|t| t.repo_id == repo.id);
        let candidate = self.candidate(input, repo, existing).await?;
        Ok(Some(match existing {
            // Never downgrade: a higher-ranked existing target stays.
            Some(old) if rank(&old.origin) > rank(&candidate.origin) => old.clone(),
            _ => candidate,
        }))
    }

    async fn candidate(&self, input: &TargetInput<'_>, repo: &Repo, existing: Option<&TicketTarget>) -> Result<TicketTarget, IntakeError> {
        let coords = RepoCoords { owner: repo.owner.clone(), name: repo.name.clone() };
        // 1b. Directive, resolved inside the routed repo only.
        if input.directives {
            if let Some(directive) = parse_directive(input.description) {
                match self.source.resolve_ref(repo.installation_id, &coords, directive.git_ref()).await {
                    Ok(sha) => return Ok(TicketTarget { repo_id: repo.id, sha, origin: TargetOrigin::Directive }),
                    Err(SourceHostError::NotFound | SourceHostError::Invalid(_)) => {
                        tracing::info!(org = %input.org, repo = %repo.id, directive = ?directive, "unresolvable target directive ignored");
                    }
                    Err(other) => return Err(other.into()),
                }
            }
        }
        // 2. Deployment of the customer's environment as of the report.
        match self.chain.deployed_sha(input.org, repo, input.environment, input.as_of).await? {
            Some(d) => {
                return Ok(TicketTarget { repo_id: repo.id, sha: d.sha, origin: TargetOrigin::Deployment { environment: d.environment } });
            }
            None => {
                // A lookup that now returns None keeps the existing deployment target.
                if let Some(old) = existing.filter(|t| matches!(t.origin, TargetOrigin::Deployment { .. })) {
                    return Ok(old.clone());
                }
            }
        }
        // 3. Default head, sticky once assumed.
        if let Some(old) = existing.filter(|t| t.origin == TargetOrigin::DefaultHead) {
            return Ok(old.clone());
        }
        let sha = self.default_head(repo, &coords).await?;
        Ok(TicketTarget { repo_id: repo.id, sha, origin: TargetOrigin::DefaultHead })
    }

    async fn default_head(&self, repo: &Repo, coords: &RepoCoords) -> Result<CommitSha, IntakeError> {
        if let Some(sha) = self.repos.default_head(repo.org_id, repo.id).await? {
            return Ok(sha);
        }
        Ok(self.source.resolve_ref(repo.installation_id, coords, &format!("heads/{}", repo.default_branch)).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directive_parsing() {
        assert_eq!(parse_directive("text\nmage-sha: 3F2A91C\nmore"), Some(Directive::Sha("3f2a91c".into())));
        assert_eq!(parse_directive("  MAGE-REF: v1.2.3  "), Some(Directive::Ref("v1.2.3".into())));
        assert_eq!(parse_directive("mage-ref: release/2026-10"), Some(Directive::Ref("release/2026-10".into())));
        // Not on its own line, too short, not hex, unsafe ref names.
        assert_eq!(parse_directive("see mage-sha: abcdef1 here"), None);
        assert_eq!(parse_directive("mage-sha: abc12"), None);
        assert_eq!(parse_directive("mage-sha: zzzzzzz"), None);
        assert_eq!(parse_directive("mage-ref: ../../etc"), None);
        assert_eq!(parse_directive("mage-ref: -delete"), None);
        assert_eq!(parse_directive("mage-ref: a b"), None);
        // First valid line wins.
        assert_eq!(parse_directive("mage-sha: nothex!\nmage-ref: main\nmage-sha: abcdef1"), Some(Directive::Ref("main".into())));
    }
}
