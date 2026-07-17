//! CX2CC (Claude-to-ChatGPT-Compatible) protocol bridge helpers.
//!
//! - **Model mapping**: translates Claude model names to OpenAI-compatible names
//!   using per-provider `ClaudeModels` overrides (or sensible defaults).
//! - **ChatGPT backend compat**: filters and adjusts request bodies so they
//!   conform to the ChatGPT Responses API field whitelist.

use super::traits::{BridgeContext, ModelMapper};
use crate::domain::providers::ClaudeModels;
use crate::gateway::proxy::cx2cc::settings::Cx2ccSettings;
use serde_json::Value;

// ─── Model Mapper ──────────────────────────────────────────────────────────

/// Maps Claude model names to OpenAI-compatible model names.
///
/// The mapping logic mirrors `cx2cc::models::map_claude_to_openai` but is
/// expressed as a [`ModelMapper`] trait implementation so the protocol bridge
/// framework can use it generically.
pub(crate) struct CX2CCModelMapper;

impl ModelMapper for CX2CCModelMapper {
    fn map(&self, source_model: &str, ctx: &BridgeContext) -> String {
        map_claude_to_openai(source_model, &ctx.claude_models, &ctx.cx2cc_settings)
    }
}

fn map_claude_to_openai(source_model: &str, cm: &ClaudeModels, settings: &Cx2ccSettings) -> String {
    if source_model.contains("opus") {
        if let Some(ref m) = cm.opus_model {
            return m.clone();
        }
        return settings.fallback_model_opus.clone();
    }

    if source_model.contains("haiku") {
        if let Some(ref m) = cm.haiku_model {
            return m.clone();
        }
        return settings.fallback_model_haiku.clone();
    }

    if source_model.contains("sonnet") {
        if let Some(ref m) = cm.sonnet_model {
            return m.clone();
        }
        return settings.fallback_model_sonnet.clone();
    }

    if let Some(ref m) = cm.main_model {
        return m.clone();
    }
    settings.fallback_model_main.clone()
}

// ─── ChatGPT Backend Compat ────────────────────────────────────────────────

/// Field whitelist for the ChatGPT Responses API.
///
/// When routing a Codex CLI request through a ChatGPT-compatible backend, only
/// these top-level keys are forwarded; everything else is stripped to avoid
/// 400-level rejections from the upstream provider.
pub(crate) const CODEX_CHATGPT_RESPONSES_ALLOWED_KEYS: &[&str] = &[
    "model",
    "instructions",
    "input",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "store",
    "stream",
    "include",
    "reasoning",
    "service_tier",
    "prompt_cache_key",
    "text",
    "previous_response_id",
];

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ForeignHistoryNormalization {
    pub(crate) reasoning_items_removed: usize,
    pub(crate) previous_response_id_removed: bool,
}

impl ForeignHistoryNormalization {
    pub(crate) fn applied(self) -> bool {
        self.reasoning_items_removed > 0
    }
}

pub(crate) fn normalize_foreign_responses_history_for_chatgpt(
    root: &mut Value,
) -> ForeignHistoryNormalization {
    let Some(obj) = root.as_object_mut() else {
        return ForeignHistoryNormalization::default();
    };

    let reasoning_items_removed = {
        let Some(items) = obj.get_mut("input").and_then(Value::as_array_mut) else {
            return ForeignHistoryNormalization::default();
        };
        let before = items.len();
        items.retain(|item| {
            let is_plaintext_reasoning = item.get("type").and_then(Value::as_str)
                == Some("reasoning")
                && item
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|content| !content.is_empty());
            !is_plaintext_reasoning
        });
        before - items.len()
    };

    if reasoning_items_removed == 0 {
        return ForeignHistoryNormalization::default();
    }

    let previous_response_id_removed = obj.remove("previous_response_id").is_some();
    ForeignHistoryNormalization {
        reasoning_items_removed,
        previous_response_id_removed,
    }
}

