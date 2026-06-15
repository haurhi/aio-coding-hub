//! Request body sanitization before sending upstream.
//!
//! This module owns request-body cleaning logic that is independent of
//! authentication concerns.  For example, Claude OAuth upstreams reject
//! messages that contain empty text blocks, so this module strips them
//! before the request is forwarded.

use super::provider_iterator::PreparedProvider;
use crate::gateway::proxy::request_context::RequestContext;
use axum::body::Bytes;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct CleanBodyOutcome {
    pub(super) body: Bytes,
    pub(super) removed_empty_text_blocks: usize,
}

impl CleanBodyOutcome {
    pub(super) fn changed(&self) -> bool {
        self.removed_empty_text_blocks > 0
    }
}

/// Clean request body (e.g. remove empty text blocks for Claude OAuth).
pub(super) fn clean_body<R: tauri::Runtime>(
    input: &RequestContext<R>,
    prepared: &PreparedProvider,
) -> CleanBodyOutcome {
    if input.cli_key == "claude" && prepared.oauth_adapter.is_some() {
        return clean_claude_oauth_body(&prepared.upstream_body_bytes);
    }
    CleanBodyOutcome {
        body: prepared.upstream_body_bytes.clone(),
        removed_empty_text_blocks: 0,
    }
}

fn clean_claude_oauth_body(upstream_body_bytes: &Bytes) -> CleanBodyOutcome {
    let Ok(mut json) = serde_json::from_slice::<serde_json::Value>(upstream_body_bytes) else {
        return CleanBodyOutcome {
            body: upstream_body_bytes.clone(),
            removed_empty_text_blocks: 0,
        };
    };

    let mut removed_empty_text_blocks = 0usize;
    if let Some(messages) = json.get_mut("messages").and_then(|v| v.as_array_mut()) {
        for msg in messages {
            if let Some(content) = msg.get_mut("content").and_then(|v| v.as_array_mut()) {
                let before_len = content.len();
                content.retain(|block| {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                        !text.trim().is_empty()
                    } else {
                        true
                    }
                });
                removed_empty_text_blocks =
                    removed_empty_text_blocks.saturating_add(before_len - content.len());
            }
        }
    }

    if removed_empty_text_blocks == 0 {
        return CleanBodyOutcome {
            body: upstream_body_bytes.clone(),
            removed_empty_text_blocks: 0,
        };
    }

    CleanBodyOutcome {
        body: serde_json::to_vec(&json)
            .unwrap_or_else(|_| upstream_body_bytes.to_vec())
            .into(),
        removed_empty_text_blocks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;

    #[test]
    fn clean_claude_oauth_body_removes_empty_text_blocks() {
        let body = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "text", "text": "   "},
                    {"type": "tool_result", "content": "ok"}
                ]
            }]
        });
        let encoded = Bytes::from(serde_json::to_vec(&body).unwrap());

        let outcome = clean_claude_oauth_body(&encoded);

        assert_eq!(outcome.removed_empty_text_blocks, 1);
        assert!(outcome.changed());
        let cleaned: serde_json::Value = serde_json::from_slice(&outcome.body).unwrap();
        let content = cleaned["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
    }

    #[test]
    fn clean_claude_oauth_body_is_noop_without_empty_text_blocks() {
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}]
        });
        let encoded = Bytes::from(serde_json::to_vec(&body).unwrap());

        let outcome = clean_claude_oauth_body(&encoded);

        assert_eq!(outcome.removed_empty_text_blocks, 0);
        assert_eq!(outcome.body, encoded);
    }
}
