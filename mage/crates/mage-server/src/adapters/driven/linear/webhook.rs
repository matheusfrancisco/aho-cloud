//! [`LinearDecoder`]: verified `/hooks/linear` bodies → [`HookEvent`] (intake.md
//! "`WebhookDecoder`"). Pure and synchronous; signature and `webhookTimestamp` freshness are checked
//! earlier by `driving::http::hooks`.

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::ports::hooks::{DecodeError, HookEvent, LinearAction, WebhookDecoder, WebhookSource};

/// Linear webhook envelope (`action`, `type`, `data`, `organizationId`, `actor`, …).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    #[serde(default)]
    action: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    data: Option<Value>,
    #[serde(default)]
    organization_id: Option<String>,
    #[serde(default)]
    actor: Option<Actor>,
}

#[derive(Deserialize)]
struct Actor {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LinearDecoder;

impl LinearDecoder {
    pub fn new() -> Self {
        LinearDecoder
    }
}

fn malformed(what: impl Into<String>) -> DecodeError {
    DecodeError(format!("linear webhook: {}", what.into()))
}

fn string_field(data: &Map<String, Value>, field: &str) -> Result<String, DecodeError> {
    match data.get(field) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        _ => Err(malformed(format!("data.{field} missing or not a string"))),
    }
}

impl WebhookDecoder for LinearDecoder {
    fn source(&self) -> WebhookSource {
        WebhookSource::Linear
    }

