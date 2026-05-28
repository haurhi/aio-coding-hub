pub(super) type ThinkingSignatureRectifierTrigger = &'static str;

pub(super) const TRIGGER_INVALID_SIGNATURE_IN_THINKING_BLOCK: ThinkingSignatureRectifierTrigger =
    "invalid_signature_in_thinking_block";
pub(super) const TRIGGER_ASSISTANT_MESSAGE_MUST_START_WITH_THINKING:
    ThinkingSignatureRectifierTrigger = "assistant_message_must_start_with_thinking";
pub(super) const TRIGGER_DEEPSEEK_THINKING_MUST_BE_PASSED_BACK: ThinkingSignatureRectifierTrigger =
    "deepseek_thinking_must_be_passed_back";
pub(super) const TRIGGER_INVALID_REQUEST: ThinkingSignatureRectifierTrigger = "invalid_request";

#[derive(Debug, Clone, Copy)]
pub(super) struct ThinkingSignatureRectifierResult {
    pub(super) applied: bool,
    pub(super) removed_thinking_blocks: usize,
    pub(super) removed_redacted_thinking_blocks: usize,
    pub(super) removed_signature_fields: usize,
    pub(super) removed_top_level_thinking: bool,
    pub(super) disabled_top_level_thinking: bool,
    pub(super) merged_adjacent_assistant_messages: usize,
}

pub(super) fn detect_trigger(error_message: &str) -> Option<ThinkingSignatureRectifierTrigger> {
    if error_message.trim().is_empty() {
        return None;
    }

    let lower = error_message.to_lowercase();

    let looks_like_thinking_enabled_but_missing_thinking_prefix = lower
        .contains("must start with a thinking block")
        || (lower.contains("expected")
            && lower.contains("thinking")
            && (lower.contains("redacted_thinking") || lower.contains("redacted thinking"))
            && lower.contains("found")
            && (lower.contains("tool_use") || lower.contains("tool use")));

    if looks_like_thinking_enabled_but_missing_thinking_prefix {
        return Some(TRIGGER_ASSISTANT_MESSAGE_MUST_START_WITH_THINKING);
    }

    let looks_like_deepseek_thinking_must_be_passed_back = lower.contains("content[].thinking")
        && lower.contains("thinking mode")
        && lower.contains("passed back");
    if looks_like_deepseek_thinking_must_be_passed_back {
        return Some(TRIGGER_DEEPSEEK_THINKING_MUST_BE_PASSED_BACK);
    }

    let looks_like_invalid_signature_in_thinking_block = lower.contains("invalid")
        && lower.contains("signature")
        && lower.contains("thinking")
        && lower.contains("block");
    if looks_like_invalid_signature_in_thinking_block {
        return Some(TRIGGER_INVALID_SIGNATURE_IN_THINKING_BLOCK);
    }

    let looks_like_missing_signature_field =
        lower.contains("signature") && lower.contains("field required");
    if looks_like_missing_signature_field {
        return Some(TRIGGER_INVALID_SIGNATURE_IN_THINKING_BLOCK);
    }

    let looks_like_extra_signature_field =
        lower.contains("signature") && lower.contains("extra inputs are not permitted");
    if looks_like_extra_signature_field {
        return Some(TRIGGER_INVALID_SIGNATURE_IN_THINKING_BLOCK);
    }

    let looks_like_thinking_block_modified = (lower.contains("thinking")
        || lower.contains("redacted_thinking"))
        && lower.contains("cannot be modified");
    if looks_like_thinking_block_modified {
        return Some(TRIGGER_INVALID_SIGNATURE_IN_THINKING_BLOCK);
    }

    let looks_like_generic_invalid_request = error_message.contains("非法请求")
        || lower.contains("illegal request")
        || lower.contains("invalid request");
    if looks_like_generic_invalid_request
        && (lower.contains("thinking") || lower.contains("signature") || lower.contains("redacted"))
    {
        return Some(TRIGGER_INVALID_REQUEST);
    }

    None
}

#[cfg(test)]
pub(super) fn detect_trigger_for_protocol_bridge(
    error_message: &str,
    protocol_bridge_type: Option<&str>,
) -> Option<ThinkingSignatureRectifierTrigger> {
    detect_trigger_for_request(error_message, protocol_bridge_type, None)
}

