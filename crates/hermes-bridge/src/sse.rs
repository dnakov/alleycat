//! SSE parsing helpers for the Hermes gateway.
//!
//! Stock Hermes emits standard Server-Sent Events frames with an `event:`
//! discriminator and JSON `data:` payloads, for example:
//!
//! ```text
//! event: message.delta
//! data: {"delta":"hello"}
//!
//! event: run.completed
//! data: {"status":"completed"}
//! ```

use crate::api_client::HermesEvent;
use serde_json::Value;
use tracing::debug;

/// Parse buffered SSE text into Hermes events.
pub fn parse_sse_frames(body: &str) -> Vec<HermesEvent> {
    let mut events = Vec::new();
    for frame in body.split("\n\n") {
        let mut event_name: Option<String> = None;
        let mut data_lines = Vec::new();
        for raw in frame.lines() {
            let line = raw.trim_end_matches('\r');
            if let Some(rest) = line.strip_prefix("event:") {
                event_name = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(rest.trim_start());
            }
        }
        let data_text = data_lines.join("\n");
        let data = if data_text.is_empty() {
            Value::Null
        } else {
            match serde_json::from_str::<Value>(&data_text) {
                Ok(value) => value,
                Err(error) => {
                    debug!(%error, "skipping unparseable Hermes SSE frame");
                    continue;
                }
            }
        };
        let Some(event) = event_name.or_else(|| {
            data.get("event")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        }) else {
            continue;
        };
        events.push(HermesEvent { event, data });
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stock_hermes_event_discriminator() {
        let body = "event: message.delta\ndata: {\"delta\":\"hel\"}\n\nevent: message.delta\ndata: {\"delta\":\"lo\"}\n\nevent: run.completed\ndata: {\"status\":\"completed\"}\n\n";
        let events = parse_sse_frames(body);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event, "message.delta");
        assert_eq!(events[0].message_delta().as_deref(), Some("hel"));
        assert!(events[2].is_terminal_success());
    }

    #[test]
    fn parses_hermes_data_only_events() {
        let body = "data: {\"event\":\"message.delta\",\"delta\":\"hel\"}\n\ndata: {\"event\":\"run.completed\",\"output\":\"hello\"}\n\n";
        let events = parse_sse_frames(body);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "message.delta");
        assert_eq!(events[0].message_delta().as_deref(), Some("hel"));
        assert!(events[1].is_terminal_success());
    }
}