    /// `event` is the `Linear-Event` header; the signed body `type` wins when both are present.
    fn decode(&self, event: &str, body: &[u8]) -> Result<HookEvent, DecodeError> {
        let envelope: Envelope = serde_json::from_slice(body).map_err(|e| malformed(e.to_string()))?;
        let kind = envelope.kind.filter(|k| !k.is_empty()).or_else(|| (!event.is_empty()).then(|| event.to_owned())).ok_or_else(|| malformed("missing type"))?;
        let action = envelope.action.filter(|a| !a.is_empty()).ok_or_else(|| malformed("missing action"))?;
        let workspace_id = || envelope.organization_id.clone().filter(|o| !o.is_empty()).ok_or_else(|| malformed("missing organizationId"));

        if kind == "OAuthApp" {
            return Ok(match action.as_str() {
                "revoked" => HookEvent::LinearAppRevoked { workspace_id: workspace_id()? },
                other => HookEvent::Ignored { reason: format!("linear OAuthApp action {other}") },
            });
        }
        if !matches!(kind.as_str(), "Issue" | "Comment" | "Attachment" | "IssueAttachment") {
            return Ok(HookEvent::Ignored { reason: format!("linear {kind} events are not handled") });
        }
        let linear_action = match action.as_str() {
            "create" => LinearAction::Create,
            "update" => LinearAction::Update,
            "remove" => LinearAction::Remove,
            other => return Ok(HookEvent::Ignored { reason: format!("linear {kind} action {other}") }),
        };
        let workspace_id = workspace_id()?;
        let Some(Value::Object(data)) = &envelope.data else {
            return Err(malformed("data missing or not an object"));
        };
        let actor_id = envelope.actor.and_then(|a| a.id).filter(|id| !id.is_empty());
        Ok(match kind.as_str() {
            "Issue" => HookEvent::LinearIssue { workspace_id, issue_id: string_field(data, "id")?, action: linear_action, actor_id },
            "Comment" => {
                let comment_id = string_field(data, "id")?;
                // Comments on project updates or documents carry no issue.
                match data.get("issueId") {
                    None | Some(Value::Null) => HookEvent::Ignored { reason: "linear comment not on an issue".into() },
                    Some(_) => HookEvent::LinearComment { workspace_id, comment_id, issue_id: string_field(data, "issueId")?, action: linear_action, actor_id },
                }
            }
            _ => HookEvent::LinearAttachment { workspace_id, issue_id: string_field(data, "issueId")?, action: linear_action, actor_id },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(event: &str, body: &str) -> Result<HookEvent, DecodeError> {
        LinearDecoder::new().decode(event, body.as_bytes())
    }

    const ORG: &str = "8f1c2d3e-0000-4000-8000-000000000001";

    #[test]
    fn decodes_recorded_shapes() {
        let issue = format!(
            r#"{{"action":"update","type":"Issue","createdAt":"2026-10-01T10:00:00.000Z","organizationId":"{ORG}","webhookTimestamp":1790000000000,"webhookId":"w1",
            "url":"https://linear.app/hoop/issue/ENG-1","actor":{{"id":"user-1","type":"user","name":"Ana"}},
            "data":{{"id":"issue-1","identifier":"ENG-1","title":"Crash","teamId":"team-1"}},"updatedFrom":{{"title":"Old"}}}}"#
        );
        assert_eq!(
            decode("Issue", &issue),
            Ok(HookEvent::LinearIssue { workspace_id: ORG.into(), issue_id: "issue-1".into(), action: LinearAction::Update, actor_id: Some("user-1".into()) })
        );

        let comment = format!(r#"{{"action":"create","type":"Comment","organizationId":"{ORG}","webhookTimestamp":1,"data":{{"id":"c-1","body":"hi","issueId":"issue-1","userId":"u"}}}}"#);
        assert_eq!(
            decode("Comment", &comment),
            Ok(HookEvent::LinearComment { workspace_id: ORG.into(), comment_id: "c-1".into(), issue_id: "issue-1".into(), action: LinearAction::Create, actor_id: None })
        );

        let attachment = format!(r#"{{"action":"remove","type":"Attachment","organizationId":"{ORG}","actor":{{"id":"app-1"}},"data":{{"id":"a-1","issueId":"issue-2","url":"https://x"}}}}"#);
        assert_eq!(
            decode("Attachment", &attachment),
            Ok(HookEvent::LinearAttachment { workspace_id: ORG.into(), issue_id: "issue-2".into(), action: LinearAction::Remove, actor_id: Some("app-1".into()) })
        );

        let revoked = format!(r#"{{"action":"revoked","type":"OAuthApp","organizationId":"{ORG}","oauthClientId":"client","webhookTimestamp":1}}"#);
        assert_eq!(decode("OAuthApp", &revoked), Ok(HookEvent::LinearAppRevoked { workspace_id: ORG.into() }));
    }

    #[test]
    fn body_type_wins_and_header_fills_a_missing_type() {
        let body = format!(r#"{{"action":"create","type":"Issue","organizationId":"{ORG}","data":{{"id":"i"}}}}"#);
        assert!(matches!(decode("Comment", &body), Ok(HookEvent::LinearIssue { .. })));
        let untyped = format!(r#"{{"action":"create","organizationId":"{ORG}","data":{{"id":"i"}}}}"#);
        assert!(matches!(decode("Issue", &untyped), Ok(HookEvent::LinearIssue { .. })));
    }

    #[test]
    fn unhandled_events_are_ignored() {
        let cases = [
            format!(r#"{{"action":"create","type":"Project","organizationId":"{ORG}","data":{{"id":"p"}}}}"#),
            format!(r#"{{"action":"update","type":"Reaction","organizationId":"{ORG}"}}"#),
            format!(r#"{{"action":"restore","type":"Issue","organizationId":"{ORG}","data":{{"id":"i"}}}}"#),
            format!(r#"{{"action":"created","type":"OAuthApp","organizationId":"{ORG}"}}"#),
            format!(r#"{{"action":"create","type":"Comment","organizationId":"{ORG}","data":{{"id":"c","projectUpdateId":"pu"}}}}"#),
        ];
        for body in cases {
            assert!(matches!(decode("", &body), Ok(HookEvent::Ignored { .. })), "{body}");
        }
    }

    #[test]
    fn malformed_bodies_are_decode_errors() {
        let cases = [
            "not json".to_owned(),
            "[1,2]".to_owned(),
            r#"{"type":"Issue","organizationId":"o","data":{"id":"i"}}"#.to_owned(),
            r#"{"action":"create","organizationId":"o","data":{"id":"i"}}"#.to_owned(),
            r#"{"action":"create","type":"Issue","data":{"id":"i"}}"#.to_owned(),
            r#"{"action":"create","type":"Issue","organizationId":"o"}"#.to_owned(),
            r#"{"action":"create","type":"Issue","organizationId":"o","data":{"id":7}}"#.to_owned(),
            r#"{"action":"create","type":"Comment","organizationId":"o","data":{"issueId":"i"}}"#.to_owned(),
            r#"{"action":"create","type":"Comment","organizationId":"o","data":{"id":"c","issueId":42}}"#.to_owned(),
            r#"{"action":"create","type":"Attachment","organizationId":"o","data":{"id":"a"}}"#.to_owned(),
            r#"{"action":"revoked","type":"OAuthApp"}"#.to_owned(),
        ];
        for body in cases {
            assert!(decode("", &body).is_err(), "{body}");
        }
    }
}
