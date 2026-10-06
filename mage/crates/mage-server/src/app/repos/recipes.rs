//! Recipe history and Draft save (server.md `PUT …/repos/{repo}/recipe`; executor.md "Recipe").

use mage_core::domain::recipe::{IssueCode, RecipeIssue, RecipeSpec};
use mage_core::domain::{OrgId, Recipe, RepoId};
use mage_core::wire::api::{RecipeHistory, SaveRecipeRequest, SavedRecipe};

use super::{ReposService, collect_pages, repo_err};
use crate::error::AppError;

/// Findings that block saving: a value that looks like a live credential, or a secret-looking
/// env key with a real (not template, not fake-marked) value. Other validation errors are saved
/// with the Draft and block only the Boot gate and prepare (executor.md).
const SECRET_CODES: [IssueCode; 2] = [IssueCode::LiveSecret, IssueCode::NotFakeSecret];

impl ReposService {
    /// Current recipe (`latest_recipe`) and every other stored recipe, newest first.
    pub async fn recipe_history(&self, org: OrgId, repo: RepoId) -> Result<RecipeHistory, AppError> {
        self.repo(org, repo).await?;
        let current = self.repos.latest_recipe(org, repo).await.map_err(repo_err)?;
        let all = collect_pages(|page| self.repos.list_recipes(org, repo, page)).await.map_err(repo_err)?;
        let history = all.into_iter().filter(|r| current.as_ref().is_none_or(|c| c.hash != r.hash)).collect();
        Ok(RecipeHistory { current, history })
    }

    /// Parse, validate, hash and secret-scan `toml`, then store it as a `Draft`. Saving a body
    /// whose canonical form is already stored returns the stored recipe (its status kept).
    pub async fn save_recipe(&self, org: OrgId, repo: RepoId, toml: String) -> Result<SavedRecipe, AppError> {
        if toml.len() > SaveRecipeRequest::MAX_BYTES {
            return Err(AppError::Invalid(format!("recipe is larger than {} bytes", SaveRecipeRequest::MAX_BYTES)));
        }
        self.repo(org, repo).await?;
        let spec = RecipeSpec::parse(&toml).map_err(|e| AppError::Invalid(format!("recipe: {e}")))?;
        let report = spec.validate();
        let secrets: Vec<&RecipeIssue> = report.errors.iter().filter(|i| SECRET_CODES.contains(&i.code)).collect();
        if !secrets.is_empty() {
            return Err(AppError::Invalid(format!("recipe rejected, secrets found: {}", issue_list(&secrets))));
        }
        let draft = Recipe::draft(org, repo, toml)?;
        let recipe = if self.repos.put_recipe(org, &draft).await.map_err(repo_err)? {
            draft
        } else {
            self.repos.get_recipe(org, repo, draft.hash).await.map_err(repo_err)?
        };
        Ok(SavedRecipe { recipe, report })
    }
}

/// `path (Code): message; …`. Messages name the rule, never the matched value.
fn issue_list(issues: &[&RecipeIssue]) -> String {
    issues
        .iter()
        .map(|i| format!("{} ({:?}): {}", if i.path.is_empty() { "<document>" } else { &i.path }, i.code, i.message))
        .collect::<Vec<_>>()
        .join("; ")
}
