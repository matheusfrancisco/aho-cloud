//! `similar_tickets` (Memory) and `run_recipe` (Recipe), knowledge.md. Neither needs `Files`.

use std::collections::BTreeSet;

use mage_core::domain::recipe::RecipeStatus;
use mage_core::domain::{Layer, RepoPath, RunSubject, TicketId};
use serde_json::{Value, json};

use super::super::envelope::{Envelope, ToolError};
use super::super::inputs::{RunRecipeInput, SimilarTicketsInput, check_len, check_range};
use super::super::scope::KnowledgeScope;
use super::super::service::{Service, check_scope};
use super::super::text::head;
use super::rfc3339;
use crate::domain::knowledge::{AnswerSource, SimilarMatch};
use crate::error::AppError;
use crate::ports::ticket::SimilarQuery;

const ROOT_CAUSE_MAX_CHARS: usize = 1024;
const RECIPE_MAX_BYTES: usize = 16 * 1024;

impl Service {
    pub(crate) async fn similar_tickets(&self, scope: &KnowledgeScope, input: SimilarTicketsInput) -> Result<Envelope, ToolError> {
        check_scope(scope, input.repo.as_deref(), input.sha.as_deref())?;
        check_range("limit", input.limit, 1, 10)?;
        if let Some(text) = &input.text {
            check_len("text", text, 1, 4096)?;
        }
        let (text, error_strings) = match input.text {
            Some(text) => (text, Vec::new()),
            None => self.run_ticket_text(scope).await?,
        };
        let q = SimilarQuery {
            repos: vec![scope.repo],
            text,
            error_strings,
            exclude: scope.memory.exclude_tickets.clone(),
            as_of: scope.memory.as_of,
        };
        let limit = u16::try_from(input.limit).unwrap_or(10);
        let found = self.tickets.similar_resolved(scope.org, &q, limit).await.map_err(AppError::from)?;
        // The store filters too; the memory cutoff must hold even if it did not.
        let found: Vec<_> = found
            .into_iter()
            .filter(|s| !scope.memory.exclude_tickets.contains(&s.ticket) && scope.memory.as_of.is_none_or(|as_of| s.resolved_at < as_of))
            .collect();

        // Which cited paths still exist at the scope sha (null when Files is not Ready there).
        let paths: Vec<RepoPath> = found
            .iter()
            .flat_map(|s| s.cause_citations.iter())
            .filter(|c| c.repo_id == scope.repo)
            .map(|c| c.path.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let existing: Option<BTreeSet<RepoPath>> = match self.repos.version(scope.org, scope.version()).await {
            Ok(version) if !scope.layer_disabled(Layer::Files) && version.layer(Layer::Files).is_some_and(|s| s.is_ready()) => {
                let entries = if paths.is_empty() { Vec::new() } else { self.knowledge.manifest_entries(scope.org, &scope.version(), &paths).await? };
                Some(entries.into_iter().map(|e| e.path).collect())
            }
            _ => None,
        };

        let mut env = Envelope::new(scope.repo, scope.sha, AnswerSource::Memory);
        if existing.is_none() {
            env.note("exists_at_scope_sha is null: the scope sha's Files layer is not Ready");
        }
        env.items = found
            .iter()
            .map(|s| {
                let citations: Vec<Value> = s
                    .cause_citations
                    .iter()
                    .map(|c| {
                        let mut cite = super::super::envelope::citation(&c.path, c.lines.start(), c.lines.end(), c.sha);
                        cite["exists_at_scope_sha"] = match &existing {
                            Some(paths) => json!(c.repo_id == scope.repo && paths.contains(&c.path)),
                            None => Value::Null,
                        };
                        cite
                    })
                    .collect();
                let matched_on: Vec<&str> = s
                    .matched_on
                    .iter()
                    .map(|m| match m {
                        SimilarMatch::Text => "text",
                        SimilarMatch::ErrorString => "error_string",
                    })
                    .collect();
                let mut item = json!({
                    "ticket": { "identifier": s.identifier, "title": s.title, "url": s.url, "resolved_at": rfc3339(s.resolved_at) },
                    "similarity": s.score,
                    "matched_on": matched_on,
                    "root_cause": head(&s.root_cause, ROOT_CAUSE_MAX_CHARS),
                    "cause_citations": citations,
                });
                if let Some(url) = &s.fix_pr_url {
                    item["fix_pr_url"] = json!(url);
                }
                item
            })
            .collect();
        Ok(env)
    }

    /// Title + body and error strings of the ticket behind the token's run.
    async fn run_ticket_text(&self, scope: &KnowledgeScope) -> Result<(String, Vec<String>), ToolError> {
        let mut subject = self.runs.get(scope.org, scope.run).await.map_err(AppError::from)?.run.subject;
        if let RunSubject::Run(parent) = subject {
            subject = self.runs.get(scope.org, parent).await.map_err(AppError::from)?.run.subject;
        }
        let ticket: TicketId = match subject {
            RunSubject::Ticket(ticket) => ticket,
            RunSubject::Cause(cause) => self.tickets.get_cause(scope.org, cause).await.map_err(AppError::from)?.ticket_id,
            RunSubject::RepoVersion(_) | RunSubject::Run(_) => {
                return Err(ToolError::invalid("this run has no ticket; pass `text`"));
            }
        };
        let ticket = self.tickets.get_ticket(scope.org, ticket).await.map_err(AppError::from)?.value;
        Ok((format!("{}\n\n{}", ticket.title, ticket.body), ticket.error_strings.clone()))
    }

    pub(crate) async fn run_recipe(&self, scope: &KnowledgeScope, input: RunRecipeInput) -> Result<Envelope, ToolError> {
        check_scope(scope, input.repo.as_deref(), None)?;
        let mut env = Envelope::new(scope.repo, scope.sha, AnswerSource::Recipe);
        match self.repos.latest_recipe(scope.org, scope.repo).await? {
            None => env.set("status", json!("none")),
            Some(recipe) => {
                let status = match recipe.status {
                    RecipeStatus::Draft => "draft",
                    RecipeStatus::Validated { .. } => "validated",
                    RecipeStatus::Broken { .. } => "broken",
                };
                let mut toml = recipe.body.clone();
                if toml.len() > RECIPE_MAX_BYTES {
                    let mut end = RECIPE_MAX_BYTES;
                    while !toml.is_char_boundary(end) {
                        end -= 1;
                    }
                    toml.truncate(end);
                    env.truncated = true;
                    env.note(format!("recipe cut at {RECIPE_MAX_BYTES} bytes"));
                }
                env.set("status", json!(status));
                env.set("hash", json!(recipe.hash));
                env.set("validated_at_sha", json!(recipe.validated_at_sha));
                env.set("toml", Value::String(toml));
            }
        }
        Ok(env)
    }
}
