//! Usage: Provider traversal + skip logic (gate, credential, OAuth, CX2CC, Gemini, Codex).
//!
//! Encapsulates all per-provider preparation that runs before the retry loop.

use super::provider_checks;
use super::*;
use crate::gateway::events::ClaudeModelMapping;
use crate::gateway::proxy::gemini_oauth::GeminiOAuthResponseMode;
use std::collections::HashSet;

fn apply_chatgpt_compat_and_record(
    provider_id: i64,
    special_settings: &std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    forwarded_path: &mut String,
    upstream_body_bytes: &mut Bytes,
    strip_request_content_encoding: &mut bool,
) {
    let outcome = maybe_apply_codex_chatgpt_request_compat(
        forwarded_path,
        upstream_body_bytes,
        strip_request_content_encoding,
    );
    let Some(setting) = codex_chatgpt::foreign_history_handoff_special_setting(outcome) else {
        return;
    };

    crate::gateway::response_fixer::push_special_setting(special_settings, setting);
    tracing::info!(
        provider_id,
        reasoning_items_removed = outcome.reasoning_items_removed,
        previous_response_id_removed = outcome.previous_response_id_removed,
        "normalized foreign Responses history for ChatGPT handoff"
    );
}

/// All mutable state accumulated by the provider preparation phase that the
/// retry loop (and later finalization) needs.
pub(super) struct PreparedProvider {
    pub(super) provider_id: i64,
    pub(super) provider_name_base: String,
    pub(super) provider_base_url_base: String,
    pub(super) provider_base_url_display: String,
    pub(super) auth_mode: String,
    pub(super) provider_index: u32,
    // Bridged (cx2cc) input semantics for this provider; threaded into
    // FailoverAttempt so the request event can compute effective_input_tokens.
    pub(super) provider_bridged: bool,
    pub(super) session_reuse: Option<bool>,
    pub(super) effective_credential: String,
    pub(super) provider_regular_max_attempts: u32,
    pub(super) provider_max_attempts: u32,
    pub(super) oauth_adapter:
        Option<&'static dyn crate::gateway::oauth::provider_trait::OAuthProvider>,
    pub(super) upstream_forwarded_path: String,
    pub(super) upstream_query: Option<String>,
    pub(super) upstream_body_bytes: Bytes,
    pub(super) strip_request_content_encoding: bool,
    pub(super) request_body_mutated_before_attempt: bool,
    pub(super) gemini_oauth_response_mode: Option<GeminiOAuthResponseMode>,
    pub(super) use_codex_chatgpt_backend: bool,
    pub(super) codex_chatgpt_account_id: Option<String>,
    pub(super) cx2cc_active: bool,
    pub(super) protocol_bridge_type: Option<String>,
    pub(super) cx2cc_source: Option<(crate::providers::ProviderForGateway, String)>,
    pub(super) cx2cc_codex_session_id: Option<String>,
    pub(super) circuit_snapshot: crate::circuit_breaker::CircuitSnapshot,
    pub(super) anthropic_stream_requested: bool,
    pub(super) stream_idle_timeout_seconds: Option<u32>,
    pub(super) claude_model_mapping: Option<ClaudeModelMapping>,
}

/// Counters accumulated across all providers in the iteration loop.
pub(super) struct IterationCounters {
    pub(super) providers_tried: usize,
    pub(super) earliest_available_unix: Option<i64>,
    pub(super) skipped_open: usize,
    pub(super) skipped_cooldown: usize,
    pub(super) skipped_limits: usize,
}

impl IterationCounters {
    pub(super) fn new() -> Self {
        Self {
            providers_tried: 0,
            earliest_available_unix: None,
            skipped_open: 0,
            skipped_cooldown: 0,
            skipped_limits: 0,
        }
    }
}

pub(super) enum PreparationOutcome {
    Ready(Box<PreparedProvider>),
    Skipped,
}

/// Structured skip reason used by CX2CC preparation and other skip paths.
pub(super) struct SkipReason {
    pub(super) error_category: &'static str,
    pub(super) error_code: &'static str,
    pub(super) reason: String,
}

