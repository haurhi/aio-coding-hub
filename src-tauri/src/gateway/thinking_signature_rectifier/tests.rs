use super::*;
use serde_json::json;

#[test]
fn detect_trigger_invalid_signature_in_thinking_block() {
    let trigger = detect_trigger("messages.1.content.0: Invalid `signature` in `thinking` block");
    assert_eq!(trigger, Some(TRIGGER_INVALID_SIGNATURE_IN_THINKING_BLOCK));

    let trigger2 = detect_trigger("Messages.1.Content.0: invalid signature in thinking block");
    assert_eq!(trigger2, Some(TRIGGER_INVALID_SIGNATURE_IN_THINKING_BLOCK));
}

#[test]
fn detect_trigger_signature_field_required_variants() {
    let trigger = detect_trigger("messages.0.content.1.signature: Field required");
    assert_eq!(trigger, Some(TRIGGER_INVALID_SIGNATURE_IN_THINKING_BLOCK));

    let trigger2 = detect_trigger("messages.0.content.1.signature: field required");
    assert_eq!(trigger2, Some(TRIGGER_INVALID_SIGNATURE_IN_THINKING_BLOCK));
}

#[test]
fn detect_trigger_signature_extra_inputs_variants() {
    let trigger = detect_trigger("messages.0.content.1.signature: Extra inputs are not permitted");
    assert_eq!(trigger, Some(TRIGGER_INVALID_SIGNATURE_IN_THINKING_BLOCK));
}

#[test]
fn detect_trigger_thinking_block_cannot_be_modified_variants() {
    let trigger = detect_trigger("thinking or redacted_thinking blocks cannot be modified");
    assert_eq!(trigger, Some(TRIGGER_INVALID_SIGNATURE_IN_THINKING_BLOCK));
}

#[test]
fn detect_trigger_missing_thinking_prefix() {
    let trigger = detect_trigger(
        "messages.69.content.0.type: Expected `thinking` or `redacted_thinking`, but found `tool_use`. When `thinking` is enabled, a final `assistant` message must start with a thinking block (preceeding the lastmost set of `tool_use` and `tool_result` blocks). To avoid this requirement, disable `thinking`.",
    );
    assert_eq!(
        trigger,
        Some(TRIGGER_ASSISTANT_MESSAGE_MUST_START_WITH_THINKING)
    );
}

#[test]
fn detect_trigger_deepseek_thinking_must_be_passed_back() {
    let trigger = detect_trigger(
        "The `content[].thinking` in the thinking mode must be passed back to the API.",
    );
    assert_eq!(trigger, Some(TRIGGER_DEEPSEEK_THINKING_MUST_BE_PASSED_BACK));
}

#[test]
fn detect_trigger_deepseek_passback_is_chat_completion_bridge_scoped() {
    let error = "The `content[].thinking` in the thinking mode must be passed back to the API.";

    assert_eq!(
        detect_trigger_for_protocol_bridge(
            error,
            Some(crate::providers::CLAUDE_CHAT_COMPLETIONS_BRIDGE_TYPE)
        ),
        Some(TRIGGER_DEEPSEEK_THINKING_MUST_BE_PASSED_BACK)
    );
    assert_eq!(detect_trigger_for_protocol_bridge(error, None), None);
    assert_eq!(
        detect_trigger_for_protocol_bridge(error, Some(crate::providers::CX2CC_BRIDGE_TYPE)),
        None
    );
}

#[test]
fn detect_trigger_deepseek_passback_allows_direct_deepseek_anthropic() {
    let error = "The `content[].thinking` in the thinking mode must be passed back to the API.";

    assert_eq!(
        detect_trigger_for_request(error, None, Some("https://api.deepseek.com/anthropic")),
        Some(TRIGGER_DEEPSEEK_THINKING_MUST_BE_PASSED_BACK)
    );
    assert_eq!(
        detect_trigger_for_request(error, None, Some("https://example.com/anthropic")),
        None
    );
    assert_eq!(
        detect_trigger_for_request(
            error,
            Some(crate::providers::CX2CC_BRIDGE_TYPE),
            Some("https://api.deepseek.com/anthropic")
        ),
        None
    );
}

#[test]
fn detect_trigger_invalid_request_with_thinking_context() {
    assert_eq!(
        detect_trigger("非法请求: thinking block signature mismatch"),
        Some(TRIGGER_INVALID_REQUEST)
    );
    assert_eq!(
        detect_trigger("illegal request: invalid thinking parameter"),
        Some(TRIGGER_INVALID_REQUEST)
    );
    assert_eq!(
        detect_trigger("invalid request: signature verification failed"),
        Some(TRIGGER_INVALID_REQUEST)
    );
    assert_eq!(
        detect_trigger("invalid request: redacted block error"),
        Some(TRIGGER_INVALID_REQUEST)
    );
}

#[test]
fn detect_trigger_invalid_request_without_thinking_context_returns_none() {
    assert_eq!(detect_trigger("非法请求"), None);
    assert_eq!(detect_trigger("illegal request format"), None);
    assert_eq!(detect_trigger("invalid request: malformed JSON"), None);
}

#[test]
fn detect_trigger_unrelated_error() {
    assert_eq!(detect_trigger("Request timeout"), None);
}

