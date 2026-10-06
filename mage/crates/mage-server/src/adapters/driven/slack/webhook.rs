//! [`SlackDecoder`]: Events API deliveries → [`HookEvent`] (intake.md "`WebhookDecoder`",
//! "Slack events decision"). The stored body is decoded; the `event` name from the inbox row
//! (`event.type`) is informational only.

use serde::Deserialize;

use crate::ports::hooks::{DecodeError, HookEvent, WebhookDecoder, WebhookSource};

/// Decodes `/hooks/slack` deliveries. Thread `message` events are an optional extension that is
/// off in v1: they decode to `Ignored` unless the decoder is built with `thread_messages = true`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SlackDecoder {
    thread_messages: bool,
}

impl SlackDecoder {
    pub fn new(thread_messages: bool) -> SlackDecoder {
        SlackDecoder { thread_messages }
    }
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    team_id: Option<String>,
    #[serde(default)]
    event: Option<Event>,
}

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    thread_ts: Option<String>,
    #[serde(default)]
    bot_id: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
    /// `message_changed` carries the edited message here.
    #[serde(default)]
    message: Option<Box<Event>>,
    /// `tokens_revoked`.
    #[serde(default)]
    tokens: Option<RevokedTokens>,
}

#[derive(Deserialize)]
struct RevokedTokens {
    #[serde(default)]
    bot: Vec<String>,
}

fn ignored(reason: impl Into<String>) -> HookEvent {
    HookEvent::Ignored { reason: reason.into() }
}

impl WebhookDecoder for SlackDecoder {
    fn source(&self) -> WebhookSource {
        WebhookSource::Slack
    }

    fn decode(&self, _event: &str, body: &[u8]) -> Result<HookEvent, DecodeError> {
        let envelope: Envelope = serde_json::from_slice(body).map_err(|e| DecodeError(format!("slack envelope: {e}")))?;
        match envelope.kind.as_str() {
            "event_callback" => {}
            "url_verification" => return Ok(ignored("url_verification")),
            other => return Ok(ignored(format!("slack envelope type {other}"))),
        }
        let team_id = envelope.team_id.filter(|t| !t.is_empty()).ok_or_else(|| DecodeError("slack event_callback without team_id".into()))?;
        let event = envelope.event.ok_or_else(|| DecodeError("slack event_callback without event".into()))?;
        match event.kind.as_str() {
            "app_uninstalled" => Ok(HookEvent::SlackAppUninstalled { team_id }),
            // Mage holds only the bot token; revoked user tokens leave the integration intact.
            "tokens_revoked" => match event.tokens {
                Some(tokens) if !tokens.bot.is_empty() => Ok(HookEvent::SlackTokensRevoked { team_id }),
                Some(_) => Ok(ignored("slack tokens_revoked without bot tokens")),
                None => Err(DecodeError("slack tokens_revoked without tokens".into())),
            },
            "message" if !self.thread_messages => Ok(ignored("slack thread messages are off")),
            "message" => Ok(thread_message(team_id, event)),
            other => Ok(ignored(format!("slack event {other}"))),
        }
    }
}