/// Prepare a single provider for the retry loop.
pub(super) async fn prepare_provider<R: tauri::Runtime>(
    ctx: CommonCtx<'_, R>,
    input: &RequestContext<R>,
    provider: &crate::providers::ProviderForGateway,
    counters: &mut IterationCounters,
    attempts: &mut Vec<FailoverAttempt>,
    failed_provider_ids: &HashSet<i64>,
    anthropic_stream_requested: bool,
) -> PreparationOutcome {
    let provider_id = provider.id;
    let provider_name_base = if provider.name.trim().is_empty() {
        format!("Provider #{} (auto-fixed)", provider.id)
    } else {
        provider.name.clone()
    };
    let provider_base_url_display = provider
        .base_urls
        .first()
        .cloned()
        .unwrap_or_else(String::new);

    if failed_provider_ids.contains(&provider_id) {
        return PreparationOutcome::Skipped;
    }

    let identity = provider_checks::ProviderIdentity {
        provider_id,
        provider_name_base: &provider_name_base,
        provider_base_url_display: &provider_base_url_display,
    };
    let gate_allow =
        match provider_checks::run_gates(ctx, input, provider, &identity, counters, attempts) {
            Some(allow) => allow,
            None => return PreparationOutcome::Skipped,
        };

    let is_cx2cc_bridge = provider.is_cx2cc_bridge();

    let mut effective_credential = if is_cx2cc_bridge {
        String::new()
    } else {
        match resolve_effective_credential(&input.state, &input.cli_key, provider).await {
            Ok(value) => value,
            Err(err) => {
                provider_checks::skip_with_reason(
                    attempts,
                    provider_id,
                    &provider_name_base,
                    &provider_base_url_display,
                    input.started.elapsed().as_millis(),
                    SkipReason {
                        error_category: "auth",
                        error_code: GatewayErrorCode::InternalError.as_str(),
                        reason: format!("provider skipped by credential resolution: {err}"),
                    },
                );
                return PreparationOutcome::Skipped;
            }
        }
    };

    let needs_codex_reasoning_context_retry = codex_body_has_reasoning_context(
        &input.cli_key,
        &input.forwarded_path,
        input.body_bytes.as_ref(),
    );
    let needs_codex_additional_tools_retry = codex_body_has_additional_tools(
        &input.cli_key,
        &input.forwarded_path,
        input.body_bytes.as_ref(),
    );
    let needs_codex_agent_message_retry = codex_body_has_agent_message(
        &input.cli_key,
        &input.forwarded_path,
        input.body_bytes.as_ref(),
    );
    let provider_regular_max_attempts = provider_regular_max_attempts_for_request(
        input.max_attempts_per_provider,
        gate_allow.circuit_after.failure_threshold,
        provider.auth_mode == "oauth",
        codex_request_has_previous_response_id(input),
        input.is_codex_model_discovery,
    );
    let provider_max_attempts = provider_total_max_attempts_for_request(
        provider_regular_max_attempts,
        needs_codex_reasoning_context_retry,
        needs_codex_additional_tools_retry,
        needs_codex_agent_message_retry,
        input.is_codex_model_discovery,
    );

    let mut provider_base_url_base = match provider_checks::resolve_base_url(
        input,
        provider,
        provider_id,
        &provider_name_base,
        &provider_base_url_display,
        attempts,
    )
    .await
    {
        Some(url) => url,
        None => return PreparationOutcome::Skipped,
    };

    let mut use_codex_chatgpt_backend =
        is_codex_chatgpt_backend(&input.cli_key, provider, &provider_base_url_base);
    let mut codex_chatgpt_account_id = if use_codex_chatgpt_backend {
        provider_checks::extract_codex_chatgpt_account_id(&input.state.db, provider.id)
    } else {
        None
    };

    let oauth_adapter = match provider_checks::resolve_oauth(
        input,
        provider,
        provider_id,
        &provider_name_base,
        &provider_base_url_display,
        attempts,
    ) {
        Some(adapter) => adapter,
        None => return PreparationOutcome::Skipped,
    };

    let mut upstream_forwarded_path = input.forwarded_path.clone();
    let mut upstream_query = input.query.clone();
    let mut upstream_body_bytes = input.request_body_state.decoded_clone();
    let mut strip_request_content_encoding = input.strip_request_content_encoding_seed;
    let mut gemini_oauth_response_mode = None;
    let mut protocol_bridge_type: Option<String> = None;

    if let Some(adapter) = &oauth_adapter {
        if adapter.provider_type() == "gemini_oauth" {
            match provider_checks::prepare_gemini_oauth(
                input,
                &effective_credential,
                &mut provider_base_url_base,
            )
            .await
            {
                Some(prepared) => {
                    upstream_forwarded_path = prepared.forwarded_path;
                    upstream_query = prepared.query;
                    upstream_body_bytes = prepared.body_bytes;
                    strip_request_content_encoding = prepared.strip_request_content_encoding;
                    gemini_oauth_response_mode = Some(prepared.response_mode);
                }
                None => {
                    provider_checks::skip_with_reason(
                        attempts,
                        provider_id,
                        &provider_name_base,
                        &provider_base_url_display,
                        input.started.elapsed().as_millis(),
                        SkipReason {
                            error_category: "auth",
                            error_code: GatewayErrorCode::InternalError.as_str(),
                            reason: "provider skipped by gemini oauth translation".into(),
                        },
                    );
                    return PreparationOutcome::Skipped;
                }
            }
        }
    }

    // --- CX2CC translation ---
    let mut cx2cc_active = false;
    let mut cx2cc_source: Option<(crate::providers::ProviderForGateway, String)> = None;
    let mut cx2cc_codex_session_id: Option<String> = None;
    if is_cx2cc_bridge {
        let outcome = cx2cc_preparation::prepare(cx2cc_preparation::Cx2ccPreparationInput {
            ctx,
            input,
            provider_id,
            provider_name_base: &provider_name_base,
            source_id: provider.source_provider_id,
            anthropic_stream_requested,
            upstream_body_bytes,
            use_codex_chatgpt_backend,
            codex_chatgpt_account_id,
        })
        .await;
        match outcome {
            cx2cc_preparation::Cx2ccOutcome::Ready(boxed) => {
                let result = *boxed;
                cx2cc_active = result.cx2cc_active;
                protocol_bridge_type = Some(crate::providers::CX2CC_BRIDGE_TYPE.to_string());
                cx2cc_source = result.cx2cc_source;
                cx2cc_codex_session_id = result.cx2cc_codex_session_id;
                effective_credential = result.effective_credential;
                provider_base_url_base = result.provider_base_url_base;
                upstream_forwarded_path = result.upstream_forwarded_path;
                upstream_query = result.upstream_query;
                upstream_body_bytes = result.upstream_body_bytes;
                strip_request_content_encoding = result.strip_request_content_encoding;
                use_codex_chatgpt_backend = result.use_codex_chatgpt_backend;
                codex_chatgpt_account_id = result.codex_chatgpt_account_id;
            }
            cx2cc_preparation::Cx2ccOutcome::Skipped(reason) => {
                provider_checks::skip_with_reason(
                    attempts,
                    provider_id,
                    &provider_name_base,
                    &provider_base_url_display,
                    input.started.elapsed().as_millis(),
                    reason,
                );
                return PreparationOutcome::Skipped;
            }
        }
    }

    let mut direct_bridge_applied = false;
    if crate::providers::is_r2c_bridge(provider.bridge_type.as_deref())
        && is_responses_request_path(&upstream_forwarded_path)
    {
        match translate_direct_bridge_request(
            crate::providers::R2C_BRIDGE_TYPE,
            &upstream_body_bytes,
            input.requested_model.as_deref(),
            anthropic_stream_requested,
            &input.cx2cc_settings,
            &crate::providers::ClaudeModels::default(),
            &provider.model_mapping,
        ) {
            Ok(translated) => {
                protocol_bridge_type = Some(crate::providers::R2C_BRIDGE_TYPE.to_string());
                upstream_forwarded_path = translated.forwarded_path;
                upstream_query = None;
                upstream_body_bytes = translated.body_bytes;
                strip_request_content_encoding = true;
                direct_bridge_applied = true;
            }
            Err(err) => {
                provider_checks::skip_with_reason(
                    attempts,
                    provider_id,
                    &provider_name_base,
                    &provider_base_url_display,
                    input.started.elapsed().as_millis(),
                    SkipReason {
                        error_category: "translation",
                        error_code: GatewayErrorCode::InternalError.as_str(),
                        reason: format!("r2c translation failed: {err}"),
                    },
                );
                return PreparationOutcome::Skipped;
            }
        }
    }

    if provider.bridge_type.as_deref()
        == Some(crate::providers::CLAUDE_CHAT_COMPLETIONS_BRIDGE_TYPE)
        && is_anthropic_messages_request_path(&upstream_forwarded_path)
    {
        match translate_direct_bridge_request(
            crate::providers::CLAUDE_CHAT_COMPLETIONS_BRIDGE_TYPE,
            &upstream_body_bytes,
            input.requested_model.as_deref(),
            anthropic_stream_requested,
            &input.cx2cc_settings,
            &provider.claude_models,
            &provider.model_mapping,
        ) {
            Ok(translated) => {
                protocol_bridge_type =
                    Some(crate::providers::CLAUDE_CHAT_COMPLETIONS_BRIDGE_TYPE.to_string());
                upstream_forwarded_path = translated.forwarded_path;
                upstream_query = None;
                upstream_body_bytes = translated.body_bytes;
                strip_request_content_encoding = true;
                direct_bridge_applied = true;
            }
            Err(err) => {
                provider_checks::skip_with_reason(
                    attempts,
                    provider_id,
                    &provider_name_base,
                    &provider_base_url_display,
                    input.started.elapsed().as_millis(),
                    SkipReason {
                        error_category: "translation",
                        error_code: GatewayErrorCode::InternalError.as_str(),
                        reason: format!("claude_chat_completions translation failed: {err}"),
                    },
                );
                return PreparationOutcome::Skipped;
            }
        }
    }

    if !direct_bridge_applied
        && !cx2cc_active
        && provider.auth_mode == crate::providers::ProviderAuthMode::ApiKey.as_str()
        && apply_codex_api_key_model_mapping(&mut upstream_body_bytes, &provider.model_mapping)
            .is_some()
    {
        strip_request_content_encoding = true;
    }

    let circuit_snapshot = gate_allow.circuit_after;
    counters.providers_tried = counters.providers_tried.saturating_add(1);
    let provider_index = counters.providers_tried as u32;
    let session_reuse = match input.session_bound_provider_id {
        Some(id) => (id == provider_id && provider_index == 1).then_some(true),
        None => None,
    };
    let provider_ctx = ProviderCtx {
        provider_id,
        provider_name_base: &provider_name_base,
        provider_base_url_base: &provider_base_url_base,
        auth_mode: provider.auth_mode.as_str(),
        provider_index,
        provider_bridged: is_cx2cc_bridge,
        session_reuse,
        stream_idle_timeout_seconds: provider.stream_idle_timeout_seconds,
        claude_model_mapping: None,
    };

    let mut claude_model_mapping = None;
    if !direct_bridge_applied
        && should_apply_claude_model_mapping(cx2cc_active, &upstream_forwarded_path)
    {
        claude_model_mapping = claude_model_mapping::apply_if_needed(
            ctx,
            provider,
            provider_ctx,
            input.requested_model_location,
            input.introspection_json.as_ref(),
            claude_model_mapping::UpstreamRequestMut {
                forwarded_path: &mut upstream_forwarded_path,
                query: &mut upstream_query,
                body_bytes: &mut upstream_body_bytes,
                strip_request_content_encoding: &mut strip_request_content_encoding,
            },
        );
    }

    claude_metadata_user_id_injection::apply_if_needed(
        claude_metadata_user_id_injection::ApplyClaudeMetadataUserIdInjectionInput {
            ctx,
            provider_id,
            enabled: input.enable_claude_metadata_user_id_injection,
            session_id: input.session_id.as_deref(),
            base_headers: &input.base_headers,
            forwarded_path: upstream_forwarded_path.as_str(),
            upstream_body_bytes: &mut upstream_body_bytes,
            strip_request_content_encoding: &mut strip_request_content_encoding,
        },
    );

    let body_len_before_compat = upstream_body_bytes.len();
    if use_codex_chatgpt_backend {
        tracing::info!(
            provider_id,
            forwarded_path = %upstream_forwarded_path,
            body_len = body_len_before_compat,
            "provider_iterator: entering chatgpt_backend compat path"
        );
        apply_chatgpt_compat_and_record(
            provider_id,
            ctx.special_settings,
            &mut upstream_forwarded_path,
            &mut upstream_body_bytes,
            &mut strip_request_content_encoding,
        );
    }
    if upstream_body_bytes.len() != body_len_before_compat {
        tracing::info!(
            provider_id,
            before = body_len_before_compat,
            after = upstream_body_bytes.len(),
            "provider_iterator: body size changed after chatgpt compatibility normalization"
        );
    }

    let request_body_mutated_before_attempt = input.request_body_state.is_mutated()
        || upstream_body_bytes != input.request_body_state.decoded_clone()
        || strip_request_content_encoding;

    PreparationOutcome::Ready(Box::new(PreparedProvider {
        provider_id,
        provider_name_base,
        provider_base_url_base,
        provider_base_url_display,
        auth_mode: provider.auth_mode.clone(),
        provider_index,
        provider_bridged: is_cx2cc_bridge,
        session_reuse,
        effective_credential,
        provider_regular_max_attempts,
        provider_max_attempts,
        oauth_adapter,
        upstream_forwarded_path,
        upstream_query,
        upstream_body_bytes,
        strip_request_content_encoding,
        request_body_mutated_before_attempt,
        gemini_oauth_response_mode,
        use_codex_chatgpt_backend,
        codex_chatgpt_account_id,
        cx2cc_active,
        protocol_bridge_type,
        cx2cc_source,
        cx2cc_codex_session_id,
        circuit_snapshot,
        anthropic_stream_requested,
        stream_idle_timeout_seconds: provider.stream_idle_timeout_seconds,
        claude_model_mapping,
    }))
}

