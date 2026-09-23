//! The real HTTP adapters against in-process services on 127.0.0.1:0.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code: a failed unwrap is a failed test")]

use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use cloud_api::ApiError;
use cloud_api::internal::{
    BuildJob, BuildResult, Deployment, DeploymentStatus, Maintenance, RunState, Scale, StartWorkspace, WorkspaceStarted, routes,
};
use cloud_contract::ledger::Stage;
use cloud_contract::manifest::{Build, Source};
use cloud_control_plane::ports::*;

const TOKEN: &str = "svc-http-token-0123456789abcdef012345";

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<String>>>);

fn authorized(h: &HeaderMap) -> Result<(), (StatusCode, Json<ApiError>)> {
    if h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {TOKEN}")) {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, Json(ApiError::new("unauthorized", "service token required"))))
    }
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn status(project: &str, stage: Stage, release: &str) -> DeploymentStatus {
    DeploymentStatus { project: project.into(), stage, release: release.into(), state: RunState::Running, processes: Vec::new() }
}

async fn fake_services(seen: Seen) -> String {
    let app = Router::new()
        .route(
            routes::BUILD,
            post(|h: HeaderMap, Json(job): Json<BuildJob>| async move {
                authorized(&h)?;
                Ok::<_, (StatusCode, Json<ApiError>)>(Json(BuildResult::Built {
                    image_digest: format!("sha256:{}", "e".repeat(64)),
                    log: job.release,
                }))
            }),
        )
        .route(
            routes::DEPLOYMENT,
            get(|h: HeaderMap, Path((p, stage)): Path<(String, String)>| async move {
                authorized(&h)?;
                if p == "missing" {
                    return Err((StatusCode::NOT_FOUND, Json(ApiError::new("not_found", "no deployment"))));
                }
                let stage: Stage = serde_json::from_value(serde_json::json!(stage)).unwrap();
                Ok::<_, (StatusCode, Json<ApiError>)>(Json(status(&p, stage, "r-1")))
            })
            .put(|h: HeaderMap, Json(d): Json<Deployment>| async move {
                authorized(&h)?;
                Ok::<_, (StatusCode, Json<ApiError>)>(Json(status(&d.project, d.stage, &d.release)))
            }),
        )
        .route(
            routes::DEPLOYMENT_SCALE,
            post(|State(seen): State<Seen>, h: HeaderMap, Path((p, stage)): Path<(String, String)>, Json(s): Json<Scale>| async move {
                authorized(&h)?;
                seen.0.lock().push(format!("scale {p}/{stage} {} {}", s.process, s.replicas));
                Ok::<_, (StatusCode, Json<ApiError>)>(StatusCode::NO_CONTENT)
            }),
        )
        .route(
            routes::DEPLOYMENT_MAINTENANCE,
            post(|State(seen): State<Seen>, h: HeaderMap, Path((p, _)): Path<(String, String)>, Json(m): Json<Maintenance>| async move {
                authorized(&h)?;
                seen.0.lock().push(format!("maintenance {p} {}", m.on));
                Ok::<_, (StatusCode, Json<ApiError>)>(Json(status(&p, Stage::Live, "r-1")))
            }),
        )
        .route(
            routes::WORKSPACES,
            post(|h: HeaderMap, Json(req): Json<StartWorkspace>| async move {
                authorized(&h)?;
                Ok::<_, (StatusCode, Json<ApiError>)>(Json(WorkspaceStarted {
                    session_id: req.session_id,
                    rpc_url: format!("wss://host/rpc/{}", req.project),
                    token: "t".into(),
                    preview_url: "https://p".into(),
                }))
            }),
        )
        .with_state(seen);
    serve(app).await
}

fn client(base: &str, token: &str) -> ServiceClient {
    ServiceClient::new("test", base, token, Duration::from_secs(5)).unwrap()
}

#[tokio::test]
async fn builder_adapter_posts_jobs_with_the_service_token() {
    let base = fake_services(Seen::default()).await;
    let job = BuildJob {
        project: "shop".into(),
        release: "r-1".into(),
        source: Source::Oci { digest: format!("sha256:{}", "a".repeat(64)) },
        build: Build::Prebuilt,
        archive_url: None,
    };
    let built = HttpBuilder(client(&base, TOKEN)).build(job.clone()).await.unwrap();
    assert_eq!(built, BuildResult::Built { image_digest: format!("sha256:{}", "e".repeat(64)), log: "r-1".into() });
    match HttpBuilder(client(&base, "wrong")).build(job).await {
        Err(PortError::Status { status: 401, message, .. }) => assert!(message.contains("service token required"), "{message}"),
        other => panic!("expected a 401, got {other:?}"),
    }
}

#[tokio::test]
async fn runtime_adapter_deploys_reads_status_and_sends_commands() {
    let seen = Seen::default();
    let base = fake_services(seen.clone()).await;
    let rt = HttpRuntime(client(&base, TOKEN));
    assert_eq!(rt.status("shop", Stage::Staging).await.unwrap().unwrap().stage, Stage::Staging);
    assert!(rt.status("missing", Stage::Live).await.unwrap().is_none(), "404 means nothing runs there");
    rt.scale("shop", Stage::Live, Scale { process: "web".into(), replicas: 3 }).await.unwrap();
    rt.maintenance("shop", Stage::Live, Maintenance { on: true }).await.unwrap();
    assert_eq!(*seen.0.lock(), vec!["scale shop/live web 3".to_owned(), "maintenance shop true".to_owned()]);
    assert!(
        matches!(rt.restart("shop", Stage::Live).await, Err(PortError::Status { status: 404, .. })),
        "a route the service lacks is an error"
    );
    let unreachable = HttpRuntime(client("http://127.0.0.1:9", TOKEN));
    assert!(matches!(unreachable.status("shop", Stage::Live).await, Err(PortError::Transport { .. })));
}

#[tokio::test]
async fn session_host_adapter_starts_workspaces() {
    let base = fake_services(Seen::default()).await;
    let host = HttpSessionHost(client(&base, TOKEN));
    let w = host.start(StartWorkspace { project: "shop".into(), user_id: "u1".into(), session_id: Some("abc".into()) }).await.unwrap();
    assert_eq!((w.session_id.as_deref(), w.rpc_url.as_str()), (Some("abc"), "wss://host/rpc/shop"));
}
