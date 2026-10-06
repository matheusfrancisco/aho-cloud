//! UI API: repos, versions, recipes, deployments, customers (server.md "Endpoint catalog").
//! Session, `{org}` → `OrgCtx` (404 for non-members) and CSRF on mutations are Platform
//! middleware; handlers check the minimum role of their row.

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use mage_core::domain::{Customer, CustomerId, Deployment, MemberRole, Repo, RepoId};
use mage_core::wire::api::routes;
use mage_core::wire::api::{
    ConnectRepoRequest, CustomerInput, CustomerList, DeploymentList, IndexVersionRequest, InstallationRepoList, JobQueued,
    RecipeHistory, RepoDetail, RepoList, SaveRecipeRequest, SavedRecipe, SetDeploymentRequest,
};

use crate::adapters::driving::http::auth::OrgCtx;
use crate::app::repos::ReposService;
use crate::error::AppError;

#[derive(Clone)]
pub struct ReposState {
    pub service: Arc<ReposService>,
}

pub fn router(state: ReposState) -> Router {
    Router::new()
        .route(routes::REPOS, get(list_repos).post(connect_repo))
        .route(routes::REPO, get(repo_detail).delete(disconnect_repo))
        .route(routes::GITHUB_REPOSITORIES, get(installation_repos))
        .route(routes::REPO_VERSIONS, post(index_version))
        .route(routes::REPO_RECIPES, get(recipe_history))
        .route(routes::REPO_RECIPE, put(save_recipe).layer(DefaultBodyLimit::max(SaveRecipeRequest::MAX_BYTES)))
        .route(routes::REPO_DEPLOYMENTS, get(list_deployments))
        .route(routes::REPO_DEPLOYMENT, put(set_deployment))
        .route(routes::CUSTOMERS, get(list_customers).post(create_customer))
        .route(routes::CUSTOMER, put(update_customer))
        .with_state(state)
}

type ApiResult<T> = Result<Json<T>, AppError>;

async fn list_repos(State(s): State<ReposState>, ctx: OrgCtx) -> ApiResult<RepoList> {
    Ok(Json(s.service.list_repos(ctx.org_id).await?))
}

async fn repo_detail(State(s): State<ReposState>, ctx: OrgCtx, Path((_, repo)): Path<(String, RepoId)>) -> ApiResult<RepoDetail> {
    Ok(Json(s.service.repo_detail(ctx.org_id, repo).await?))
}

async fn installation_repos(State(s): State<ReposState>, ctx: OrgCtx) -> ApiResult<InstallationRepoList> {
    ctx.require(MemberRole::Admin)?;
    Ok(Json(s.service.installation_repos(ctx.org_id).await?))
}

async fn connect_repo(State(s): State<ReposState>, ctx: OrgCtx, Json(req): Json<ConnectRepoRequest>) -> ApiResult<Repo> {
    ctx.require(MemberRole::Admin)?;
    Ok(Json(s.service.connect(ctx.org_id, req.external_id).await?))
}