fn codex_request_has_previous_response_id<R: tauri::Runtime>(input: &RequestContext<R>) -> bool {
    codex_body_has_previous_response_id(&input.cli_key, &input.body_bytes)
}

fn codex_body_has_previous_response_id(cli_key: &str, body: &[u8]) -> bool {
    // grok 与 codex 同走 OpenAI Responses API，rectifier 重试额度同样适用。
    if !matches!(cli_key, "codex" | "grok") {
        return false;
    }

    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|root| {
            root.get("previous_response_id")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .map(str::to_string)
        })
        .is_some_and(|value| !value.is_empty())
}

fn codex_body_has_reasoning_context(cli_key: &str, forwarded_path: &str, body: &[u8]) -> bool {
    if !matches!(cli_key, "codex" | "grok") || !is_responses_request_path(forwarded_path) {
        return false;
    }

    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|root| root.get("reasoning")?.as_object().cloned())
        .is_some_and(|reasoning| reasoning.contains_key("context"))
}

fn codex_body_has_additional_tools(cli_key: &str, forwarded_path: &str, body: &[u8]) -> bool {
    if !matches!(cli_key, "codex" | "grok") || !is_responses_request_path(forwarded_path) {
        return false;
    }

    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|root| root.get("input")?.as_array().cloned())
        .is_some_and(|input| {
            input.iter().any(|item| {
                item.get("type").and_then(serde_json::Value::as_str) == Some("additional_tools")
            })
        })
}

