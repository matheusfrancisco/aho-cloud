//! `edge.toml`: which routes the edge gates and how. The first matching route wins; unmatched
//! paths take `default`.
//!
//! ```toml
//! default = "edge"
//!
//! [[route]]
//! path = "/webhooks/stripe"
//! mode = "public"
//! verify = { kind = "stripe", secret = "STRIPE_WEBHOOK_SECRET" }
//!
//! [[route]]
//! path = "/api/**"
//! mode = "bearer"
//! ```

use globset::{GlobBuilder, GlobMatcher};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::manifest::{Auth, Manifest};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct EdgeConfig {
    #[serde(default = "default_mode")]
    pub default: Mode,
    #[serde(default, rename = "route")]
    pub routes: Vec<Route>,
}

impl Default for EdgeConfig {
    fn default() -> Self {
        Self { default: Mode::Edge, routes: Vec::new() }
    }
}

fn default_mode() -> Mode {
    Mode::Edge
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// Glob over the request path: `*` stays within a segment, `**` crosses segments.
    pub path: String,
    pub mode: Mode,
    /// Only with `mode = "public"`: the edge checks the request signature before forwarding.
    #[serde(default)]
    pub verify: Option<Verifier>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Login required; the app gets `X-Principal-JWT`.
    Edge,
    /// Principal attached when a session exists; nothing is gated.
    Optional,
    /// `Authorization: Bearer <project service token>`, checked at the edge.
    Bearer,
    /// Anyone; with `verify`, only correctly signed requests.
    Public,
    /// Forwarded untouched; the app is on its own. Principal headers are still stripped.
    Passthrough,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Verifier {
    /// `Stripe-Signature: t=…,v1=…`, HMAC-SHA256 over `t.body`.
    Stripe { secret: String },
    /// `X-Hub-Signature-256: sha256=…`, HMAC-SHA256 over the body.
    Github { secret: String },
    /// `header` carries hex HMAC-SHA256 over the body.
    Hmac { header: String, secret: String },
}

impl Verifier {
    pub fn secret(&self) -> &str {
        match self {
            Verifier::Stripe { secret } | Verifier::Github { secret } | Verifier::Hmac { secret, .. } => secret,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EdgeConfigError {
    #[error("edge.toml: {0}")]
    Parse(String),
    #[error("route {index} (`{path}`): bad glob: {reason}")]
    Glob { index: usize, path: String, reason: String },
    #[error("route {index} (`{path}`): `verify` is only valid with mode = \"public\"")]
    VerifyWithoutPublic { index: usize, path: String },
    #[error("route {index} (`{path}`): route paths start with `/`")]
    Relative { index: usize, path: String },
    #[error("route {index} (`{path}`): verifier secret `{secret}` is not declared in the manifest")]
    UnknownSecret { index: usize, path: String, secret: String },
    #[error("mode `{mode:?}` needs an edge login, but manifest auth is `{auth}`")]
    ModeNeedsAuth { mode: Mode, auth: &'static str },
}

impl EdgeConfig {
    pub fn parse(text: &str) -> Result<Self, EdgeConfigError> {
        toml::from_str(text).map_err(|e| EdgeConfigError::Parse(e.message().to_owned()))
    }

    /// Compiles the globs; errors name the route index and path.
    pub fn compile(&self) -> Result<Routes, EdgeConfigError> {
        let mut compiled = Vec::with_capacity(self.routes.len());
        for (index, r) in self.routes.iter().enumerate() {
            if !r.path.starts_with('/') {
                return Err(EdgeConfigError::Relative { index, path: r.path.clone() });
            }
            if r.verify.is_some() && r.mode != Mode::Public {
                return Err(EdgeConfigError::VerifyWithoutPublic { index, path: r.path.clone() });
            }
            let glob = GlobBuilder::new(&r.path).literal_separator(true).build().map_err(|e| EdgeConfigError::Glob {
                index,
                path: r.path.clone(),
                reason: e.kind().to_string(),
            })?;
            compiled.push((glob.compile_matcher(), r.clone()));
        }
        Ok(Routes { default: self.default, compiled })
    }

    /// Cross-checks against the manifest this edge.toml ships with.
    pub fn check_against(&self, manifest: &Manifest) -> Result<(), Vec<EdgeConfigError>> {
        let mut errs = Vec::new();
        let auth = match manifest.auth {
            Auth::None => Some("none"),
            Auth::AppOwned => Some("app_owned"),
            _ => None,
        };
        if let Some(auth) = auth {
            let modes = std::iter::once(self.default).chain(self.routes.iter().map(|r| r.mode));
            if let Some(mode) = modes.into_iter().find(|m| matches!(m, Mode::Edge | Mode::Optional)) {
                errs.push(EdgeConfigError::ModeNeedsAuth { mode, auth });
            }
        }
        for (index, r) in self.routes.iter().enumerate() {
            if let Some(v) = &r.verify
                && manifest.secret(v.secret()).is_none()
            {
                errs.push(EdgeConfigError::UnknownSecret { index, path: r.path.clone(), secret: v.secret().to_owned() });
            }
        }
        if errs.is_empty() { Ok(()) } else { Err(errs) }
    }

    /// The edge.toml implied by a manifest when the project ships none.
    pub fn implied_by(manifest: &Manifest) -> Self {
        let default = match manifest.auth {
            Auth::None | Auth::AppOwned => Mode::Passthrough,
            _ => Mode::Edge,
        };
        Self { default, routes: Vec::new() }
    }
}

/// Compiled routes; cheap to query per request.
#[derive(Clone, Debug)]
pub struct Routes {
    default: Mode,
    compiled: Vec<(GlobMatcher, Route)>,
}

/// The decision for one path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Matched<'a> {
    pub mode: Mode,
    pub verify: Option<&'a Verifier>,
    /// Index into `EdgeConfig::routes`; `None` for the default.
    pub route: Option<usize>,
}

impl Routes {
    pub fn resolve(&self, path: &str) -> Matched<'_> {
        let path = path.split(['?', '#']).next().unwrap_or(path);
        for (i, (m, r)) in self.compiled.iter().enumerate() {
            if m.is_match(path) {
                return Matched { mode: r.mode, verify: r.verify.as_ref(), route: Some(i) };
            }
        }
        Matched { mode: self.default, verify: None, route: None }
    }
}
