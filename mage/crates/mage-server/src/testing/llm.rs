//! Scripted `ModelUpstream` (server.md "Testing": "scripted upstream (recorded SSE fixtures)").
//! Answers queued responses in order and records every request it was sent.

use std::collections::VecDeque;

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;

use mage_core::wire::api::ModelRouteKind;

use crate::ports::llm::{
    ModelRoute, ModelUpstream, UpstreamBody, UpstreamError, UpstreamHead, UpstreamOp, UpstreamRequest, UpstreamResponse,
};

/// One Messages turn as the Anthropic API streams it: usage in `message_start`, a ping, one text
/// block, cumulative usage repeated in `message_delta`, `message_stop`.
pub const SSE_TEXT_TURN: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":120,\"cache_creation_input_tokens\":30,\"cache_read_input_tokens\":2000,\"output_tokens\":1}}}\n\n",
    "event: ping\n",
    "data: {\"type\": \"ping\"}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":120,\"cache_creation_input_tokens\":30,\"cache_read_input_tokens\":2000,\"output_tokens\":57}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// A non-streaming Messages answer.
pub const MESSAGE_JSON: &str = r#"{"id":"msg_02","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":4}}"#;

#[derive(Debug, Clone)]
pub enum ScriptedBody {
    Full(Bytes),
    /// Delivered as `UpstreamBody::Stream` when the request streams and the status is 2xx.
    Chunks(Vec<Bytes>),
}

#[derive(Debug, Clone)]
pub enum Scripted {
    Respond { status: u16, headers: Vec<(String, String)>, body: ScriptedBody },
    Fail(UpstreamError),
}

impl Scripted {
    pub fn json(status: u16, body: &str) -> Scripted {
        Scripted::Respond {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: ScriptedBody::Full(Bytes::copy_from_slice(body.as_bytes())),
        }
    }

    /// 200 `text/event-stream` with the given chunks.
    pub fn sse<'a>(chunks: impl IntoIterator<Item = &'a str>) -> Scripted {
        Scripted::Respond {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: ScriptedBody::Chunks(chunks.into_iter().map(|c| Bytes::copy_from_slice(c.as_bytes())).collect()),
        }
    }
}

/// What the gateway sent, with the credential reduced to what a test may compare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentRequest {
    pub op: UpstreamOp,
    pub route: ModelRouteKind,
    /// `AnthropicApi` key bytes; `None` on Vertex.
    pub api_key: Option<Vec<u8>>,
    /// Vertex model id; `None` on the Anthropic route.
    pub vertex_model: Option<String>,
    pub stream: bool,
    pub anthropic_version: Option<String>,
    pub anthropic_beta: Vec<String>,
    pub body: Bytes,
}

pub struct ScriptedUpstream {
    script: Mutex<VecDeque<Scripted>>,
    sent: Mutex<Vec<SentRequest>>,
    vertex: bool,
}

impl ScriptedUpstream {
    /// Anthropic route only (Messages and CountTokens).
    pub fn new() -> ScriptedUpstream {
        ScriptedUpstream { script: Mutex::new(VecDeque::new()), sent: Mutex::new(Vec::new()), vertex: false }
    }

    /// Also serves the Vertex route (Messages only, like `VertexUpstream`).
    pub fn with_vertex() -> ScriptedUpstream {
        ScriptedUpstream { vertex: true, ..ScriptedUpstream::new() }
    }

    pub fn push(&self, response: Scripted) {
        self.script.lock().push_back(response);
    }

    pub fn sent(&self) -> Vec<SentRequest> {
        self.sent.lock().clone()
    }
}

impl Default for ScriptedUpstream {
    fn default() -> Self {
        ScriptedUpstream::new()
    }
}

#[async_trait]
impl ModelUpstream for ScriptedUpstream {
    async fn send(&self, req: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let (api_key, vertex_model) = match &req.route {
            ModelRoute::AnthropicApi { api_key } => (Some(api_key.expose().to_vec()), None),
            ModelRoute::Vertex { model, .. } => (None, Some(model.clone())),
        };
        self.sent.lock().push(SentRequest {
            op: req.op,
            route: req.route.kind(),
            api_key,
            vertex_model,
            stream: req.stream,
            anthropic_version: req.anthropic_version.clone(),
            anthropic_beta: req.anthropic_beta.clone(),
            body: req.body.clone(),
        });
        let next = self.script.lock().pop_front().ok_or_else(|| UpstreamError::Unavailable("no scripted response left".into()))?;
        let (status, headers, body) = match next {
            Scripted::Fail(e) => return Err(e),
            Scripted::Respond { status, headers, body } => (status, headers, body),
        };
        let head = UpstreamHead { status, headers };
        let body = match body {
            ScriptedBody::Chunks(chunks) if req.stream && head.is_success() => {
                UpstreamBody::Stream(Box::pin(futures::stream::iter(chunks.into_iter().map(Ok))))
            }
            ScriptedBody::Chunks(chunks) => UpstreamBody::Full(chunks.concat().into()),
            ScriptedBody::Full(bytes) => UpstreamBody::Full(bytes),
        };
        Ok(UpstreamResponse { head, body })
    }

    fn supports(&self, route: &ModelRoute, op: UpstreamOp) -> bool {
        match route {
            ModelRoute::AnthropicApi { .. } => true,
            ModelRoute::Vertex { .. } => self.vertex && op == UpstreamOp::Messages,
        }
    }
}