fn codex_body_has_agent_message(cli_key: &str, forwarded_path: &str, body: &[u8]) -> bool {
    if !matches!(cli_key, "codex" | "grok") || !is_responses_request_path(forwarded_path) {
        return false;
    }

    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|root| root.get("input")?.as_array().cloned())
        .is_some_and(|input| {
            input.iter().any(|item| {
                item.get("type").and_then(serde_json::Value::as_str) == Some("agent_message")
            })
        })
}

fn provider_regular_max_attempts_for_request(
    configured_max_attempts: u32,
    circuit_failure_threshold: u32,
    needs_oauth_reactive_refresh_retry: bool,
    needs_codex_previous_response_id_retry: bool,
    strict_configured_limit: bool,
) -> u32 {
    if strict_configured_limit {
        return configured_max_attempts.max(1);
    }

    let required_internal_retries = u32::from(needs_oauth_reactive_refresh_retry)
        + u32::from(needs_codex_previous_response_id_retry);
    configured_max_attempts
        .max(circuit_failure_threshold.max(1))
        .max(1 + required_internal_retries)
}

fn provider_total_max_attempts_for_request(
    provider_regular_max_attempts: u32,
    needs_codex_reasoning_context_retry: bool,
    needs_codex_additional_tools_retry: bool,
    needs_codex_agent_message_retry: bool,
    strict_configured_limit: bool,
) -> u32 {
    if strict_configured_limit {
        provider_regular_max_attempts
    } else {
        provider_regular_max_attempts.saturating_add(
            u32::from(needs_codex_reasoning_context_retry)
                + u32::from(needs_codex_additional_tools_retry)
                + u32::from(needs_codex_agent_message_retry),
        )
    }
}

