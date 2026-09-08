//! The internal HTTP API end to end: service-token auth, validation, and status transitions.
// Helpers outside `#[test]` fns are test code too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use cloud_api::ApiError;
use cloud_api::internal::{DeploymentStatus, RunState, routes};
use cloud_runtime::egress::EgressPolicies;
use cloud_runtime::net::Allocator;
use cloud_runtime::{Supervisor, SupervisorConfig, api};
use common::{DIGEST_1, TestVmm, deployment};
use reqwest::{Client, Method, StatusCode};
use serde_json::json;

const TOKEN: &str = "svc-token-3f9a";

struct Server {
    base: String,
    http: Client,
    sup: Supervisor<TestVmm>,
}

impl Server {
    async fn start() -> Server {
        let allocator = Allocator::new("10.77.0.0/24".parse().unwrap()).unwrap();
        let cfg = SupervisorConfig { health_timeout: Duration::from_millis(500), startup_grace: Duration::ZERO, ..Default::default() };
        let sup = Supervisor::new(TestVmm::default(), allocator, EgressPolicies::default(), cfg);
        let app = api::router(sup.clone(), TOKEN);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Server { base, http: Client::new(), sup }
    }

    fn url(&self, route: &str, project: &str, stage: &str) -> String {
        format!("{}{}", self.base, cloud_api::routes::fill(route, &[("p", project), ("stage", stage)]))
    }

    async fn call(&self, method: Method, route: &str, stage: &str, body: Option<serde_json::Value>) -> reqwest::Response {
        let mut req = self.http.request(method, self.url(route, "acme-shop", stage)).bearer_auth(TOKEN);
        if let Some(body) = body {
            req = req.json(&body);
        }
        req.send().await.unwrap()
    }

    async fn status(&self, method: Method, route: &str, body: Option<serde_json::Value>) -> DeploymentStatus {
        let res = self.call(method, route, "live", body).await;
        assert_eq!(res.status(), StatusCode::OK);
        res.json().await.unwrap()
    }
}

async fn error(res: reqwest::Response, status: StatusCode) -> ApiError {
    assert_eq!(res.status(), status);
    res.json().await.unwrap()
}

#[tokio::test]
async fn every_route_requires_the_service_token() {
    let s = Server::start().await;
    let body = serde_json::to_value(deployment("r1", DIGEST_1)).unwrap();
    for (method, route) in [
        (Method::PUT, routes::DEPLOYMENT),
        (Method::GET, routes::DEPLOYMENT),
        (Method::DELETE, routes::DEPLOYMENT),
        (Method::POST, routes::DEPLOYMENT_RESTART),
        (Method::POST, routes::DEPLOYMENT_SCALE),
        (Method::POST, routes::DEPLOYMENT_MAINTENANCE),
        (Method::POST, routes::DEPLOYMENT_SLEEP),
    ] {
        let url = s.url(route, "acme-shop", "live");
        let anonymous = s.http.request(method.clone(), &url).json(&body).send().await.unwrap();
        assert_eq!(error(anonymous, StatusCode::UNAUTHORIZED).await.code, "unauthorized", "{method} {route}");
        let wrong = s.http.request(method.clone(), &url).bearer_auth("svc-token-3f9b").json(&body).send().await.unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        let empty = s.http.request(method.clone(), &url).header("authorization", "Bearer ").send().await.unwrap();
        assert_eq!(empty.status(), StatusCode::UNAUTHORIZED);
        let basic = s.http.request(method.clone(), &url).header("authorization", format!("Basic {TOKEN}")).send().await.unwrap();
        assert_eq!(basic.status(), StatusCode::UNAUTHORIZED);
    }
    assert!(s.sup.status("acme-shop", cloud_contract::ledger::Stage::Live).is_err(), "nothing was applied");
}