/// Filter a request body to only the ChatGPT Responses API allowed keys, then
/// force `stream: true`, `store: false`, and coerce `instructions` to a string.
///
/// If `root` is not a JSON object it is returned unchanged.
pub(crate) fn codex_chatgpt_request_compat_value(root: &Value) -> Value {
    let Some(obj) = root.as_object() else {
        return root.clone();
    };

    let mut next = serde_json::Map::new();
    for key in CODEX_CHATGPT_RESPONSES_ALLOWED_KEYS {
        if let Some(value) = obj.get(*key).cloned() {
            next.insert((*key).to_string(), value);
        }
    }
    next.insert("stream".to_string(), Value::Bool(true));
    next.insert("store".to_string(), Value::Bool(false));
    let instructions_needs_coercion = next.get("instructions").and_then(Value::as_str).is_none();
    if instructions_needs_coercion {
        next.insert("instructions".to_string(), Value::String(String::new()));
    }
    Value::Object(next)
}

/// Returns `true` when the original Anthropic request body (captured before
/// CX2CC translation) had `"stream": true`.
///
/// `introspection_json` is the pre-translation snapshot of the request body
/// that the failover loop keeps for diagnostics / compat decisions.
pub(crate) fn original_anthropic_stream_requested(introspection_json: Option<&Value>) -> bool {
    introspection_json
        .and_then(|body| body.get("stream"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::providers::ClaudeModels;
    use serde_json::json;

    fn default_ctx() -> BridgeContext {
        BridgeContext {
            claude_models: ClaudeModels::default(),
            model_mapping: Default::default(),
            cx2cc_settings: Cx2ccSettings::default(),
            requested_model: None,
            mapped_model: None,
            stream_requested: false,
            is_chatgpt_backend: false,
        }
    }

    fn ctx_with_models(cm: ClaudeModels) -> BridgeContext {
        BridgeContext {
            claude_models: cm,
            ..default_ctx()
        }
    }

    // ── Model mapping: default values ──────────────────────────────────────

    #[test]
    fn maps_opus_to_default_o3() {
        let mapper = CX2CCModelMapper;
        assert_eq!(
            mapper.map("claude-3-opus-20240229", &default_ctx()),
            "gpt-5.4"
        );
    }

    #[test]
    fn maps_haiku_to_default_gpt41_mini() {
        let mapper = CX2CCModelMapper;
        assert_eq!(
            mapper.map("claude-3-haiku-20240307", &default_ctx()),
            "gpt-5.4"
        );
    }

    #[test]
    fn maps_sonnet_to_default_gpt41() {
        let mapper = CX2CCModelMapper;
        assert_eq!(
            mapper.map("claude-3-5-sonnet-20241022", &default_ctx()),
            "gpt-5.4"
        );
    }

    #[test]
    fn maps_unknown_model_to_default_gpt41() {
        let mapper = CX2CCModelMapper;
        assert_eq!(mapper.map("some-unknown-model", &default_ctx()), "gpt-5.4");
    }

    // ── Model mapping: custom overrides ────────────────────────────────────

    #[test]
    fn opus_override() {
        let cm = ClaudeModels {
            opus_model: Some("my-opus".into()),
            ..ClaudeModels::default()
        };
        let mapper = CX2CCModelMapper;
        assert_eq!(
            mapper.map("claude-3-opus-20240229", &ctx_with_models(cm)),
            "my-opus"
        );
    }

    #[test]
    fn haiku_override() {
        let cm = ClaudeModels {
            haiku_model: Some("my-haiku".into()),
            ..ClaudeModels::default()
        };
        let mapper = CX2CCModelMapper;
        assert_eq!(
            mapper.map("claude-3-haiku-20240307", &ctx_with_models(cm)),
            "my-haiku"
        );
    }

    #[test]
    fn sonnet_override() {
        let cm = ClaudeModels {
            sonnet_model: Some("my-sonnet".into()),
            ..ClaudeModels::default()
        };
        let mapper = CX2CCModelMapper;
        assert_eq!(
            mapper.map("claude-3-5-sonnet-20241022", &ctx_with_models(cm)),
            "my-sonnet"
        );
    }

    #[test]
    fn main_model_override_for_unknown() {
        let cm = ClaudeModels {
            main_model: Some("my-main".into()),
            ..ClaudeModels::default()
        };
        let mapper = CX2CCModelMapper;
        assert_eq!(
            mapper.map("some-unknown-model", &ctx_with_models(cm)),
            "my-main"
        );
    }

    #[test]
    fn runtime_settings_override_fallbacks() {
        let mut ctx = default_ctx();
        ctx.cx2cc_settings = Cx2ccSettings {
            fallback_model_opus: "custom-opus".into(),
            fallback_model_sonnet: "custom-sonnet".into(),
            fallback_model_haiku: "custom-haiku".into(),
            fallback_model_main: "custom-main".into(),
            ..Cx2ccSettings::default()
        };

        let mapper = CX2CCModelMapper;
        assert_eq!(mapper.map("claude-3-opus-20240229", &ctx), "custom-opus");
        assert_eq!(mapper.map("claude-3-haiku-20240307", &ctx), "custom-haiku");
        assert_eq!(
            mapper.map("claude-3-5-sonnet-20241022", &ctx),
            "custom-sonnet"
        );
        assert_eq!(mapper.map("some-unknown-model", &ctx), "custom-main");
    }

    // ── ChatGPT compat filter ──────────────────────────────────────────────

    #[test]
    fn foreign_history_normalizes_plaintext_reasoning_without_rewriting_other_items() {
        let expected_input = json!([
            {"role": "developer", "content": [{"type": "input_text", "text": "system"}]},
            {"role": "user", "content": [
                {"type": "input_text", "text": "make marker"},
                {"type": "input_image", "image_url": "data:image/png;base64,abc"}
            ]},
            {"role": "assistant", "content": [{"type": "output_text", "text": "AU_MARKER"}]},
            {"type": "function_call", "call_id": "call_1", "name": "read_file", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"},
            {"role": "user", "content": [{"type": "input_text", "text": "repeat marker"}]}
        ]);
        let mut root = json!({
            "model": "gpt-5.5",
            "previous_response_id": "resp_longcat",
            "input": [
                {"role": "developer", "content": [{"type": "input_text", "text": "system"}]},
                {"role": "user", "content": [
                    {"type": "input_text", "text": "make marker"},
                    {"type": "input_image", "image_url": "data:image/png;base64,abc"}
                ]},
                {"type": "reasoning", "id": "rs_longcat", "content": [
                    {"type": "reasoning_text", "text": "plaintext reasoning must be discarded"}
                ]},
                {"role": "assistant", "content": [{"type": "output_text", "text": "AU_MARKER"}]},
                {"type": "function_call", "call_id": "call_1", "name": "read_file", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"},
                {"role": "user", "content": [{"type": "input_text", "text": "repeat marker"}]}
            ]
        });

        let outcome = normalize_foreign_responses_history_for_chatgpt(&mut root);

        assert_eq!(outcome.reasoning_items_removed, 1);
        assert!(outcome.previous_response_id_removed);
        assert_eq!(root["input"], expected_input);
        assert_eq!(root["input"][2]["role"], "assistant");
        assert_eq!(root["input"][2]["content"][0]["text"], "AU_MARKER");
        assert!(root.get("previous_response_id").is_none());
        assert!(!root.to_string().contains("plaintext reasoning"));
    }

    #[test]
    fn foreign_history_compatible_shapes_are_noops() {
        let cases = [
            json!({
                "input": [
                    {"type": "reasoning", "content": null, "encrypted_content": "ciphertext"},
                    {"role": "assistant", "content": [{"type": "output_text", "text": "IK_MARKER"}]}
                ]
            }),
            json!({
                "input": [
                    {"type": "reasoning", "content": [], "encrypted_content": "ciphertext"},
                    {"role": "assistant", "content": [{"type": "output_text", "text": "IK_MARKER"}]}
                ]
            }),
            json!({
                "input": [
                    {"role": "assistant", "content": [{"type": "output_text", "text": "XF_MARKER"}]}
                ]
            }),
            json!({"input": "hello"}),
        ];

        for original in cases {
            let mut root = original.clone();
            let outcome = normalize_foreign_responses_history_for_chatgpt(&mut root);
            assert!(!outcome.applied());
            assert_eq!(root, original);
        }
    }

    #[test]
    fn foreign_history_normalization_is_idempotent_for_tool_only_input() {
        let mut root = json!({
            "previous_response_id": "resp_longcat",
            "input": [
                {"type": "reasoning", "content": [{"type": "reasoning_text", "text": "drop"}]},
                {"type": "function_call_output", "call_id": "call_1", "output": "tool result"}
            ]
        });

        let first = normalize_foreign_responses_history_for_chatgpt(&mut root);
        let once = root.clone();
        let second = normalize_foreign_responses_history_for_chatgpt(&mut root);

        assert_eq!(first.reasoning_items_removed, 1);
        assert!(first.previous_response_id_removed);
        assert!(!second.applied());
        assert_eq!(root, once);
        assert_eq!(root["input"][0]["type"], "function_call_output");
    }

    #[test]
    fn compat_keeps_allowed_keys_only() {
        let root = json!({
            "model": "gpt-5",
            "instructions": "system prompt",
            "input": "hello",
            "temperature": 0.7,
            "extra_field": true,
        });
        let next = codex_chatgpt_request_compat_value(&root);

        assert_eq!(next["model"], "gpt-5");
        assert_eq!(next["instructions"], "system prompt");
        assert_eq!(next["input"], "hello");
        // Stripped fields
        assert!(next.get("temperature").is_none());
        assert!(next.get("extra_field").is_none());
    }

    #[test]
    fn compat_preserves_assistant_messages_for_chatgpt_backend() {
        let root = json!({
            "model": "gpt-5",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"role": "assistant", "content": [{"type": "output_text", "text": "old reply"}]},
                {"type": "function_call", "call_id": "call_1", "name": "read_file", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
            ]
        });

        let next = codex_chatgpt_request_compat_value(&root);
        let input = next["input"].as_array().expect("input array");

        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["text"], "old reply");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[3]["type"], "function_call_output");
    }

    #[test]
    fn compat_forces_stream_true_and_store_false() {
        let root = json!({
            "model": "gpt-5",
            "stream": false,
            "store": true,
        });
        let next = codex_chatgpt_request_compat_value(&root);

        assert_eq!(next["stream"], true);
        assert_eq!(next["store"], false);
    }

    #[test]
    fn compat_injects_empty_instructions_when_missing() {
        let root = json!({
            "model": "gpt-5",
            "input": "hello"
        });

        let next = codex_chatgpt_request_compat_value(&root);

        assert_eq!(next["stream"], true);
        assert_eq!(next["store"], false);
        assert_eq!(next["instructions"], "");
    }

    #[test]
    fn compat_coerces_null_instructions_to_empty_string() {
        let root = json!({
            "model": "gpt-5",
            "input": "hello",
            "instructions": null
        });

        let next = codex_chatgpt_request_compat_value(&root);

        assert_eq!(next["stream"], true);
        assert_eq!(next["store"], false);
        assert_eq!(next["instructions"], "");
    }

    #[test]
    fn compat_returns_non_object_unchanged() {
        let root = json!("just a string");
        let next = codex_chatgpt_request_compat_value(&root);
        assert_eq!(next, root);
    }

    // ── original_anthropic_stream_requested ────────────────────────────────

    #[test]
    fn detects_stream_true() {
        let body = json!({"stream": true, "model": "claude-3"});
        assert!(original_anthropic_stream_requested(Some(&body)));
    }

    #[test]
    fn detects_stream_false() {
        let body = json!({"stream": false, "model": "claude-3"});
        assert!(!original_anthropic_stream_requested(Some(&body)));
    }

    #[test]
    fn returns_false_when_no_stream_field() {
        let body = json!({"model": "claude-3"});
        assert!(!original_anthropic_stream_requested(Some(&body)));
    }

    #[test]
    fn returns_false_when_none() {
        assert!(!original_anthropic_stream_requested(None));
    }
}