pub(super) fn is_responses_request_path(path: &str) -> bool {
    matches!(path.trim_end_matches('/'), "/v1/responses" | "/responses")
}

fn is_anthropic_messages_request_path(path: &str) -> bool {
    matches!(path.trim_end_matches('/'), "/v1/messages" | "/messages")
}

struct DirectBridgeTranslation {
    forwarded_path: String,
    body_bytes: Bytes,
}

fn translate_direct_bridge_request(
    bridge_type: &str,
    body_bytes: &Bytes,
    requested_model: Option<&str>,
    stream_requested: bool,
    cx2cc_settings: &crate::gateway::proxy::cx2cc::settings::Cx2ccSettings,
    claude_models: &crate::providers::ClaudeModels,
    model_mapping: &crate::providers::ProviderModelMapping,
) -> Result<DirectBridgeTranslation, String> {
    let body_val: serde_json::Value =
        serde_json::from_slice(body_bytes.as_ref()).map_err(|err| err.to_string())?;
    let bridge = crate::gateway::proxy::protocol_bridge::get_bridge(bridge_type)
        .ok_or_else(|| format!("{bridge_type} bridge not registered"))?;
    let bridge_ctx = crate::gateway::proxy::protocol_bridge::BridgeContext {
        claude_models: claude_models.clone(),
        model_mapping: model_mapping.clone(),
        cx2cc_settings: cx2cc_settings.clone(),
        requested_model: requested_model.filter(|m| !m.is_empty()).map(String::from),
        mapped_model: None,
        stream_requested,
        is_chatgpt_backend: false,
    };
    let translated = bridge
        .translate_request(body_val, &bridge_ctx)
        .map_err(|err| err.to_string())?;
    let body_bytes = serde_json::to_vec(&translated.body)
        .map(Bytes::from)
        .map_err(|err| err.to_string())?;

    Ok(DirectBridgeTranslation {
        forwarded_path: translated.target_path,
        body_bytes,
    })
}

fn apply_codex_api_key_model_mapping(
    body_bytes: &mut Bytes,
    model_mapping: &crate::providers::ProviderModelMapping,
) -> Option<(String, String)> {
    if model_mapping.is_empty() {
        return None;
    }

    let mut body_val: serde_json::Value = serde_json::from_slice(body_bytes.as_ref()).ok()?;
    let requested_model = body_val.get("model")?.as_str()?.to_string();
    let mapped_model = crate::providers::map_provider_model(model_mapping, &requested_model);
    if mapped_model == requested_model {
        return None;
    }

    body_val["model"] = serde_json::Value::String(mapped_model.clone());
    *body_bytes = Bytes::from(serde_json::to_vec(&body_val).ok()?);
    Some((requested_model, mapped_model))
}

#[cfg(test)]
mod tests {
    use super::{
        apply_chatgpt_compat_and_record, apply_codex_api_key_model_mapping,
        codex_body_has_additional_tools, codex_body_has_agent_message,
        codex_body_has_previous_response_id, codex_body_has_reasoning_context,
        is_anthropic_messages_request_path, is_responses_request_path,
        provider_regular_max_attempts_for_request, provider_total_max_attempts_for_request,
        translate_direct_bridge_request,
    };
    use axum::body::Bytes;
    use std::sync::{Arc, Mutex};