#[tokio::test]
async fn an_empty_configured_token_admits_nobody() {
    let allocator = Allocator::new("10.77.0.0/24".parse().unwrap()).unwrap();
    let sup = Supervisor::new(TestVmm::default(), allocator, EgressPolicies::default(), SupervisorConfig::default());
    let app = api::router(sup, "");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}{}",
        listener.local_addr().unwrap(),
        cloud_api::routes::fill(routes::DEPLOYMENT, &[("p", "x"), ("stage", "live")])
    );
    tokio::spawn(async move { axum::serve(listener, app).await });
    let res = Client::new().get(&url).header("authorization", "Bearer ").send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn put_validates_the_deployment() {
    let s = Server::start().await;
    let good = serde_json::to_value(deployment("r1", DIGEST_1)).unwrap();

    let res = s.call(Method::PUT, routes::DEPLOYMENT, "staging", Some(good.clone())).await;
    assert_eq!(error(res, StatusCode::BAD_REQUEST).await.code, "invalid", "stage mismatch");
    let res = s.http.put(s.url(routes::DEPLOYMENT, "other", "live")).bearer_auth(TOKEN).json(&good).send().await.unwrap();
    assert_eq!(error(res, StatusCode::BAD_REQUEST).await.code, "invalid", "project mismatch");

    let mut bad_digest = good.clone();
    bad_digest["image_digest"] = json!("latest");
    let res = s.call(Method::PUT, routes::DEPLOYMENT, "live", Some(bad_digest)).await;
    assert_eq!(error(res, StatusCode::BAD_REQUEST).await.code, "invalid");

    let mut refused = good.clone();
    refused["manifest"]["egress"] = json!([]);
    let res = s.call(Method::PUT, routes::DEPLOYMENT, "live", Some(refused)).await;
    let e = error(res, StatusCode::BAD_REQUEST).await;
    assert_eq!(e.code, "refused");
    assert!(!e.refusals.is_empty(), "a secret's reach must be covered by egress");

    let res = s.call(Method::PUT, routes::DEPLOYMENT, "live", Some(json!({"release": "r1"}))).await;
    assert_eq!(error(res, StatusCode::BAD_REQUEST).await.code, "invalid");

    let res = s.call(Method::GET, routes::DEPLOYMENT, "nightly", None).await;
    assert_eq!(error(res, StatusCode::NOT_FOUND).await.code, "not_found");
}

#[tokio::test]
async fn status_moves_through_the_lifecycle() {
    let s = Server::start().await;
    let res = s.call(Method::GET, routes::DEPLOYMENT, "live", None).await;
    assert_eq!(error(res, StatusCode::NOT_FOUND).await.code, "not_found");

    let body = serde_json::to_value(deployment("r1", DIGEST_1)).unwrap();
    let st = s.status(Method::PUT, routes::DEPLOYMENT, Some(body.clone())).await;
    assert_eq!((st.state, st.release.as_str()), (RunState::Starting, "r1"));
    assert_eq!(s.status(Method::GET, routes::DEPLOYMENT, None).await.state, RunState::Starting);

    s.sup.reconcile().await;
    let st = s.status(Method::GET, routes::DEPLOYMENT, None).await;
    assert_eq!(st.state, RunState::Running);
    let web = st.processes.iter().find(|p| p.name == "web").unwrap();
    assert_eq!((web.replicas, web.healthy, web.addresses.len()), (1, 1, 1));

    let on = s.status(Method::POST, routes::DEPLOYMENT_MAINTENANCE, Some(json!({"on": true}))).await;
    assert_eq!(on.state, RunState::Maintenance);
    let off = s.status(Method::POST, routes::DEPLOYMENT_MAINTENANCE, Some(json!({"on": false}))).await;
    assert_eq!(off.state, RunState::Running);

    let scaled = s.status(Method::POST, routes::DEPLOYMENT_SCALE, Some(json!({"process": "web", "replicas": 2}))).await;
    assert_eq!(scaled.state, RunState::Starting);
    s.sup.reconcile().await;
    let st = s.status(Method::GET, routes::DEPLOYMENT, None).await;
    assert_eq!(st.processes.iter().find(|p| p.name == "web").unwrap().replicas, 2);
    let res = s.call(Method::POST, routes::DEPLOYMENT_SCALE, "live", Some(json!({"process": "ghost", "replicas": 1}))).await;
    assert_eq!(error(res, StatusCode::NOT_FOUND).await.code, "not_found");
    let res = s.call(Method::POST, routes::DEPLOYMENT_SCALE, "live", Some(json!({"process": "web", "replicas": 999}))).await;
    assert_eq!(error(res, StatusCode::BAD_REQUEST).await.code, "invalid");

    assert_eq!(s.status(Method::POST, routes::DEPLOYMENT_RESTART, None).await.state, RunState::Starting);
    s.sup.reconcile().await;
    s.sup.reconcile().await;
    assert_eq!(s.status(Method::GET, routes::DEPLOYMENT, None).await.state, RunState::Running);

    assert_eq!(s.status(Method::POST, routes::DEPLOYMENT_SLEEP, None).await.state, RunState::Sleeping);
    s.sup.reconcile().await;
    let st = s.status(Method::GET, routes::DEPLOYMENT, None).await;
    assert_eq!(st.state, RunState::Sleeping);
    assert!(st.processes.iter().all(|p| p.replicas == 0));

    // A new PUT wakes it.
    assert_eq!(s.status(Method::PUT, routes::DEPLOYMENT, Some(body)).await.state, RunState::Starting);

    let res = s.call(Method::DELETE, routes::DEPLOYMENT, "live", None).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let res = s.call(Method::GET, routes::DEPLOYMENT, "live", None).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = s.call(Method::POST, routes::DEPLOYMENT_SLEEP, "live", None).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}
