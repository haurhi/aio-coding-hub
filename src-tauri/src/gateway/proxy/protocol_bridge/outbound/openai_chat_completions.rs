//! Outbound adapter: IR <-> OpenAI-compatible Chat Completions.

use crate::gateway::proxy::protocol_bridge::ir::*;
use crate::gateway::proxy::protocol_bridge::traits::*;
use serde_json::{json, Value};

pub(crate) struct OpenAIChatCompletionsOutbound;

const PENDING_FINISH_REASON_KEY: &str = "openai_chat_completions_pending_finish_reason";

impl Outbound for OpenAIChatCompletionsOutbound {
    fn protocol(&self) -> &'static str {
        "openai_chat_completions"
    }

    fn target_path(&self) -> &str {
        "/chat/completions"
    }

    fn ir_to_request(
        &self,
        ir: &InternalRequest,
        _ctx: &BridgeContext,
    ) -> Result<Value, BridgeError> {
        ir_to_request(ir)
    }

    fn response_to_ir(
        &self,
        body: Value,
        _ctx: &BridgeContext,
    ) -> Result<InternalResponse, BridgeError> {
        response_to_ir(body)
    }

    fn sse_event_to_ir(
        &self,
        event_type: &str,
        data: &Value,
        state: &mut StreamState,
    ) -> Result<Vec<IRStreamChunk>, BridgeError> {
        sse_event_to_ir(event_type, data, state)
    }
}

fn ir_to_request(ir: &InternalRequest) -> Result<Value, BridgeError> {
    let mut messages = Vec::new();

    if let Some(system) = ir.system.as_deref().filter(|s| !s.is_empty()) {
        messages.push(json!({"role": "system", "content": system}));
    }

    for message in &ir.messages {
        append_message(&mut messages, message);
    }

    let mut body = json!({
        "model": ir.model,
        "messages": messages,
        "stream": ir.stream
    });
    if ir.stream {
        body["stream_options"] = json!({"include_usage": true});
    }

    if let Some(max_tokens) = ir.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    if let Some(temperature) = ir.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(top_p) = ir.top_p {
        body["top_p"] = json!(top_p);
    }
    if !ir.stop_sequences.is_empty() {
        body["stop"] = json!(ir.stop_sequences);
    }
    if !ir.tools.is_empty() {
        body["tools"] = json!(ir
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters
                    }
                })
            })
            .collect::<Vec<_>>());
    }
    if let Some(tool_choice) = &ir.tool_choice {
        body["tool_choice"] = match tool_choice {
            IRToolChoice::Auto => json!("auto"),
            IRToolChoice::Required => json!("required"),
            IRToolChoice::None => json!("none"),
            IRToolChoice::Specific { name } => {
                json!({"type": "function", "function": {"name": name}})
            }
        };
    }

    Ok(body)
}

fn append_message(messages: &mut Vec<Value>, message: &IRMessage) {
    let role = match message.role {
        IRRole::User => "user",
        IRRole::Assistant => "assistant",
    };

    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();
    let mut tool_results = Vec::new();

    for block in &message.content {
        match block {
            IRContentBlock::Text { text } => text_parts.push(text.as_str()),
            IRContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": serde_json::to_string(input).unwrap_or_default()
                    }
                }));
            }
            IRContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                tool_results.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": content
                }));
            }
            _ => {}
        }
    }

    for tool_result in tool_results {
        messages.push(tool_result);
    }

    if !tool_calls.is_empty() {
        messages.push(json!({
            "role": "assistant",
            "content": text_parts.join("\n\n"),
            "tool_calls": tool_calls
        }));
    } else if !text_parts.is_empty() {
        messages.push(json!({
            "role": role,
            "content": text_parts.join("\n\n")
        }));
    }
}

fn response_to_ir(body: Value) -> Result<InternalResponse, BridgeError> {
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let choice = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| BridgeError::TransformFailed("missing chat completion choice".into()))?;

    let mut content = Vec::new();
    let message = choice.get("message").unwrap_or(&Value::Null);
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            content.push(IRContentBlock::Text {
                text: text.to_string(),
            });
        }
    }
    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in tool_calls {
            if let Some(block) = parse_tool_call_block(call) {
                content.push(block);
            }
        }
    }

    let finish_reason = choice.get("finish_reason").and_then(Value::as_str);
    let stop_reason = stop_reason_from_finish_reason(finish_reason);
    let usage = parse_usage(body.get("usage"));

    Ok(InternalResponse {
        id,
        model,
        content,
        stop_reason,
        usage,
    })
}