    #[test]
    fn chatgpt_preparation_records_foreign_history_handoff_without_mutating_shared_body() {
        let shared = Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "gpt-5",
                "instructions": "keep instructions",
                "input": [
                    {"role": "user", "content": [
                        {"type": "input_text", "text": "marker"},
                        {"type": "input_image", "image_url": "data:image/png;base64,abc"}
                    ]},
                    {"role": "assistant", "content": [{"type": "output_text", "text": "prior answer"}]},
                    {"type": "reasoning", "content": [{"type": "reasoning_text", "text": "foreign reasoning"}]},
                    {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{}"},
                    {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
                ],
                "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}],
                "include": ["reasoning.encrypted_content"],
                "previous_response_id": "resp_longcat",
                "stream": false,
                "store": true,
                "unsupported": "drop me"
            }))
            .unwrap(),
        );
        let original = shared.clone();
        let mut outbound = shared.clone();
        let mut path = "/v1/responses".to_string();
        let mut strip_content_encoding = false;
        let special_settings = Arc::new(Mutex::new(Vec::new()));

        apply_chatgpt_compat_and_record(
            12,
            &special_settings,
            &mut path,
            &mut outbound,
            &mut strip_content_encoding,
        );

        assert_eq!(
            shared, original,
            "shared request body must remain byte-identical"
        );
        assert_eq!(path, "/responses");
        assert!(strip_content_encoding);
        let body: serde_json::Value = serde_json::from_slice(&outbound).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["content"][0]["text"], "marker");
        assert_eq!(input[0]["content"][1]["type"], "input_image");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["text"], "prior answer");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(body["instructions"], "keep instructions");
        assert_eq!(body["tools"][0]["name"], "lookup");
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get("unsupported").is_none());
        assert!(!outbound
            .windows(b"foreign_history_handoff".len())
            .any(|window| window == b"foreign_history_handoff"));
        assert_eq!(
            *special_settings.lock().unwrap(),
            vec![serde_json::json!({
                "type": "foreign_history_handoff",
                "trigger": "plaintext_reasoning_content",
                "reasoning_items_removed": 1,
                "previous_response_id_removed": true
            })]
        );
    }

    fn body(value: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&value).expect("serialize body")
    }

    #[test]
    fn codex_request_has_previous_response_id_detects_codex_continuation() {
        let body = body(serde_json::json!({
            "previous_response_id": "resp_123"
        }));

        assert!(codex_body_has_previous_response_id("codex", &body));
        assert!(codex_body_has_previous_response_id("grok", &body));
    }

    #[test]
    fn codex_request_has_previous_response_id_ignores_other_cli_or_missing_value() {
        let with_previous = body(serde_json::json!({
            "previous_response_id": "resp_123"
        }));
        let without_previous = body(serde_json::json!({}));

        assert!(!codex_body_has_previous_response_id(
            "claude",
            &with_previous
        ));
        assert!(!codex_body_has_previous_response_id(
            "codex",
            &without_previous
        ));
        assert!(!codex_body_has_previous_response_id(
            "grok",
            &without_previous
        ));
    }

    #[test]
    fn codex_body_reasoning_context_requires_internal_retry_budget() {
        let body = body(serde_json::json!({
            "reasoning": {"effort": "low", "context": "all_turns"}
        }));

        assert!(codex_body_has_reasoning_context(
            "codex",
            "/v1/responses",
            &body
        ));
        assert!(codex_body_has_reasoning_context(
            "grok",
            "/responses/",
            &body
        ));
        assert!(!codex_body_has_reasoning_context(
            "codex",
            "/v1/models",
            &body
        ));
        assert!(!codex_body_has_reasoning_context(
            "claude",
            "/v1/responses",
            &body
        ));
    }

    #[test]
    fn codex_body_reasoning_context_rejects_incompatible_shapes() {
        for body in [
            b"not-json".to_vec(),
            body(serde_json::json!({})),
            body(serde_json::json!({"reasoning": null})),
            body(serde_json::json!({"reasoning": "all_turns"})),
            body(serde_json::json!({"reasoning": {"effort": "low"}})),
        ] {
            assert!(!codex_body_has_reasoning_context(
                "codex",
                "/v1/responses",
                &body
            ));
        }
    }

    #[test]
    fn codex_body_additional_tools_requires_internal_retry_budget() {
        let body = body(serde_json::json!({
            "input": [
                {"type": "additional_tools", "tools": []},
                {"role": "user", "content": "KEEP"}
            ]
        }));

        assert!(codex_body_has_additional_tools(
            "codex",
            "/v1/responses",
            &body
        ));
        assert!(codex_body_has_additional_tools(
            "grok",
            "/responses/",
            &body
        ));
        assert!(!codex_body_has_additional_tools(
            "codex",
            "/v1/models",
            &body
        ));
        assert!(!codex_body_has_additional_tools(
            "claude",
            "/v1/responses",
            &body
        ));
    }

    #[test]
    fn codex_body_additional_tools_rejects_incompatible_shapes() {
        for body in [
            b"not-json".to_vec(),
            body(serde_json::json!({})),
            body(serde_json::json!({"input": "hello"})),
            body(serde_json::json!({"input": [{"type": "message"}]})),
            body(serde_json::json!({"input": [{"additional_tools": []}]})),
        ] {
            assert!(!codex_body_has_additional_tools(
                "codex",
                "/v1/responses",
                &body
            ));
        }
    }

    #[test]
    fn codex_body_agent_message_requires_internal_retry_budget() {
        let body = body(serde_json::json!({
            "input": [
                {"type": "agent_message", "content": [{"type": "input_text", "text": "KEEP"}]},
                {"role": "user", "content": "CONTINUE"}
            ]
        }));

        assert!(codex_body_has_agent_message(
            "codex",
            "/v1/responses",
            &body
        ));
        assert!(codex_body_has_agent_message("grok", "/responses/", &body));
        assert!(!codex_body_has_agent_message("codex", "/v1/models", &body));
        assert!(!codex_body_has_agent_message(
            "claude",
            "/v1/responses",
            &body
        ));
    }

    #[test]
    fn codex_body_agent_message_rejects_incompatible_shapes() {
        for body in [
            b"not-json".to_vec(),
            body(serde_json::json!({})),
            body(serde_json::json!({"input": "hello"})),
            body(serde_json::json!({"input": [{"type": "message"}]})),
            body(serde_json::json!({"input": [{"agent_message": []}]})),
        ] {
            assert!(!codex_body_has_agent_message(
                "codex",
                "/v1/responses",
                &body
            ));
        }
    }

    #[test]
    fn provider_max_attempts_reserves_budget_for_internal_retries() {
        assert_eq!(
            provider_regular_max_attempts_for_request(1, 1, false, false, false),
            1
        );
        assert_eq!(
            provider_regular_max_attempts_for_request(1, 1, true, false, false),
            2
        );
        assert_eq!(
            provider_regular_max_attempts_for_request(1, 1, false, true, false),
            2
        );
        assert_eq!(
            provider_regular_max_attempts_for_request(1, 1, true, true, false),
            3
        );
        assert_eq!(
            provider_regular_max_attempts_for_request(5, 1, true, true, false),
            5
        );
    }

    #[test]
    fn provider_max_attempts_respects_circuit_failure_threshold() {
        assert_eq!(
            provider_regular_max_attempts_for_request(1, 5, false, false, false),
            5
        );
        assert_eq!(
            provider_regular_max_attempts_for_request(3, 5, true, true, false),
            5
        );
        assert_eq!(
            provider_regular_max_attempts_for_request(10, 5, false, false, false),
            10
        );
    }

    #[test]
    fn provider_max_attempts_honors_strict_request_limit() {
        assert_eq!(
            provider_regular_max_attempts_for_request(1, 5, true, true, true),
            1
        );
    }

    #[test]
    fn provider_max_attempts_reserves_reasoning_context_rectifier_attempt() {
        assert_eq!(
            provider_total_max_attempts_for_request(1, true, false, false, false),
            2
        );
        assert_eq!(
            provider_total_max_attempts_for_request(2, true, false, false, false),
            3
        );
        assert_eq!(
            provider_total_max_attempts_for_request(1, true, false, false, true),
            1
        );
    }

    #[test]
    fn provider_max_attempts_reserves_chained_codex_rectifier_attempts() {
        assert_eq!(
            provider_total_max_attempts_for_request(1, true, true, false, false),
            3
        );
        assert_eq!(
            provider_total_max_attempts_for_request(1, false, true, false, false),
            2
        );
        assert_eq!(
            provider_total_max_attempts_for_request(1, true, true, true, false),
            4
        );
        assert_eq!(
            provider_total_max_attempts_for_request(1, false, false, true, false),
            2
        );
    }

    #[test]
    fn responses_bridge_path_guard_only_matches_responses_endpoint() {
        assert!(is_responses_request_path("/v1/responses"));
        assert!(is_responses_request_path("/responses/"));
        assert!(!is_responses_request_path("/v1/models"));
        assert!(!is_responses_request_path("/chat/completions"));
    }

    #[test]
    fn anthropic_messages_bridge_path_guard_only_matches_messages_endpoint() {
        assert!(is_anthropic_messages_request_path("/v1/messages"));
        assert!(is_anthropic_messages_request_path("/messages/"));
        assert!(!is_anthropic_messages_request_path("/v1/models"));
        assert!(!is_anthropic_messages_request_path("/chat/completions"));
    }

    #[test]
    fn translate_direct_bridge_request_maps_responses_to_chat_completions() {
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "DeepSeek-V4-Pro",
                "instructions": "You are Codex.",
                "input": [
                    {"role": "user", "content": [{"type": "input_text", "text": "hi"}]}
                ],
                "stream": true
            }))
            .unwrap(),
        );

        let translated = translate_direct_bridge_request(
            crate::providers::R2C_BRIDGE_TYPE,
            &body,
            Some("DeepSeek-V4-Pro"),
            true,
            &crate::gateway::proxy::cx2cc::settings::Cx2ccSettings::default(),
            &crate::providers::ClaudeModels::default(),
            &crate::providers::ProviderModelMapping::default(),
        )
        .expect("translate r2c request");
        let translated_body: serde_json::Value =
            serde_json::from_slice(translated.body_bytes.as_ref()).unwrap();

        assert_eq!(translated.forwarded_path, "/chat/completions");
        assert_eq!(translated_body["model"], "DeepSeek-V4-Pro");
        assert_eq!(translated_body["messages"][0]["role"], "system");
        assert_eq!(translated_body["messages"][0]["content"], "You are Codex.");
        assert_eq!(translated_body["messages"][1]["role"], "user");
        assert_eq!(translated_body["messages"][1]["content"], "hi");
        assert_eq!(translated_body["stream"], true);
    }

    #[test]
    fn translate_direct_bridge_request_applies_exact_model_mapping() {
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "gpt-5.5",
                "input": [
                    {"role": "user", "content": [{"type": "input_text", "text": "hi"}]}
                ],
                "stream": true
            }))
            .unwrap(),
        );
        let mapping = crate::providers::ProviderModelMapping::from_iter([(
            "gpt-5.5".to_string(),
            "DeepSeek-V4-Pro".to_string(),
        )]);

        let translated = translate_direct_bridge_request(
            crate::providers::R2C_BRIDGE_TYPE,
            &body,
            Some("gpt-5.5"),
            true,
            &crate::gateway::proxy::cx2cc::settings::Cx2ccSettings::default(),
            &crate::providers::ClaudeModels::default(),
            &mapping,
        )
        .expect("translate r2c request with model mapping");
        let translated_body: serde_json::Value =
            serde_json::from_slice(translated.body_bytes.as_ref()).unwrap();

        assert_eq!(translated_body["model"], "DeepSeek-V4-Pro");
    }

    #[test]
    fn apply_codex_api_key_model_mapping_rewrites_passthrough_model() {
        let mut body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "gpt-5.5",
                "input": "hi"
            }))
            .unwrap(),
        );
        let mapping = crate::providers::ProviderModelMapping::from_iter([(
            "gpt-5.5".to_string(),
            "LongCat-Flash-Chat".to_string(),
        )]);

        let applied = apply_codex_api_key_model_mapping(&mut body, &mapping);
        let translated_body: serde_json::Value = serde_json::from_slice(body.as_ref()).unwrap();

        assert_eq!(
            applied,
            Some(("gpt-5.5".to_string(), "LongCat-Flash-Chat".to_string()))
        );
        assert_eq!(translated_body["model"], "LongCat-Flash-Chat");
        assert_eq!(translated_body["input"], "hi");
    }

    #[test]
    fn translate_direct_bridge_request_maps_anthropic_messages_to_chat_completions() {
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "claude-sonnet-4-20250514",
                "system": "You are concise.",
                "max_tokens": 64,
                "messages": [
                    {"role": "user", "content": "hi"}
                ],
                "stream": true
            }))
            .unwrap(),
        );
        let claude_models = crate::providers::ClaudeModels {
            sonnet_model: Some("mimo-v2.5-pro".to_string()),
            ..Default::default()
        };

        let translated = translate_direct_bridge_request(
            crate::providers::CLAUDE_CHAT_COMPLETIONS_BRIDGE_TYPE,
            &body,
            Some("claude-sonnet-4-20250514"),
            true,
            &crate::gateway::proxy::cx2cc::settings::Cx2ccSettings::default(),
            &claude_models,
            &crate::providers::ProviderModelMapping::default(),
        )
        .expect("translate claude messages to chat completions");
        let translated_body: serde_json::Value =
            serde_json::from_slice(translated.body_bytes.as_ref()).unwrap();

        assert_eq!(translated.forwarded_path, "/chat/completions");
        assert_eq!(translated_body["model"], "mimo-v2.5-pro");
        assert_eq!(translated_body["messages"][0]["role"], "system");
        assert_eq!(
            translated_body["messages"][0]["content"],
            "You are concise."
        );
        assert_eq!(translated_body["messages"][1]["role"], "user");
        assert_eq!(translated_body["messages"][1]["content"], "hi");
        assert_eq!(translated_body["stream"], true);
    }

    #[test]
    fn translate_direct_bridge_request_flattens_array_tool_result_content_for_chat_completions() {
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "claude-sonnet-4-20250514",
                "max_tokens": 64,
                "messages": [
                    {"role": "user", "content": "Read file"},
                    {
                        "role": "assistant",
                        "content": [
                            {
                                "type": "tool_use",
                                "id": "call_1",
                                "name": "read_file",
                                "input": {"path": "Cargo.toml"}
                            }
                        ]
                    },
                    {
                        "role": "user",
                        "content": [
                            {
                                "type": "tool_result",
                                "tool_use_id": "call_1",
                                "content": [
                                    {"type": "text", "text": "[package]\n"},
                                    {"type": "text", "text": "name = \"aio-coding-hub\""}
                                ]
                            }
                        ]
                    }
                ],
                "stream": false
            }))
            .unwrap(),
        );
        let claude_models = crate::providers::ClaudeModels {
            sonnet_model: Some("mimo-v2.5-pro".to_string()),
            ..Default::default()
        };

        let translated = translate_direct_bridge_request(
            crate::providers::CLAUDE_CHAT_COMPLETIONS_BRIDGE_TYPE,
            &body,
            Some("claude-sonnet-4-20250514"),
            false,
            &crate::gateway::proxy::cx2cc::settings::Cx2ccSettings::default(),
            &claude_models,
            &crate::providers::ProviderModelMapping::default(),
        )
        .expect("translate array-format tool result to chat completions");
        let translated_body: serde_json::Value =
            serde_json::from_slice(translated.body_bytes.as_ref()).unwrap();
        let messages = translated_body["messages"].as_array().unwrap();

        assert_eq!(translated.forwarded_path, "/chat/completions");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "Read file");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], "");
        assert_eq!(messages[1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call_1");
        assert_eq!(
            messages[2]["content"],
            "[package]\nname = \"aio-coding-hub\""
        );
        assert!(messages
            .iter()
            .all(|message| message["content"].is_string() || message["content"].is_null()));
    }
}