pub(super) fn detect_trigger_for_request(
    error_message: &str,
    protocol_bridge_type: Option<&str>,
    provider_base_url: Option<&str>,
) -> Option<ThinkingSignatureRectifierTrigger> {
    let trigger = detect_trigger(error_message)?;
    if trigger != TRIGGER_DEEPSEEK_THINKING_MUST_BE_PASSED_BACK {
        return Some(trigger);
    }

    if protocol_bridge_type == Some(crate::providers::CLAUDE_CHAT_COMPLETIONS_BRIDGE_TYPE) {
        return Some(trigger);
    }

    if protocol_bridge_type.is_none() && is_direct_deepseek_anthropic_provider(provider_base_url) {
        return Some(trigger);
    }

    None
}

fn is_direct_deepseek_anthropic_provider(provider_base_url: Option<&str>) -> bool {
    let Some(base_url) = provider_base_url else {
        return false;
    };
    let lower = base_url.trim().to_ascii_lowercase();
    lower.contains("api.deepseek.com") && lower.contains("/anthropic")
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct ThinkingSignatureRectifierOptions {
    pub(super) strip_thinking_blocks_and_signatures: bool,
    pub(super) remove_top_level_thinking_when_any_assistant_lacks_thinking: bool,
    pub(super) disable_top_level_thinking_instead_of_remove: bool,
    pub(super) merge_adjacent_assistant_messages: bool,
}

impl ThinkingSignatureRectifierOptions {
    #[cfg(test)]
    fn strip_thinking_blocks_and_signatures() -> Self {
        Self {
            strip_thinking_blocks_and_signatures: true,
            remove_top_level_thinking_when_any_assistant_lacks_thinking: false,
            disable_top_level_thinking_instead_of_remove: false,
            merge_adjacent_assistant_messages: false,
        }
    }
}

fn is_direct_deepseek_passback_request(
    trigger: ThinkingSignatureRectifierTrigger,
    protocol_bridge_type: Option<&str>,
    provider_base_url: Option<&str>,
) -> bool {
    trigger == TRIGGER_DEEPSEEK_THINKING_MUST_BE_PASSED_BACK
        && protocol_bridge_type.is_none()
        && is_direct_deepseek_anthropic_provider(provider_base_url)
}

fn options_for_trigger(
    trigger: ThinkingSignatureRectifierTrigger,
    protocol_bridge_type: Option<&str>,
    provider_base_url: Option<&str>,
) -> ThinkingSignatureRectifierOptions {
    if is_direct_deepseek_passback_request(trigger, protocol_bridge_type, provider_base_url) {
        return ThinkingSignatureRectifierOptions {
            strip_thinking_blocks_and_signatures: false,
            remove_top_level_thinking_when_any_assistant_lacks_thinking: true,
            disable_top_level_thinking_instead_of_remove: true,
            merge_adjacent_assistant_messages: true,
        };
    }

    ThinkingSignatureRectifierOptions {
        strip_thinking_blocks_and_signatures: true,
        remove_top_level_thinking_when_any_assistant_lacks_thinking: trigger
            == TRIGGER_DEEPSEEK_THINKING_MUST_BE_PASSED_BACK
            && protocol_bridge_type == Some(crate::providers::CLAUDE_CHAT_COMPLETIONS_BRIDGE_TYPE),
        disable_top_level_thinking_instead_of_remove: false,
        merge_adjacent_assistant_messages: false,
    }
}

fn merge_adjacent_assistant_messages(messages: &mut Vec<serde_json::Value>) -> usize {
    let original = std::mem::take(messages);
    let mut merged_count = 0usize;
    let mut next_messages: Vec<serde_json::Value> = Vec::with_capacity(original.len());

    for msg in original {
        let is_assistant = msg
            .as_object()
            .and_then(|obj| obj.get("role"))
            .and_then(|v| v.as_str())
            == Some("assistant");

        if is_assistant {
            if let Some(prev) = next_messages.last_mut() {
                let prev_is_assistant = prev
                    .as_object()
                    .and_then(|obj| obj.get("role"))
                    .and_then(|v| v.as_str())
                    == Some("assistant");

                if prev_is_assistant {
                    let maybe_prev_content = prev
                        .as_object_mut()
                        .and_then(|obj| obj.get_mut("content"))
                        .and_then(|v| v.as_array_mut());
                    let maybe_current_content = msg
                        .as_object()
                        .and_then(|obj| obj.get("content"))
                        .and_then(|v| v.as_array());

                    if let (Some(prev_content), Some(current_content)) =
                        (maybe_prev_content, maybe_current_content)
                    {
                        prev_content.extend(current_content.iter().cloned());
                        merged_count += 1;
                        continue;
                    }
                }
            }
        }

        next_messages.push(msg);
    }

    *messages = next_messages;
    merged_count
}

fn strip_thinking_blocks_and_signatures(
    messages: &mut [serde_json::Value],
) -> (usize, usize, usize, bool) {
    let mut removed_thinking_blocks = 0usize;
    let mut removed_redacted_thinking_blocks = 0usize;
    let mut removed_signature_fields = 0usize;
    let mut applied = false;

    for msg in messages.iter_mut() {
        let Some(msg_obj) = msg.as_object_mut() else {
            continue;
        };

        let Some(content) = msg_obj.get_mut("content").and_then(|v| v.as_array_mut()) else {
            continue;
        };

        let original = std::mem::take(content);
        let mut new_content: Vec<serde_json::Value> = Vec::with_capacity(original.len());
        let mut content_modified = false;

        for mut block in original {
            let Some(block_obj) = block.as_object_mut() else {
                new_content.push(block);
                continue;
            };

            match block_obj.get("type").and_then(|v| v.as_str()) {
                Some("thinking") => {
                    removed_thinking_blocks += 1;
                    content_modified = true;
                    continue;
                }
                Some("redacted_thinking") => {
                    removed_redacted_thinking_blocks += 1;
                    content_modified = true;
                    continue;
                }
                _ => {}
            }

            if block_obj.remove("signature").is_some() {
                removed_signature_fields += 1;
                content_modified = true;
            }

            new_content.push(block);
        }

        if content_modified {
            applied = true;
        }
        *content = new_content;
    }

    (
        removed_thinking_blocks,
        removed_redacted_thinking_blocks,
        removed_signature_fields,
        applied,
    )
}

#[cfg(test)]
pub(super) fn rectify_anthropic_request_message(
    message: &mut serde_json::Value,
) -> ThinkingSignatureRectifierResult {
    rectify_anthropic_request_message_with_options(
        message,
        ThinkingSignatureRectifierOptions::strip_thinking_blocks_and_signatures(),
    )
}

#[cfg(test)]
pub(super) fn rectify_anthropic_request_message_for_trigger(
    message: &mut serde_json::Value,
    trigger: ThinkingSignatureRectifierTrigger,
    protocol_bridge_type: Option<&str>,
) -> ThinkingSignatureRectifierResult {
    rectify_anthropic_request_message_for_request(message, trigger, protocol_bridge_type, None)
}

pub(super) fn rectify_anthropic_request_message_for_request(
    message: &mut serde_json::Value,
    trigger: ThinkingSignatureRectifierTrigger,
    protocol_bridge_type: Option<&str>,
    provider_base_url: Option<&str>,
) -> ThinkingSignatureRectifierResult {
    let options = options_for_trigger(trigger, protocol_bridge_type, provider_base_url);
    rectify_anthropic_request_message_with_options(message, options)
}

pub(super) fn rectify_anthropic_request_message_with_options(
    message: &mut serde_json::Value,
    options: ThinkingSignatureRectifierOptions,
) -> ThinkingSignatureRectifierResult {
    let mut removed_thinking_blocks = 0usize;
    let mut removed_redacted_thinking_blocks = 0usize;
    let mut removed_signature_fields = 0usize;
    let mut removed_top_level_thinking = false;
    let mut disabled_top_level_thinking = false;
    let mut merged_adjacent_assistant_messages = 0usize;
    let mut applied = false;

    let Some(message_obj) = message.as_object_mut() else {
        return ThinkingSignatureRectifierResult {
            applied: false,
            removed_thinking_blocks,
            removed_redacted_thinking_blocks,
            removed_signature_fields,
            removed_top_level_thinking,
            disabled_top_level_thinking,
            merged_adjacent_assistant_messages,
        };
    };

    let thinking_type = message_obj
        .get("thinking")
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get("type"))
        .and_then(|v| v.as_str());
    let thinking_enabled = thinking_type == Some("enabled")
        || (options.disable_top_level_thinking_instead_of_remove
            && thinking_type != Some("disabled"));

    let mut should_remove_top_level_thinking = false;

    {
        let Some(messages) = message_obj
            .get_mut("messages")
            .and_then(|v| v.as_array_mut())
        else {
            return ThinkingSignatureRectifierResult {
                applied: false,
                removed_thinking_blocks,
                removed_redacted_thinking_blocks,
                removed_signature_fields,
                removed_top_level_thinking,
                disabled_top_level_thinking,
                merged_adjacent_assistant_messages,
            };
        };

        if options.merge_adjacent_assistant_messages {
            merged_adjacent_assistant_messages = merge_adjacent_assistant_messages(messages);
            applied = applied || merged_adjacent_assistant_messages > 0;
        }

        // Fallback: if top-level thinking is enabled, but the final assistant message doesn't start
        // with thinking/redacted_thinking AND contains tool_use, remove top-level thinking to avoid
        // Anthropic 400 "Expected thinking..., but found tool_use". DeepSeek's passback error is
        // stricter: if any historical assistant turn lacks a thinking block, disable thinking for
        // the retry because the missing passback cannot be reconstructed.
        if thinking_enabled {
            if options.remove_top_level_thinking_when_any_assistant_lacks_thinking {
                should_remove_top_level_thinking = messages
                    .iter()
                    .filter_map(|msg| msg.as_object())
                    .any(|msg_obj| {
                        if msg_obj.get("role").and_then(|v| v.as_str()) != Some("assistant") {
                            return false;
                        }
                        let Some(content) = msg_obj.get("content").and_then(|v| v.as_array())
                        else {
                            return false;
                        };
                        let has_thinking_block = content.iter().any(|block| {
                            matches!(
                                block
                                    .as_object()
                                    .and_then(|obj| obj.get("type"))
                                    .and_then(|v| v.as_str()),
                                Some("thinking") | Some("redacted_thinking")
                            )
                        });
                        !has_thinking_block
                    });
            }

            let last_assistant_content = messages.iter().rev().find_map(|msg| {
                let msg_obj = msg.as_object()?;
                if msg_obj.get("role").and_then(|v| v.as_str()) != Some("assistant") {
                    return None;
                }
                msg_obj.get("content").and_then(|v| v.as_array())
            });

            if let Some(content) = last_assistant_content {
                if let Some(first_block) = content.first() {
                    let first_block_type = first_block
                        .as_object()
                        .and_then(|obj| obj.get("type"))
                        .and_then(|v| v.as_str());

                    let missing_thinking_prefix = first_block_type != Some("thinking")
                        && first_block_type != Some("redacted_thinking");

                    if missing_thinking_prefix {
                        let has_tool_use = content.iter().any(|block| {
                            block
                                .as_object()
                                .and_then(|obj| obj.get("type"))
                                .and_then(|v| v.as_str())
                                == Some("tool_use")
                        });

                        if has_tool_use {
                            should_remove_top_level_thinking = true;
                        }
                    }
                }
            }
        }

        if options.strip_thinking_blocks_and_signatures || should_remove_top_level_thinking {
            let (
                stripped_thinking_blocks,
                stripped_redacted_thinking_blocks,
                stripped_signature_fields,
                strip_applied,
            ) = strip_thinking_blocks_and_signatures(messages);
            removed_thinking_blocks += stripped_thinking_blocks;
            removed_redacted_thinking_blocks += stripped_redacted_thinking_blocks;
            removed_signature_fields += stripped_signature_fields;
            applied = applied || strip_applied;
        }
    }

    if should_remove_top_level_thinking {
        if options.disable_top_level_thinking_instead_of_remove {
            let already_disabled = message_obj
                .get("thinking")
                .and_then(|v| v.as_object())
                .and_then(|obj| obj.get("type"))
                .and_then(|v| v.as_str())
                == Some("disabled");
            if !already_disabled {
                message_obj.insert(
                    "thinking".to_string(),
                    serde_json::json!({ "type": "disabled" }),
                );
                disabled_top_level_thinking = true;
                applied = true;
            }
        } else if message_obj.remove("thinking").is_some() {
            removed_top_level_thinking = true;
            applied = true;
        }
    }

    ThinkingSignatureRectifierResult {
        applied,
        removed_thinking_blocks,
        removed_redacted_thinking_blocks,
        removed_signature_fields,
        removed_top_level_thinking,
        disabled_top_level_thinking,
        merged_adjacent_assistant_messages,
    }
}

#[cfg(test)]
mod tests;