#[test]
fn rectify_removes_thinking_blocks_and_signature_fields() {
    let mut message = json!({
        "model": "claude-test",
        "messages": [
            {
                "role": "assistant",
                "content": [
                    { "type": "thinking", "thinking": "t", "signature": "sig_thinking" },
                    { "type": "text", "text": "hello", "signature": "sig_text_should_remove" },
                    { "type": "tool_use", "id": "toolu_1", "name": "WebSearch", "input": { "query": "q" }, "signature": "sig_tool_should_remove" },
                    { "type": "redacted_thinking", "data": "r", "signature": "sig_redacted" }
                ]
            },
            {
                "role": "user",
                "content": [ { "type": "text", "text": "hi" } ]
            }
        ]
    });

    let result = rectify_anthropic_request_message(&mut message);
    assert!(result.applied);
    assert_eq!(result.removed_thinking_blocks, 1);
    assert_eq!(result.removed_redacted_thinking_blocks, 1);
    assert_eq!(result.removed_signature_fields, 2);

    let content = message["messages"][0]["content"]
        .as_array()
        .expect("content should be array");
    let types: Vec<_> = content
        .iter()
        .map(|v| v["type"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(types, vec!["text", "tool_use"]);
    assert!(content[0].get("signature").is_none());
    assert!(content[1].get("signature").is_none());
}

#[test]
fn rectify_no_messages_should_not_modify() {
    let mut message = json!({ "model": "claude-test" });
    let result = rectify_anthropic_request_message(&mut message);
    assert!(!result.applied);
    assert_eq!(result.removed_thinking_blocks, 0);
    assert_eq!(result.removed_redacted_thinking_blocks, 0);
    assert_eq!(result.removed_signature_fields, 0);
}

#[test]
fn rectify_removes_top_level_thinking_when_tool_use_without_thinking_prefix() {
    let mut message = json!({
        "model": "claude-test",
        "thinking": { "type": "enabled", "budget_tokens": 1024 },
        "messages": [
            {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "toolu_1", "name": "WebSearch", "input": { "query": "q" } }
                ]
            },
            {
                "role": "user",
                "content": [ { "type": "tool_result", "tool_use_id": "toolu_1", "content": "ok" } ]
            }
        ]
    });

    let result = rectify_anthropic_request_message(&mut message);
    assert!(result.applied);
    assert!(result.removed_top_level_thinking);
    assert!(message.get("thinking").is_none());
}

#[test]
fn rectify_removes_top_level_thinking_when_assistant_history_lacks_thinking_blocks() {
    let mut message = json!({
        "model": "claude-test",
        "thinking": { "type": "enabled", "budget_tokens": 1024 },
        "messages": [
            {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "previous answer" }
                ]
            },
            {
                "role": "user",
                "content": [ { "type": "text", "text": "continue" } ]
            }
        ]
    });

    let result = rectify_anthropic_request_message_for_trigger(
        &mut message,
        TRIGGER_DEEPSEEK_THINKING_MUST_BE_PASSED_BACK,
        Some(crate::providers::CLAUDE_CHAT_COMPLETIONS_BRIDGE_TYPE),
    );
    assert!(result.applied);
    assert!(result.removed_top_level_thinking);
    assert!(message.get("thinking").is_none());
    assert_eq!(
        message["messages"][0]["content"][0]["text"].as_str(),
        Some("previous answer")
    );
}

#[test]
fn rectify_merges_adjacent_assistant_chunks_for_direct_deepseek_passback() {
    let mut message = json!({
        "model": "deepseek-v4-pro",
        "messages": [
            {
                "role": "user",
                "content": [ { "type": "text", "text": "open page" } ]
            },
            {
                "role": "assistant",
                "content": [
                    { "type": "thinking", "thinking": "I should inspect the page", "signature": "sig-1" }
                ]
            },
            {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "I will open the page." }
                ]
            },
            {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "toolu_1", "name": "browser_navigate", "input": { "url": "https://example.com" } }
                ]
            },
            {
                "role": "user",
                "content": [ { "type": "tool_result", "tool_use_id": "toolu_1", "content": "ok" } ]
            }
        ]
    });

    let result = rectify_anthropic_request_message_for_request(
        &mut message,
        TRIGGER_DEEPSEEK_THINKING_MUST_BE_PASSED_BACK,
        None,
        Some("https://api.deepseek.com/anthropic"),
    );

    assert!(result.applied);
    assert_eq!(result.merged_adjacent_assistant_messages, 2);

    let messages = message["messages"]
        .as_array()
        .expect("messages remain array");
    assert_eq!(messages.len(), 3);
    let merged_content = messages[1]["content"]
        .as_array()
        .expect("assistant content remains array");
    let types: Vec<_> = merged_content
        .iter()
        .map(|v| v["type"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(types, vec!["thinking", "text", "tool_use"]);
    assert_eq!(merged_content[0]["signature"].as_str(), Some("sig-1"));
}

#[test]
fn rectify_keeps_top_level_thinking_for_direct_provider_without_tool_use() {
    let mut message = json!({
        "model": "claude-test",
        "thinking": { "type": "enabled", "budget_tokens": 1024 },
        "messages": [
            {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "previous answer" }
                ]
            },
            {
                "role": "user",
                "content": [ { "type": "text", "text": "continue" } ]
            }
        ]
    });

    let result = rectify_anthropic_request_message_for_trigger(
        &mut message,
        TRIGGER_DEEPSEEK_THINKING_MUST_BE_PASSED_BACK,
        None,
    );
    assert!(!result.applied);
    assert!(!result.removed_top_level_thinking);
    assert!(message.get("thinking").is_some());
}
