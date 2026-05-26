//! Inbound adapter for the OpenAI Responses API protocol.
//!
//! This is the client-facing half needed when Codex sends Responses API
//! requests but the selected upstream only supports Chat Completions.

use super::super::ir::*;
use super::super::traits::*;
use axum::body::Bytes;
use serde_json::{json, Value};

pub(crate) struct OpenAIResponsesInbound;

impl Inbound for OpenAIResponsesInbound {
    fn protocol(&self) -> &'static str {
        "openai_responses"
    }

    fn request_to_ir(
        &self,
        body: Value,
        _ctx: &BridgeContext,
    ) -> Result<InternalRequest, BridgeError> {
        parse_request(body)
    }

    fn ir_to_response(
        &self,
        ir: &InternalResponse,
        ctx: &BridgeContext,
    ) -> Result<Value, BridgeError> {
        build_response(ir, ctx)
    }

    fn ir_chunk_to_sse(
        &self,
        chunk: &IRStreamChunk,
        ctx: &BridgeContext,
    ) -> Result<Vec<Bytes>, BridgeError> {
        render_chunk(chunk, ctx)
    }
}

fn parse_request(body: Value) -> Result<InternalRequest, BridgeError> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let system = parse_instructions(&body);
    let messages = parse_input(body.get("input"))?;
    let tools = parse_tools(&body);
    let tool_choice = parse_tool_choice(&body);
    let max_tokens = body
        .get("max_output_tokens")
        .or_else(|| body.get("max_tokens"))
        .and_then(Value::as_u64);
    let temperature = body.get("temperature").and_then(Value::as_f64);
    let top_p = body.get("top_p").and_then(Value::as_f64);
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let stop_sequences = parse_stop_sequences(body.get("stop"));

    Ok(InternalRequest {
        model,
        messages,
        system,
        tools,
        tool_choice,
        max_tokens,
        temperature,
        top_p,
        stop_sequences,
        stream,
        metadata: IRMetadata::default(),
    })
}

fn parse_instructions(body: &Value) -> Option<String> {
    match body.get("instructions") {
        Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
        Some(Value::Array(parts)) => {
            let joined = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n");
            (!joined.is_empty()).then_some(joined)
        }
        _ => None,
    }
}

fn parse_input(input: Option<&Value>) -> Result<Vec<IRMessage>, BridgeError> {
    match input {
        Some(Value::String(text)) => Ok(vec![IRMessage {
            role: IRRole::User,
            content: vec![IRContentBlock::Text { text: text.clone() }],
        }]),
        Some(Value::Array(items)) => {
            let mut messages = Vec::new();
            for item in items {
                if let Some(message) = parse_input_item(item)? {
                    messages.push(message);
                }
            }
            Ok(messages)
        }
        None => Ok(Vec::new()),
        _ => Err(BridgeError::TransformFailed(
            "responses input must be a string or array".into(),
        )),
    }
}

fn parse_input_item(item: &Value) -> Result<Option<IRMessage>, BridgeError> {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => {
            let id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let input = serde_json::from_str(arguments).unwrap_or_else(|_| json!(arguments));
            Ok(Some(IRMessage {
                role: IRRole::Assistant,
                content: vec![IRContentBlock::ToolUse { id, name, input }],
            }))
        }
        Some("function_call_output") => {
            let tool_use_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let content = item
                .get("output")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| item.get("output").map(Value::to_string).unwrap_or_default());
            Ok(Some(IRMessage {
                role: IRRole::User,
                content: vec![IRContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error: false,
                }],
            }))
        }
        Some("message") | None => parse_message_item(item).map(Some),
        Some(_) => Ok(None),
    }
}

fn parse_message_item(item: &Value) -> Result<IRMessage, BridgeError> {
    let role = match item.get("role").and_then(Value::as_str).unwrap_or("user") {
        "assistant" => IRRole::Assistant,
        _ => IRRole::User,
    };
    let content = parse_content(item.get("content"))?;
    Ok(IRMessage { role, content })
}