fn thread_message(team_id: String, event: Event) -> HookEvent {
    let bot = event.bot_id.is_some() || event.subtype.as_deref() == Some("bot_message");
    let (thread_ts, inner_bot) = match (&event.thread_ts, &event.message) {
        (Some(ts), _) => (Some(ts.clone()), false),
        (None, Some(inner)) => (inner.thread_ts.clone(), inner.bot_id.is_some() || inner.subtype.as_deref() == Some("bot_message")),
        (None, None) => (None, false),
    };
    match (event.channel, thread_ts) {
        (Some(channel), Some(thread_ts)) => HookEvent::SlackThreadMessage { team_id, channel, thread_ts, bot: bot || inner_bot },
        _ => ignored("slack message outside a thread"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn decode(decoder: SlackDecoder, body: &str) -> Result<HookEvent, DecodeError> {
        decoder.decode("ignored-by-decoder", body.as_bytes())
    }

    fn callback(event: &str) -> String {
        format!(r#"{{"token":"x","team_id":"T024BE7LD","api_app_id":"A1","type":"event_callback","event_id":"Ev1","event_time":1,"event":{event}}}"#)
    }

    #[test]
    fn source_is_slack() {
        assert_eq!(SlackDecoder::default().source(), WebhookSource::Slack);
    }

    #[test]
    fn lifecycle_events() {
        let d = SlackDecoder::default();
        assert_eq!(decode(d, &callback(r#"{"type":"app_uninstalled"}"#)).unwrap(), HookEvent::SlackAppUninstalled { team_id: "T024BE7LD".into() });
        assert_eq!(
            decode(d, &callback(r#"{"type":"tokens_revoked","tokens":{"oauth":["U1"],"bot":["U2"]}}"#)).unwrap(),
            HookEvent::SlackTokensRevoked { team_id: "T024BE7LD".into() }
        );
        assert!(matches!(decode(d, &callback(r#"{"type":"tokens_revoked","tokens":{"oauth":["U1"]}}"#)).unwrap(), HookEvent::Ignored { .. }));
    }

    #[test]
    fn thread_messages_are_ignored_by_default() {
        let body = callback(r#"{"type":"message","channel":"C1","user":"U1","text":"hi","ts":"2.0","thread_ts":"1.0"}"#);
        assert!(matches!(decode(SlackDecoder::default(), &body).unwrap(), HookEvent::Ignored { .. }));
        assert_eq!(
            decode(SlackDecoder::new(true), &body).unwrap(),
            HookEvent::SlackThreadMessage { team_id: "T024BE7LD".into(), channel: "C1".into(), thread_ts: "1.0".into(), bot: false }
        );
    }

    #[test]
    fn thread_message_mapping_when_enabled() {
        let d = SlackDecoder::new(true);
        let bot = callback(r#"{"type":"message","subtype":"bot_message","bot_id":"B1","channel":"C1","ts":"2.0","thread_ts":"1.0"}"#);
        assert!(matches!(decode(d, &bot).unwrap(), HookEvent::SlackThreadMessage { bot: true, .. }));
        let edited = callback(r#"{"type":"message","subtype":"message_changed","channel":"C1","message":{"type":"message","ts":"2.0","thread_ts":"1.0"}}"#);
        assert!(matches!(decode(d, &edited).unwrap(), HookEvent::SlackThreadMessage { ref thread_ts, .. } if thread_ts == "1.0"));
        let top_level = callback(r#"{"type":"message","channel":"C1","user":"U1","ts":"2.0"}"#);
        assert!(matches!(decode(d, &top_level).unwrap(), HookEvent::Ignored { .. }));
    }

    #[test]
    fn other_envelopes_and_events_are_ignored() {
        let d = SlackDecoder::default();
        assert!(matches!(decode(d, r#"{"token":"x","challenge":"c","type":"url_verification"}"#).unwrap(), HookEvent::Ignored { .. }));
        assert!(matches!(decode(d, r#"{"type":"app_rate_limited","team_id":"T1","minute_rate_limited":1}"#).unwrap(), HookEvent::Ignored { .. }));
        assert!(matches!(decode(d, &callback(r#"{"type":"channel_rename"}"#)).unwrap(), HookEvent::Ignored { .. }));
    }

    #[test]
    fn malformed_bodies_are_decode_errors() {
        let d = SlackDecoder::default();
        assert!(decode(d, "not json").is_err());
        assert!(decode(d, r#"{"team_id":"T1"}"#).is_err());
        assert!(decode(d, r#"{"type":"event_callback","event":{"type":"app_uninstalled"}}"#).is_err());
        assert!(decode(d, r#"{"type":"event_callback","team_id":"T1"}"#).is_err());
        assert!(decode(d, &callback(r#"{"type":"tokens_revoked"}"#)).is_err());
    }
}