fn parse_tool_call_block(call: &Value) -> Option<IRContentBlock> {
    let id = call
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let name = call
        .pointer("/function/name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let args = call
        .pointer("/function/arguments")
        .and_then(Value::as_str)
        .unwrap_or("{}");
    let input = serde_json::from_str(args).unwrap_or_else(|_| json!(args));

    Some(IRContentBlock::ToolUse { id, name, input })
}

fn parse_usage(usage: Option<&Value>) -> IRUsage {
    let Some(usage) = usage else {
        return IRUsage::default();
    };

    let cache_read_input_tokens = usage
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .or_else(|| {
            usage
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
        })
        .or_else(|| usage.get("cache_read_input_tokens").and_then(Value::as_u64));

    let cache_creation_5m_input_tokens = usage
        .get("cache_creation_5m_input_tokens")
        .and_then(Value::as_u64)
        .or_else(|| {
            usage
                .pointer("/cache_creation/ephemeral_5m_input_tokens")
                .and_then(Value::as_u64)
        })
        .or_else(|| {
            usage
                .get("claude_cache_creation_5_m_tokens")
                .and_then(Value::as_u64)
        });

    let cache_creation_1h_input_tokens = usage
        .get("cache_creation_1h_input_tokens")
        .and_then(Value::as_u64)
        .or_else(|| {
            usage
                .pointer("/cache_creation/ephemeral_1h_input_tokens")
                .and_then(Value::as_u64)
        })
        .or_else(|| {
            usage
                .get("claude_cache_creation_1_h_tokens")
                .and_then(Value::as_u64)
        });

    let cache_creation_input_tokens = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .or_else(|| {
            match (
                cache_creation_5m_input_tokens,
                cache_creation_1h_input_tokens,
            ) {
                (Some(a), Some(b)) => Some(a.saturating_add(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            }
        });

    IRUsage {
        input_tokens: usage
            .get("prompt_tokens")
            .or_else(|| usage.get("input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: usage
            .get("completion_tokens")
            .or_else(|| usage.get("output_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_creation_input_tokens,
        cache_creation_5m_input_tokens,
        cache_creation_1h_input_tokens,
        cache_read_input_tokens,
    }
}

fn stop_reason_from_finish_reason(finish_reason: Option<&str>) -> IRStopReason {
    match finish_reason {
        Some("tool_calls") | Some("function_call") => IRStopReason::ToolUse,
        Some("length") => IRStopReason::MaxTokens,
        Some("stop") => IRStopReason::EndTurn,
        Some(other) => IRStopReason::Unknown(other.to_string()),
        None => IRStopReason::EndTurn,
    }
}

fn sse_event_to_ir(
    event_type: &str,
    data: &Value,
    state: &mut StreamState,
) -> Result<Vec<IRStreamChunk>, BridgeError> {
    if state.stream_completed {
        return Ok(Vec::new());
    }

    let mut chunks = Vec::new();

    if event_type == "done" {
        if let Some(stop_reason) = pending_stop_reason(state) {
            complete_stream(state, &mut chunks, stop_reason, IRUsage::default());
        }
        return Ok(chunks);
    }

    if !stream_started(state) {
        chunks.push(IRStreamChunk::MessageStart {
            id: data
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            model: data
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            initial_usage: None,
        });
        mark_stream_started(state);
    }

    if is_usage_only_chunk(data) {
        let stop_reason = pending_stop_reason(state).unwrap_or(IRStopReason::EndTurn);
        complete_stream(
            state,
            &mut chunks,
            stop_reason,
            parse_usage(data.get("usage")),
        );
        return Ok(chunks);
    }

    let Some(choice) = data
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    else {
        return Ok(chunks);
    };

    let delta = choice.get("delta").unwrap_or(&Value::Null);
    if let Some(text) = delta.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            open_text_block_if_needed(state, &mut chunks);
            state.active_text.push_str(text);
            chunks.push(IRStreamChunk::ContentBlockDelta {
                index: state.block_index.saturating_sub(1),
                delta: IRDelta::TextDelta {
                    text: text.to_string(),
                },
            });
            state.text_emitted = true;
            state.saw_visible_text = true;
        }
    }

    if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for call in tool_calls {
            handle_tool_call_delta(call, state, &mut chunks);
        }
    }

    if let Some(finish_reason) = choice.get("finish_reason").and_then(Value::as_str) {
        close_active_block(state, &mut chunks);
        let stop_reason = stop_reason_from_finish_reason(Some(finish_reason));
        if data.get("usage").is_some_and(|usage| !usage.is_null()) {
            complete_stream(
                state,
                &mut chunks,
                stop_reason,
                parse_usage(data.get("usage")),
            );
        } else {
            state.extra.insert(
                PENDING_FINISH_REASON_KEY.to_string(),
                Value::String(finish_reason.to_string()),
            );
        }
    }

    Ok(chunks)
}

fn is_usage_only_chunk(data: &Value) -> bool {
    data.get("choices")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
        && data.get("usage").is_some_and(|usage| !usage.is_null())
}

fn pending_stop_reason(state: &mut StreamState) -> Option<IRStopReason> {
    state
        .extra
        .remove(PENDING_FINISH_REASON_KEY)
        .and_then(|value| value.as_str().map(|reason| reason.to_string()))
        .map(|reason| stop_reason_from_finish_reason(Some(&reason)))
}

fn complete_stream(
    state: &mut StreamState,
    chunks: &mut Vec<IRStreamChunk>,
    stop_reason: IRStopReason,
    usage: IRUsage,
) {
    chunks.push(IRStreamChunk::MessageDelta { stop_reason, usage });
    chunks.push(IRStreamChunk::MessageStop);
    state.stream_completed = true;
}

fn handle_tool_call_delta(call: &Value, state: &mut StreamState, chunks: &mut Vec<IRStreamChunk>) {
    let id = call
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| state.active_tool.as_ref().map(|tool| tool.id.clone()))
        .unwrap_or_default();
    let name = call
        .pointer("/function/name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| state.active_tool.as_ref().map(|tool| tool.name.clone()))
        .unwrap_or_default();

    if state.active_tool.is_none() {
        close_active_block(state, chunks);

        let index = state.block_index;
        state.block_index += 1;
        state.block_open = true;
        state.saw_tool_use = true;
        state.active_tool_arguments.clear();
        state.active_tool = Some(ActiveToolState {
            id: id.clone(),
            name: name.clone(),
        });
        chunks.push(IRStreamChunk::ContentBlockStart {
            index,
            block_type: IRBlockType::ToolUse { id, name },
        });
    }

    if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
        if !arguments.is_empty() {
            state.active_tool_arguments.push_str(arguments);
            chunks.push(IRStreamChunk::ContentBlockDelta {
                index: state.block_index.saturating_sub(1),
                delta: IRDelta::InputJsonDelta {
                    partial_json: arguments.to_string(),
                },
            });
        }
    }
}

fn open_text_block_if_needed(state: &mut StreamState, chunks: &mut Vec<IRStreamChunk>) {
    if state.block_open && state.active_tool.is_some() {
        close_active_block(state, chunks);
    }
    if state.block_open {
        return;
    }
    let index = state.block_index;
    state.block_index += 1;
    state.block_open = true;
    state.active_text.clear();
    chunks.push(IRStreamChunk::ContentBlockStart {
        index,
        block_type: IRBlockType::Text,
    });
}

fn close_active_block(state: &mut StreamState, chunks: &mut Vec<IRStreamChunk>) {
    if state.block_open {
        let block_type = if let Some(tool) = state.active_tool.as_ref() {
            Some(IRBlockType::ToolUse {
                id: tool.id.clone(),
                name: tool.name.clone(),
            })
        } else {
            Some(IRBlockType::Text)
        };
        let final_json = state
            .active_tool
            .is_some()
            .then(|| std::mem::take(&mut state.active_tool_arguments));
        let final_text = if final_json.is_none() {
            Some(std::mem::take(&mut state.active_text))
        } else {
            None
        };
        chunks.push(IRStreamChunk::ContentBlockStop {
            index: state.block_index.saturating_sub(1),
            block_type,
            final_text,
            final_json,
        });
        state.block_open = false;
    }
    state.active_tool = None;
}

fn stream_started(state: &StreamState) -> bool {
    state
        .extra
        .get("chat_stream_started")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn mark_stream_started(state: &mut StreamState) {
    state
        .extra
        .insert("chat_stream_started".to_string(), Value::Bool(true));
}