fn parse_content(content: Option<&Value>) -> Result<Vec<IRContentBlock>, BridgeError> {
    match content {
        Some(Value::String(text)) => Ok(vec![IRContentBlock::Text { text: text.clone() }]),
        Some(Value::Array(parts)) => {
            let mut blocks = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str).unwrap_or("") {
                    "input_text" | "output_text" | "text" => {
                        let text = part
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        blocks.push(IRContentBlock::Text { text });
                    }
                    "input_image" => {
                        if let Some((media_type, data)) = parse_data_url(
                            part.get("image_url").and_then(Value::as_str).unwrap_or(""),
                        ) {
                            blocks.push(IRContentBlock::Image { media_type, data });
                        }
                    }
                    _ => {}
                }
            }
            Ok(blocks)
        }
        _ => Ok(Vec::new()),
    }
}

fn parse_data_url(value: &str) -> Option<(String, String)> {
    let rest = value.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    Some((media_type.to_string(), data.to_string()))
}

fn parse_tools(body: &Value) -> Vec<IRToolDefinition> {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return Vec::new();
    };

    tools
        .iter()
        .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("function"))
        .map(|tool| {
            let function = tool.get("function").unwrap_or(tool);
            IRToolDefinition {
                name: function
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                description: function
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                parameters: function.get("parameters").cloned().unwrap_or(json!({})),
            }
        })
        .collect()
}

fn parse_tool_choice(body: &Value) -> Option<IRToolChoice> {
    match body.get("tool_choice")? {
        Value::String(value) => match value.as_str() {
            "auto" => Some(IRToolChoice::Auto),
            "required" => Some(IRToolChoice::Required),
            "none" => Some(IRToolChoice::None),
            _ => None,
        },
        Value::Object(obj) => {
            if obj.get("type").and_then(Value::as_str) != Some("function") {
                return None;
            }
            let name = obj
                .get("name")
                .or_else(|| {
                    obj.get("function")
                        .and_then(|function| function.get("name"))
                })
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some(IRToolChoice::Specific { name })
        }
        _ => None,
    }
}

fn parse_stop_sequences(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn build_response(ir: &InternalResponse, ctx: &BridgeContext) -> Result<Value, BridgeError> {
    let model = ctx
        .requested_model
        .as_deref()
        .filter(|m| !m.is_empty())
        .unwrap_or(&ir.model);
    let status = match ir.stop_reason {
        IRStopReason::MaxTokens => "incomplete",
        _ => "completed",
    };

    let mut output = Vec::new();
    let mut message_content = Vec::new();
    for block in &ir.content {
        match block {
            IRContentBlock::Text { text } => {
                message_content.push(json!({"type": "output_text", "text": text}));
            }
            IRContentBlock::ToolUse { id, name, input } => {
                if !message_content.is_empty() {
                    output.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": std::mem::take(&mut message_content)
                    }));
                }
                output.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": serde_json::to_string(input).unwrap_or_default()
                }));
            }
            _ => {}
        }
    }
    if !message_content.is_empty() {
        output.push(json!({
            "type": "message",
            "role": "assistant",
            "content": message_content
        }));
    }

    Ok(json!({
        "id": ir.id,
        "object": "response",
        "status": status,
        "model": model,
        "output": output,
        "usage": {
            "input_tokens": ir.usage.input_tokens,
            "output_tokens": ir.usage.output_tokens
        }
    }))
}

fn sse_frame(event_type: &str, payload: Value) -> Bytes {
    let data = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    Bytes::from(format!("event: {event_type}\ndata: {data}\n\n"))
}

