//! Ports to the other services, each with its real HTTP adapter against
//! `cloud_api::internal::routes` (service-token auth). In-memory fakes live in [`crate::fakes`].

use std::time::Duration;

use async_trait::async_trait;
use cloud_api::ApiError;
use cloud_api::internal::{
    BuildJob, BuildResult, Deployment, DeploymentStatus, Maintenance, Scale, StartWorkspace, WorkspaceFileRef, WorkspacePackage,
    WorkspaceRef, WorkspaceStarted, routes,
};
use cloud_api::{FileContent, FileTree};
use cloud_contract::ledger::Stage;
use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Debug, thiserror::Error)]
pub enum PortError {
    #[error("{service}: {message}")]
    Transport { service: &'static str, message: String },
    #[error("{service} answered {status}: {message}")]
    Status { service: &'static str, status: u16, message: String },
    #[error("{service}: unreadable response: {message}")]
    Decode { service: &'static str, message: String },
    /// The service has no such thing (no workspace, no file at that path).
    #[error("{service}: {message}")]
    NotFound { service: &'static str, message: String },
    /// What was asked for is over the service's limits.
    #[error("{service}: too large: {message}")]
    TooLarge { service: &'static str, message: String },
    /// The service refused the request itself (e.g. a path escaping the workspace).
    #[error("{service}: {message}")]
    Rejected { service: &'static str, message: String },
}

impl PortError {
    /// Reads a 4xx answer as the caller's problem: 404 → [`PortError::NotFound`], 413 →
    /// [`PortError::TooLarge`], any other 4xx but 401/403 (the service token, i.e. ours) →
    /// [`PortError::Rejected`]. Everything else is unchanged.
    pub fn classify(self) -> Self {
        match self {
            PortError::Status { service, status: 404, message } => PortError::NotFound { service, message },
            PortError::Status { service, status: 413, message } => PortError::TooLarge { service, message },
            PortError::Status { service, status, message } if (400..500).contains(&status) && status != 401 && status != 403 => {
                PortError::Rejected { service, message }
            }
            other => other,
        }
    }
}

/// Source → image. Synchronous from the caller's point of view; may take minutes.
#[async_trait]
pub trait BuildService: Send + Sync {
    async fn build(&self, job: BuildJob) -> Result<BuildResult, PortError>;
}

/// The runtime agents: desired state per `(project, stage)`.
#[async_trait]
pub trait RuntimeService: Send + Sync {
    async fn deploy(&self, deployment: Deployment) -> Result<DeploymentStatus, PortError>;
    /// `None` when nothing runs there.
    async fn status(&self, project: &str, stage: Stage) -> Result<Option<DeploymentStatus>, PortError>;
    async fn restart(&self, project: &str, stage: Stage) -> Result<(), PortError>;
    async fn scale(&self, project: &str, stage: Stage, scale: Scale) -> Result<(), PortError>;
    async fn maintenance(&self, project: &str, stage: Stage, maintenance: Maintenance) -> Result<(), PortError>;
    async fn sleep(&self, project: &str, stage: Stage) -> Result<(), PortError>;
}

/// Entry A: aho workspaces, one per `(project, user)`.
#[async_trait]
pub trait SessionHostService: Send + Sync {
    async fn start(&self, request: StartWorkspace) -> Result<WorkspaceStarted, PortError>;
    /// The workspace packed for a deploy: archive, newest session log, `deploy.json`, draft.
    async fn package(&self, workspace: WorkspaceRef) -> Result<WorkspacePackage, PortError>;
    /// What a deploy of the workspace would ship.
    async fn files(&self, workspace: WorkspaceRef) -> Result<FileTree, PortError>;
    /// One file of the workspace.
    async fn file(&self, file: WorkspaceFileRef) -> Result<FileContent, PortError>;
}

/// An authenticated JSON client for one internal service.
#[derive(Clone)]
pub struct ServiceClient {
    service: &'static str,
    http: reqwest::Client,
    base: String,
    token: String,
}

impl ServiceClient {
    /// `timeout` bounds each request end to end.
    pub fn new(service: &'static str, base_url: &str, service_token: &str, timeout: Duration) -> Result<Self, PortError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| PortError::Transport { service, message: e.to_string() })?;
        Ok(Self { service, http, base: base_url.trim_end_matches('/').to_owned(), token: service_token.to_owned() })
    }

    async fn send<B: Serialize>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<Option<reqwest::Response>, PortError> {
        let mut req = self.http.request(method, format!("{}{path}", self.base)).bearer_auth(&self.token);
        if let Some(body) = body {
            let bytes = serde_json::to_vec(body).map_err(|e| PortError::Decode { service: self.service, message: e.to_string() })?;
            req = req.header(reqwest::header::CONTENT_TYPE, "application/json").body(bytes);
        }
        let resp = req.send().await.map_err(|e| PortError::Transport { service: self.service, message: e.to_string() })?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let message = serde_json::from_str::<ApiError>(&text).map(|e| e.to_string()).unwrap_or(text);
            return Err(PortError::Status { service: self.service, status: status.as_u16(), message });
        }
        Ok(Some(resp))
    }

    async fn json<T: DeserializeOwned>(&self, resp: reqwest::Response) -> Result<T, PortError> {
        let bytes = resp.bytes().await.map_err(|e| PortError::Transport { service: self.service, message: e.to_string() })?;
        serde_json::from_slice(&bytes).map_err(|e| PortError::Decode { service: self.service, message: e.to_string() })
    }

    fn not_found(&self, path: &str) -> PortError {
        PortError::Status { service: self.service, status: 404, message: format!("{path} not found") }
    }

    /// Request with a JSON body whose 2xx answer is JSON.
    pub async fn call<B: Serialize, T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<T, PortError> {
        match self.send(method, path, body).await? {
            Some(resp) => self.json(resp).await,
            None => Err(self.not_found(path)),
        }
    }

    /// Like [`ServiceClient::call`], with 404 as `None`.
    pub async fn call_optional<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>, PortError> {
        match self.send::<()>(reqwest::Method::GET, path, None).await? {
            Some(resp) => self.json(resp).await.map(Some),
            None => Ok(None),
        }
    }

    /// Request whose 2xx body is ignored.
    pub async fn call_empty<B: Serialize>(&self, path: &str, body: Option<&B>) -> Result<(), PortError> {
        match self.send(reqwest::Method::POST, path, body).await? {
            Some(_) => Ok(()),
            None => Err(self.not_found(path)),
        }
    }
}

fn deployment_path(template: &str, project: &str, stage: Stage) -> String {
    cloud_api::routes::fill(template, &[("p", project), ("stage", stage.as_str())])
}

/// The builder over HTTP.
pub struct HttpBuilder(pub ServiceClient);

#[async_trait]
impl BuildService for HttpBuilder {
    async fn build(&self, job: BuildJob) -> Result<BuildResult, PortError> {
        self.0.call(reqwest::Method::POST, routes::BUILD, Some(&job)).await
    }
}

/// A runtime agent over HTTP.
pub struct HttpRuntime(pub ServiceClient);

#[async_trait]
impl RuntimeService for HttpRuntime {
    async fn deploy(&self, deployment: Deployment) -> Result<DeploymentStatus, PortError> {
        let path = deployment_path(routes::DEPLOYMENT, &deployment.project, deployment.stage);
        self.0.call(reqwest::Method::PUT, &path, Some(&deployment)).await
    }

    async fn status(&self, project: &str, stage: Stage) -> Result<Option<DeploymentStatus>, PortError> {
        self.0.call_optional(&deployment_path(routes::DEPLOYMENT, project, stage)).await
    }

    async fn restart(&self, project: &str, stage: Stage) -> Result<(), PortError> {
        self.0.call_empty::<()>(&deployment_path(routes::DEPLOYMENT_RESTART, project, stage), None).await
    }

    async fn scale(&self, project: &str, stage: Stage, scale: Scale) -> Result<(), PortError> {
        self.0.call_empty(&deployment_path(routes::DEPLOYMENT_SCALE, project, stage), Some(&scale)).await
    }

    async fn maintenance(&self, project: &str, stage: Stage, maintenance: Maintenance) -> Result<(), PortError> {
        self.0.call_empty(&deployment_path(routes::DEPLOYMENT_MAINTENANCE, project, stage), Some(&maintenance)).await
    }

    async fn sleep(&self, project: &str, stage: Stage) -> Result<(), PortError> {
        self.0.call_empty::<()>(&deployment_path(routes::DEPLOYMENT_SLEEP, project, stage), None).await
    }
}

/// The session host over HTTP.
pub struct HttpSessionHost(pub ServiceClient);

impl HttpSessionHost {
    /// `POST` whose 4xx answers are [`PortError::classify`]d.
    async fn workspace_call<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Result<T, PortError> {
        self.0.call(reqwest::Method::POST, path, Some(body)).await.map_err(PortError::classify)
    }
}

#[async_trait]
impl SessionHostService for HttpSessionHost {
    async fn start(&self, request: StartWorkspace) -> Result<WorkspaceStarted, PortError> {
        self.0.call(reqwest::Method::POST, routes::WORKSPACES, Some(&request)).await
    }

    async fn package(&self, workspace: WorkspaceRef) -> Result<WorkspacePackage, PortError> {
        self.workspace_call(routes::WORKSPACE_PACKAGE, &workspace).await
    }

    async fn files(&self, workspace: WorkspaceRef) -> Result<FileTree, PortError> {
        self.workspace_call(routes::WORKSPACE_FILES, &workspace).await
    }

    async fn file(&self, file: WorkspaceFileRef) -> Result<FileContent, PortError> {
        self.workspace_call(routes::WORKSPACE_FILE, &file).await
    }
}