async fn disconnect_repo(State(s): State<ReposState>, ctx: OrgCtx, Path((_, repo)): Path<(String, RepoId)>) -> Result<StatusCode, AppError> {
    ctx.require(MemberRole::Admin)?;
    s.service.disconnect(ctx.org_id, repo).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn index_version(
    State(s): State<ReposState>,
    ctx: OrgCtx,
    Path((_, repo)): Path<(String, RepoId)>,
    Json(req): Json<IndexVersionRequest>,
) -> ApiResult<JobQueued> {
    Ok(Json(s.service.index_rev(ctx.org_id, repo, &req.rev).await?))
}

async fn recipe_history(State(s): State<ReposState>, ctx: OrgCtx, Path((_, repo)): Path<(String, RepoId)>) -> ApiResult<RecipeHistory> {
    Ok(Json(s.service.recipe_history(ctx.org_id, repo).await?))
}

async fn save_recipe(
    State(s): State<ReposState>,
    ctx: OrgCtx,
    Path((_, repo)): Path<(String, RepoId)>,
    Json(req): Json<SaveRecipeRequest>,
) -> ApiResult<SavedRecipe> {
    ctx.require(MemberRole::Admin)?;
    Ok(Json(s.service.save_recipe(ctx.org_id, repo, req.toml).await?))
}

async fn list_deployments(State(s): State<ReposState>, ctx: OrgCtx, Path((_, repo)): Path<(String, RepoId)>) -> ApiResult<DeploymentList> {
    Ok(Json(s.service.list_deployments(ctx.org_id, repo).await?))
}

async fn set_deployment(
    State(s): State<ReposState>,
    ctx: OrgCtx,
    Path((_, repo, environment)): Path<(String, RepoId, String)>,
    Json(req): Json<SetDeploymentRequest>,
) -> ApiResult<Deployment> {
    ctx.require(MemberRole::Admin)?;
    Ok(Json(s.service.set_deployment(ctx.org_id, repo, &environment, req.sha).await?))
}

async fn list_customers(State(s): State<ReposState>, ctx: OrgCtx) -> ApiResult<CustomerList> {
    Ok(Json(s.service.list_customers(ctx.org_id).await?))
}

async fn create_customer(State(s): State<ReposState>, ctx: OrgCtx, Json(input): Json<CustomerInput>) -> ApiResult<Customer> {
    ctx.require(MemberRole::Admin)?;
    Ok(Json(s.service.create_customer(ctx.org_id, input).await?))
}

async fn update_customer(
    State(s): State<ReposState>,
    ctx: OrgCtx,
    Path((_, customer)): Path<(String, CustomerId)>,
    Json(input): Json<CustomerInput>,
) -> ApiResult<Customer> {
    ctx.require(MemberRole::Admin)?;
    Ok(Json(s.service.update_customer(ctx.org_id, customer, input).await?))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use axum::body::Body;
    use axum::http::{Method, Request};
    use mage_core::domain::{RepoAccess, UserId};
    use mage_core::ports::RepoStore;
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;
    use crate::app::repos::tests::{Fixture, WEB};

    fn app(f: &Fixture, role: MemberRole) -> Router {
        let ctx = OrgCtx { org_id: f.org, user_id: UserId::from_uuid(Uuid::from_u128(5)), role };
        router(ReposState { service: f.svc.clone() }).layer(axum::Extension(ctx))
    }

    fn path(template: &str, repo: Option<RepoId>) -> String {
        let p = template.replace("{org}", "acme");
        match repo {
            Some(repo) => p.replace("{repo}", &repo.to_string()),
            None => p,
        }
    }

    async fn call(app: Router, method: Method, uri: &str, body: Option<serde_json::Value>) -> (StatusCode, serde_json::Value) {
        let req = Request::builder().method(method).uri(uri).header("content-type", "application/json");
        let req = req.body(body.map_or_else(Body::empty, |b| Body::from(b.to_string()))).unwrap();
        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    #[tokio::test]
    async fn members_read_and_index_but_admin_rows_are_forbidden() {
        let f = Fixture::new().await;
        let repo = f.svc.connect(f.org, WEB).await.unwrap();
        let member = || app(&f, MemberRole::Member);
        let repo_path = path(routes::REPO, Some(repo.id));

        assert_eq!(call(member(), Method::GET, &path(routes::REPOS, None), None).await.0, StatusCode::OK);
        assert_eq!(call(member(), Method::GET, &repo_path, None).await.0, StatusCode::OK);
        assert_eq!(call(member(), Method::GET, &path(routes::CUSTOMERS, None), None).await.0, StatusCode::OK);
        let (status, body) =
            call(member(), Method::POST, &path(routes::REPO_VERSIONS, Some(repo.id)), Some(serde_json::json!({"rev": "v1.0.0"}))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.get("job_id").is_some(), "{body}");

        let deployment = path(routes::REPO_DEPLOYMENT, Some(repo.id)).replace("{environment}", "prod");
        let forbidden = [
            (Method::GET, path(routes::GITHUB_REPOSITORIES, None), None),
            (Method::POST, path(routes::REPOS, None), Some(serde_json::json!({"external_id": WEB}))),
            (Method::DELETE, repo_path.clone(), None),
            (Method::PUT, path(routes::REPO_RECIPE, Some(repo.id)), Some(serde_json::json!({"toml": "version = 1\n"}))),
            (Method::PUT, deployment, Some(serde_json::json!({"sha": "0101010101010101010101010101010101010101"}))),
            (Method::POST, path(routes::CUSTOMERS, None), Some(serde_json::json!({"name": "Acme", "external_refs": []}))),
        ];
        for (method, uri, body) in forbidden {
            assert_eq!(call(member(), method.clone(), &uri, body).await.0, StatusCode::FORBIDDEN, "{method} {uri}");
        }
        assert_eq!(f.repos.repo(f.org, repo.id).await.unwrap().access, RepoAccess::Granted, "the member's delete did nothing");
    }

    #[tokio::test]
    async fn admin_connects_disconnects_and_saves_recipes() {
        let f = Fixture::new().await;
        let admin = || app(&f, MemberRole::Admin);
        let (status, body) = call(admin(), Method::POST, &path(routes::REPOS, None), Some(serde_json::json!({"external_id": WEB}))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let repo: Repo = serde_json::from_value(body).unwrap();
        assert_eq!(f.index_jobs().len(), 1);

        let (status, body) =
            call(admin(), Method::PUT, &path(routes::REPO_RECIPE, Some(repo.id)), Some(serde_json::json!({"toml": "version = 1\n"}))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let saved: SavedRecipe = serde_json::from_value(body).unwrap();
        let (_, body) = call(admin(), Method::GET, &path(routes::REPO_RECIPES, Some(repo.id)), None).await;
        let history: RecipeHistory = serde_json::from_value(body).unwrap();
        assert_eq!(history.current.map(|r| r.hash), Some(saved.recipe.hash));

        let secret = serde_json::json!({"toml": "version = 1\n[env]\nGH = \"ghp_0123456789abcdefghij0123456789abcdef\"\n"});
        assert_eq!(call(admin(), Method::PUT, &path(routes::REPO_RECIPE, Some(repo.id)), Some(secret)).await.0, StatusCode::UNPROCESSABLE_ENTITY);

        assert_eq!(call(admin(), Method::DELETE, &path(routes::REPO, Some(repo.id)), None).await.0, StatusCode::NO_CONTENT);
        assert_eq!(f.repos.repo(f.org, repo.id).await.unwrap().access, RepoAccess::Lost);
    }
}