fn render_chunk(chunk: &IRStreamChunk, ctx: &BridgeContext) -> Result<Vec<Bytes>, BridgeError> {
    let frames = match chunk {
        IRStreamChunk::MessageStart { id, model, .. } => {
            let model = ctx
                .requested_model
                .as_deref()
                .filter(|m| !m.is_empty())
                .unwrap_or(model);
            vec![sse_frame(
                "response.created",
                json!({
                    "type": "response.created",
                    "response": {
                        "id": id,
                        "object": "response",
                        "status": "in_progress",
                        "model": model,
                        "output": []
                    }
                }),
            )]
        }
        IRStreamChunk::ContentBlockStart { index, block_type } => match block_type {
            IRBlockType::Text => {
                let item_id = format!("msg_{index}");
                vec![
                    sse_frame(
                        "response.output_item.added",
                        json!({
                            "type": "response.output_item.added",
                            "output_index": index,
                            "item": {
                                "id": item_id,
                                "type": "message",
                                "status": "in_progress",
                                "role": "assistant",
                                "content": []
                            }
                        }),
                    ),
                    sse_frame(
                        "response.content_part.added",
                        json!({
                            "type": "response.content_part.added",
                            "item_id": item_id,
                            "output_index": index,
                            "content_index": 0,
                            "part": {"type": "output_text", "text": ""}
                        }),
                    ),
                ]
            }
            IRBlockType::ToolUse { id, name } => {
                vec![sse_frame(
                    "response.output_item.added",
                    json!({
                        "type": "response.output_item.added",
                        "output_index": index,
                        "item": {
                            "id": id,
                            "type": "function_call",
                            "status": "in_progress",
                            "call_id": id,
                            "name": name,
                            "arguments": ""
                        }
                    }),
                )]
            }
            IRBlockType::Thinking => Vec::new(),
        },
        IRStreamChunk::ContentBlockDelta { index, delta } => match delta {
            IRDelta::TextDelta { text } => vec![sse_frame(
                "response.output_text.delta",
                json!({
                    "type": "response.output_text.delta",
                    "item_id": format!("msg_{index}"),
                    "output_index": index,
                    "content_index": 0,
                    "delta": text
                }),
            )],
            IRDelta::InputJsonDelta { partial_json } => vec![sse_frame(
                "response.function_call_arguments.delta",
                json!({
                    "type": "response.function_call_arguments.delta",
                    "output_index": index,
                    "delta": partial_json
                }),
            )],
            IRDelta::ThinkingDelta { .. } => Vec::new(),
        },
        IRStreamChunk::ContentBlockStop {
            index,
            block_type,
            final_text,
            final_json,
        } => {
            let mut frames = Vec::new();
            if let Some(arguments) = final_json {
                let (id, name) = match block_type {
                    Some(IRBlockType::ToolUse { id, name }) => (id.clone(), name.clone()),
                    _ => (format!("call_{index}"), String::new()),
                };
                frames.push(sse_frame(
                    "response.function_call_arguments.done",
                    json!({
                        "type": "response.function_call_arguments.done",
                        "output_index": index,
                        "arguments": arguments
                    }),
                ));
                frames.push(sse_frame(
                    "response.output_item.done",
                    json!({
                        "type": "response.output_item.done",
                        "output_index": index,
                        "item": {
                            "id": id,
                            "type": "function_call",
                            "status": "completed",
                            "call_id": id,
                            "name": name,
                            "arguments": arguments
                        }
                    }),
                ));
            } else {
                let text = final_text.as_deref().unwrap_or("");
                let item_id = format!("msg_{index}");
                frames.extend([
                    sse_frame(
                        "response.output_text.done",
                        json!({
                            "type": "response.output_text.done",
                            "item_id": item_id,
                            "output_index": index,
                            "content_index": 0,
                            "text": text
                        }),
                    ),
                    sse_frame(
                        "response.content_part.done",
                        json!({
                            "type": "response.content_part.done",
                            "item_id": item_id,
                            "output_index": index,
                            "content_index": 0,
                            "part": {"type": "output_text", "text": text}
                        }),
                    ),
                    sse_frame(
                        "response.output_item.done",
                        json!({
                            "type": "response.output_item.done",
                            "output_index": index,
                            "item": {
                                "id": item_id,
                                "type": "message",
                                "status": "completed",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": text}]
                            }
                        }),
                    ),
                ]);
            }
            frames
        }
        IRStreamChunk::MessageDelta { .. } => Vec::new(),
        IRStreamChunk::MessageStop => vec![sse_frame(
            "response.completed",
            json!({
                "type": "response.completed",
                "response": {
                    "id": "",
                    "object": "response",
                    "status": "completed",
                    "model": ctx.requested_model.as_deref().unwrap_or(""),
                    "output": []
                }
            }),
        )],
        IRStreamChunk::Ping => Vec::new(),
    };

    Ok(frames)
}
