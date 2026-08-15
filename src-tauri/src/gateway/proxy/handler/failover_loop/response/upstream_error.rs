//! Usage: Handle upstream non-success responses and reqwest errors inside `failover_loop::run`.

use super::attempt_record::{
    record_system_failure_and_decide, record_system_failure_and_decide_no_cooldown,
    RecordSystemFailureArgs,
};
use super::context::{
    AttemptCtx, AttemptOutcome, CommonCtx, CommonCtxOwned, LoopControl, LoopState, ProviderCtx,
    MAX_NON_SSE_BODY_BYTES,
};
use super::thinking_signature_rectifier_400;
use super::{emit_attempt_event_and_log, AttemptCircuitFields};
use super::{
    emit_gateway_log, emit_request_event_and_enqueue_request_log, RequestCompletion,
    RequestEndArgs, RequestEndContextArgs, RequestEndDeps,
};
use crate::circuit_breaker;
use crate::domain::provider_oauth_limits;
use crate::gateway::events::decision_chain as dc;
use crate::gateway::events::FailoverAttempt;
use crate::gateway::proxy::errors::{
    classify_reqwest_error, classify_upstream_status, error_response,
};
use crate::gateway::proxy::failover::{retry_backoff_delay, FailoverDecision};
use crate::gateway::proxy::http_util::{
    build_response, has_gzip_content_encoding, has_non_identity_content_encoding,
    maybe_gunzip_response_body_bytes_with_limit,
};
use crate::gateway::proxy::is_claude_count_tokens_request;
use crate::gateway::proxy::provider_router;
use crate::gateway::proxy::upstream_client_error_rules;
use crate::gateway::proxy::{ErrorCategory, GatewayErrorCode};
use crate::gateway::response_fixer;
use crate::gateway::streams::GunzipStream;
use crate::gateway::util::{now_unix_seconds, strip_hop_headers};
use crate::shared::mutex_ext::MutexExt;
use axum::body::{Body, Bytes};
use axum::http::{header, HeaderValue};

use super::provider_iterator::is_responses_request_path;

const CLAUDE_CODE_CLIENT_RESTRICTION_REASON: &str = "claude_code_client_restriction";
const CLAUDE_CODE_CLIENT_RESTRICTION_PHRASE: &str = "this group only allows claude code clients";

fn contains_claude_code_client_restriction(message: &str) -> bool {
    let normalized = message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    normalized.contains(CLAUDE_CODE_CLIENT_RESTRICTION_PHRASE)
}

fn matches_claude_code_client_restriction(
    cli_key: &str,
    status: reqwest::StatusCode,
    body: &[u8],
) -> bool {
    if cli_key != "claude" || status != reqwest::StatusCode::SERVICE_UNAVAILABLE {
        return false;
    }

    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) {
        for message in [
            value.pointer("/error/message"),
            value.get("message"),
            value.pointer("/error/error/message"),
        ]
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        {
            if contains_claude_code_client_restriction(message) {
                return true;
            }
        }
    }

    contains_claude_code_client_restriction(&String::from_utf8_lossy(body))
}

fn upstream_error_decision(
    is_count_tokens: bool,
    base_decision: FailoverDecision,
    retry_index: u32,
    max_attempts_per_provider: u32,
) -> FailoverDecision {
    if is_count_tokens {
        return FailoverDecision::Abort;
    }

    if matches!(base_decision, FailoverDecision::RetrySameProvider)
        && retry_index >= max_attempts_per_provider
    {
        return FailoverDecision::SwitchProvider;
    }

    base_decision
}

fn reqwest_error_decision(
    is_count_tokens: bool,
    _is_connect: bool,
    retry_index: u32,
    max_attempts_per_provider: u32,
) -> FailoverDecision {
    if is_count_tokens {
        return FailoverDecision::Abort;
    }

    if retry_index < max_attempts_per_provider {
        FailoverDecision::RetrySameProvider
    } else {
        FailoverDecision::SwitchProvider
    }
}

pub(super) struct BoundedResponseBody {
    pub(super) body: Bytes,
    pub(super) truncated: bool,
}

async fn read_response_body_with_limit(
    mut resp: reqwest::Response,
    max_bytes: u64,
) -> Result<BoundedResponseBody, reqwest::Error> {
    let limit = max_bytes.min(usize::MAX as u64) as usize;
    let mut out = Vec::with_capacity(limit.min(16 * 1024));
    let content_length = resp.content_length();
    let mut truncated = content_length.is_some_and(|length| length > max_bytes);

    while out.len() < limit {
        let Some(chunk) = resp.chunk().await? else {
            break;
        };

        let remaining = limit - out.len();
        if chunk.len() > remaining {
            out.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }

        out.extend_from_slice(&chunk);
    }

    if out.len() == limit && content_length.is_none() && resp.chunk().await?.is_some() {
        truncated = true;
    }

    Ok(BoundedResponseBody {
        body: Bytes::from(out),
        truncated,
    })
}

fn error_body_scan_limit_bytes() -> u64 {
    upstream_client_error_rules::max_body_read_bytes().min(MAX_NON_SSE_BODY_BYTES as u64)
}

pub(super) fn error_body_scan_limit_usize() -> usize {
    error_body_scan_limit_bytes().min(usize::MAX as u64) as usize
}

fn retry_after_reset_at(headers: &axum::http::HeaderMap, now_unix: i64) -> Option<i64> {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| {
            if let Ok(seconds) = value.parse::<i64>() {
                return (seconds > 0).then_some(now_unix.saturating_add(seconds));
            }
            chrono::DateTime::parse_from_rfc2822(value)
                .ok()
                .map(|value| value.timestamp())
                .filter(|timestamp| *timestamp > 0)
        })
}

fn save_oauth_quota_exhausted_snapshot(
    db: &crate::db::Db,
    provider_id: i64,
    reset_at: Option<i64>,
) {
    if let Err(err) = provider_oauth_limits::save_exhausted_snapshot(db, provider_id, reset_at) {
        tracing::warn!(
            provider_id,
            "failed to save OAuth exhausted quota snapshot: {err}"
        );
    }
}

pub(super) async fn read_response_body_for_error_scan(
    resp: reqwest::Response,
) -> Result<BoundedResponseBody, reqwest::Error> {
    read_response_body_with_limit(resp, error_body_scan_limit_bytes()).await
}

pub(super) struct UpstreamRequestState<'a> {
    pub(super) upstream_body_bytes: &'a mut Bytes,
    pub(super) strip_request_content_encoding: &'a mut bool,
    pub(super) codex_previous_response_id_rectifier_retried: &'a mut bool,
    pub(super) codex_reasoning_context_rectifier_retried: &'a mut bool,
    pub(super) codex_reasoning_context_retry_pending: &'a mut bool,
    pub(super) codex_additional_tools_rectifier_retried: &'a mut bool,
    pub(super) codex_additional_tools_retry_pending: &'a mut bool,
    pub(super) codex_agent_message_rectifier_retried: &'a mut bool,
    pub(super) codex_agent_message_retry_pending: &'a mut bool,
    pub(super) thinking_effort_conflict_rectifier_retried: &'a mut bool,
    pub(super) thinking_signature_rectifier_retried: &'a mut bool,
    pub(super) thinking_budget_rectifier_retried: &'a mut bool,
    pub(super) gemini_function_id_rectifier_retried: &'a mut bool,
    pub(super) additional_repair_retry_slots: &'a mut u32,
}

fn codex_request_has_previous_response_id(body: &[u8]) -> bool {
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

fn should_scan_codex_previous_response_id_error(
    cli_key: &str,
    status: reqwest::StatusCode,
    already_retried: bool,
    upstream_body: &[u8],
) -> bool {
    // grok 与 codex 同走 OpenAI Responses API：failover 切换供应商后
    // previous_response_id 在新供应商侧不存在，同样需要摘除后重试。
    matches!(cli_key, "codex" | "grok")
        && !already_retried
        && matches!(
            status,
            reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::NOT_FOUND
        )
        && codex_request_has_previous_response_id(upstream_body)
}

fn matches_codex_previous_response_id_error(status: reqwest::StatusCode, body: &[u8]) -> bool {
    if !matches!(
        status,
        reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::NOT_FOUND
    ) {
        return false;
    }
    if body.is_empty() {
        return false;
    }

    let haystack = String::from_utf8_lossy(body).to_ascii_lowercase();
    let mentions_previous_response = haystack.contains("previous_response_id")
        || haystack.contains("previous response")
        || haystack.contains("previous response id");
    let says_missing = haystack.contains("not found")
        || haystack.contains("no response")
        || haystack.contains("could not find")
        || haystack.contains("does not exist")
        || haystack.contains("unknown")
        || haystack.contains("invalid");

    mentions_previous_response && says_missing
}

fn remove_codex_previous_response_id(body: &mut Bytes) -> bool {
    let Ok(mut root) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };

    let Some(obj) = root.as_object_mut() else {
        return false;
    };
    if obj.remove("previous_response_id").is_none() {
        return false;
    }

    match serde_json::to_vec(&root) {
        Ok(next) => {
            *body = Bytes::from(next);
            true
        }
        Err(_) => false,
    }
}

fn matches_codex_reasoning_context_error(status: reqwest::StatusCode, body: &[u8]) -> bool {
    if status != reqwest::StatusCode::BAD_REQUEST {
        return false;
    }

    let Ok(root) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };

    root.pointer("/error/code")
        .and_then(serde_json::Value::as_str)
        == Some("unsupported_field")
        && root
            .pointer("/error/param")
            .and_then(serde_json::Value::as_str)
            == Some("reasoning.context")
}

fn remove_codex_reasoning_context(body: &mut Bytes) -> bool {
    let Ok(mut root) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let Some(root_obj) = root.as_object_mut() else {
        return false;
    };
    let Some(reasoning) = root_obj
        .get_mut("reasoning")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return false;
    };
    if reasoning.remove("context").is_none() {
        return false;
    }
    if reasoning.is_empty() {
        root_obj.remove("reasoning");
    }

    match serde_json::to_vec(&root) {
        Ok(next) => {
            *body = Bytes::from(next);
            true
        }
        Err(_) => false,
    }
}

fn maybe_rectify_codex_reasoning_context(
    cli_key: &str,
    forwarded_path: &str,
    status: reqwest::StatusCode,
    error_body: &[u8],
    error_body_truncated: bool,
    already_retried: &mut bool,
    upstream_body: &mut Bytes,
) -> Option<LoopControl> {
    if !matches!(cli_key, "codex" | "grok")
        || !is_responses_request_path(forwarded_path)
        || error_body_truncated
        || *already_retried
        || !matches_codex_reasoning_context_error(status, error_body)
        || !remove_codex_reasoning_context(upstream_body)
    {
        return None;
    }

    *already_retried = true;
    Some(LoopControl::ContinueRetry)
}

fn matches_codex_additional_tools_error(status: reqwest::StatusCode, body: &[u8]) -> bool {
    if status != reqwest::StatusCode::BAD_REQUEST {
        return false;
    }

    let Ok(root) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let param_matches = root
        .pointer("/error/param")
        .and_then(serde_json::Value::as_str)
        .and_then(|param| param.strip_prefix("input["))
        .and_then(|param| param.strip_suffix("].type"))
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()));

    root.pointer("/error/type")
        .and_then(serde_json::Value::as_str)
        == Some("invalid_request_error")
        && root
            .pointer("/error/code")
            .and_then(serde_json::Value::as_str)
            == Some("invalid_request")
        && root
            .pointer("/error/message")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            == Some("unsupported input item type: additional_tools")
        && param_matches
}

fn remove_codex_additional_tools_input_items(body: &mut Bytes) -> Option<usize> {
    let Ok(mut root) = serde_json::from_slice::<serde_json::Value>(body) else {
        return None;
    };
    let input = root.get_mut("input")?.as_array_mut()?;
    let original_len = input.len();
    input.retain(|item| {
        item.get("type").and_then(serde_json::Value::as_str) != Some("additional_tools")
    });
    let removed = original_len.saturating_sub(input.len());
    if removed == 0 {
        return None;
    }

    let next = serde_json::to_vec(&root).ok()?;
    *body = Bytes::from(next);
    Some(removed)
}

fn maybe_rectify_codex_additional_tools(
    cli_key: &str,
    forwarded_path: &str,
    status: reqwest::StatusCode,
    error_body: &[u8],
    error_body_truncated: bool,
    already_retried: &mut bool,
    upstream_body: &mut Bytes,
) -> Option<usize> {
    if !matches!(cli_key, "codex" | "grok")
        || !is_responses_request_path(forwarded_path)
        || error_body_truncated
        || *already_retried
        || !matches_codex_additional_tools_error(status, error_body)
    {
        return None;
    }

    let removed = remove_codex_additional_tools_input_items(upstream_body)?;
    *already_retried = true;
    Some(removed)
}

fn matches_codex_agent_message_error(status: reqwest::StatusCode, body: &[u8]) -> bool {
    if status != reqwest::StatusCode::BAD_REQUEST {
        return false;
    }

    let Ok(root) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let param_matches = root
        .pointer("/error/param")
        .and_then(serde_json::Value::as_str)
        .and_then(|param| param.strip_prefix("input["))
        .and_then(|param| param.strip_suffix("].type"))
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()));

    root.pointer("/error/type")
        .and_then(serde_json::Value::as_str)
        == Some("invalid_request_error")
        && root
            .pointer("/error/code")
            .and_then(serde_json::Value::as_str)
            == Some("invalid_request")
        && root
            .pointer("/error/message")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            == Some("unsupported input item type: agent_message")
        && param_matches
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CodexAgentMessageRectificationOutcome {
    items_converted: usize,
    encrypted_content_parts_removed: usize,
    empty_items_removed: usize,
}

impl CodexAgentMessageRectificationOutcome {
    fn changed(self) -> bool {
        self.items_converted > 0 || self.empty_items_removed > 0
    }
}

fn convert_codex_agent_messages_to_user_messages(
    body: &mut Bytes,
) -> Option<CodexAgentMessageRectificationOutcome> {
    let Ok(mut root) = serde_json::from_slice::<serde_json::Value>(body) else {
        return None;
    };
    let input = root.get_mut("input")?.as_array_mut()?;
    let mut next_input = Vec::with_capacity(input.len());
    let mut outcome = CodexAgentMessageRectificationOutcome {
        items_converted: 0,
        encrypted_content_parts_removed: 0,
        empty_items_removed: 0,
    };

    for item in std::mem::take(input) {
        let Some(item_obj) = item.as_object() else {
            next_input.push(item);
            continue;
        };
        if item_obj.get("type").and_then(serde_json::Value::as_str) != Some("agent_message") {
            next_input.push(item);
            continue;
        }
        let Some(mut content) = item_obj
            .get("content")
            .and_then(serde_json::Value::as_array)
            .cloned()
        else {
            next_input.push(item);
            continue;
        };

        let content_len_before = content.len();
        content.retain(|part| {
            part.get("type").and_then(serde_json::Value::as_str) != Some("encrypted_content")
        });
        outcome.encrypted_content_parts_removed = outcome
            .encrypted_content_parts_removed
            .saturating_add(content_len_before.saturating_sub(content.len()));

        if content.is_empty() {
            outcome.empty_items_removed = outcome.empty_items_removed.saturating_add(1);
            continue;
        }

        next_input.push(serde_json::json!({
            "type": "message",
            "role": "user",
            "content": content,
        }));
        outcome.items_converted = outcome.items_converted.saturating_add(1);
    }
    if !outcome.changed() {
        return None;
    }
    *input = next_input;

    let next = serde_json::to_vec(&root).ok()?;
    *body = Bytes::from(next);
    Some(outcome)
}

fn maybe_rectify_codex_agent_messages(
    cli_key: &str,
    forwarded_path: &str,
    status: reqwest::StatusCode,
    error_body: &[u8],
    error_body_truncated: bool,
    already_retried: &mut bool,
    upstream_body: &mut Bytes,
) -> Option<CodexAgentMessageRectificationOutcome> {
    if !matches!(cli_key, "codex" | "grok")
        || !is_responses_request_path(forwarded_path)
        || error_body_truncated
        || *already_retried
        || !matches_codex_agent_message_error(status, error_body)
    {
        return None;
    }

    let outcome = convert_codex_agent_messages_to_user_messages(upstream_body)?;
    *already_retried = true;
    Some(outcome)
}

#[allow(clippy::too_many_arguments)]
fn maybe_prepare_codex_agent_message_retry(
    cli_key: &str,
    forwarded_path: &str,
    status: reqwest::StatusCode,
    error_body: &[u8],
    error_body_truncated: bool,
    already_retried: &mut bool,
    retry_pending: &mut bool,
    strip_request_content_encoding: &mut bool,
    upstream_body: &mut Bytes,
) -> Option<(LoopControl, CodexAgentMessageRectificationOutcome)> {
    let outcome = maybe_rectify_codex_agent_messages(
        cli_key,
        forwarded_path,
        status,
        error_body,
        error_body_truncated,
        already_retried,
        upstream_body,
    )?;
    *strip_request_content_encoding = true;
    *retry_pending = true;
    Some((LoopControl::ContinueRetry, outcome))
}

fn codex_agent_message_rectifier_special_setting(
    provider_id: i64,
    status: reqwest::StatusCode,
    retry_index: u32,
    outcome: CodexAgentMessageRectificationOutcome,
) -> serde_json::Value {
    serde_json::json!({
        "type": "codex_agent_message_rectifier",
        "scope": "attempt",
        "hit": true,
        "action": "convert_agent_messages_to_user_messages_and_retry",
        "providerId": provider_id,
        "status": status.as_u16(),
        "retryAttemptNumber": retry_index,
        "retryAttemptNumberNext": retry_index + 1,
        "itemsConverted": outcome.items_converted,
        "encryptedContentPartsRemoved": outcome.encrypted_content_parts_removed,
        "emptyItemsRemoved": outcome.empty_items_removed,
    })
}

pub(super) struct HandleNonSuccessResponseInput<'a, R: tauri::Runtime = tauri::Wry> {
    pub(super) ctx: CommonCtx<'a, R>,
    pub(super) provider_ctx: ProviderCtx<'a>,
    pub(super) attempt_ctx: AttemptCtx<'a>,
    pub(super) loop_state: LoopState<'a, R>,
    pub(super) enable_thinking_signature_rectifier: bool,
    pub(super) enable_thinking_budget_rectifier: bool,
    pub(super) enable_thinking_effort_conflict_rectifier: bool,
    pub(super) enable_gemini_function_id_rectifier: bool,
    pub(super) resp: reqwest::Response,
    pub(super) upstream: UpstreamRequestState<'a>,
}

pub(super) async fn handle_non_success_response<R: tauri::Runtime>(
    input: HandleNonSuccessResponseInput<'_, R>,
) -> LoopControl {
    let HandleNonSuccessResponseInput {
        ctx,
        provider_ctx,
        attempt_ctx,
        loop_state,
        enable_thinking_signature_rectifier,
        enable_thinking_budget_rectifier,
        enable_thinking_effort_conflict_rectifier,
        enable_gemini_function_id_rectifier,
        resp,
        upstream,
    } = input;
    let status = resp.status();
    let response_headers = resp.headers().clone();
    let is_count_tokens =
        is_claude_count_tokens_request(ctx.cli_key.as_str(), ctx.forwarded_path.as_str());

    let reactive_rectifier_enabled = (ctx.cli_key == "claude"
        && (enable_thinking_effort_conflict_rectifier
            || enable_thinking_signature_rectifier
            || enable_thinking_budget_rectifier))
        || (ctx.cli_key == "gemini" && enable_gemini_function_id_rectifier);
    if !is_count_tokens
        && status.as_u16() == 400
        && !attempt_ctx.cx2cc_active
        && reactive_rectifier_enabled
    {
        return thinking_signature_rectifier_400::handle_thinking_rectifiers_400(
            thinking_signature_rectifier_400::HandleThinkingRectifiers400Input {
                ctx,
                provider_ctx,
                attempt_ctx,
                loop_state,
                enable_thinking_signature_rectifier,
                enable_thinking_budget_rectifier,
                enable_thinking_effort_conflict_rectifier,
                enable_gemini_function_id_rectifier,
                resp,
                status,
                response_headers,
                upstream,
            },
        )
        .await;
    }

    let mut resp = Some(resp);

    let state = ctx.state;
    let provider_cooldown_secs = ctx.provider_cooldown_secs;

    let ProviderCtx {
        provider_id,
        provider_name_base,
        provider_base_url_base,
        auth_mode,
        provider_index,
        session_reuse,
        ..
    } = provider_ctx;

    let AttemptCtx {
        attempt_index: _,
        retry_index,
        provider_max_attempts,
        attempt_started_ms,
        attempt_started,
        circuit_before,
        cx2cc_active,
        ..
    } = attempt_ctx;

    let LoopState {
        attempts,
        failed_provider_ids,
        last_outcome,
        circuit_snapshot,
        abort_guard,
    } = loop_state;

    let (base_category, error_code, base_decision) = classify_upstream_status(status);
    let mut category = base_category;
    let mut decision = upstream_error_decision(
        is_count_tokens,
        base_decision,
        retry_index,
        provider_max_attempts,
    );
    let mut response_status = status;

    let mut abort_body_bytes: Option<Bytes> = None;
    let mut abort_response_headers: Option<axum::http::HeaderMap> = None;
    let mut abort_body_truncated = false;
    let mut matched_rule_id: Option<&'static str> = None;
    let mut matched_429_concurrency_limit = false;
    let mut matched_claude_client_restriction = false;
    // Body preview for errors where preserving the upstream diagnostic text matters.
    let mut upstream_body_preview: Option<String> = None;
    let need_client_error_scan = !is_count_tokens
        && (upstream_client_error_rules::should_attempt_non_retryable_match(
            status,
            resp.as_ref().and_then(|r| r.content_length()),
        ) || matches!(status.as_u16(), 402 | 429));
    // Error classification and diagnostic capture are separate concerns: statuses such as 401
    // intentionally skip rule matching, but their bounded body is still useful in request logs.
    let need_error_body_preview = !is_count_tokens
        && (status.is_client_error() || status.is_server_error())
        && !need_client_error_scan;
    let need_codex_previous_response_id_scan = !is_count_tokens
        && should_scan_codex_previous_response_id_error(
            ctx.cli_key.as_str(),
            status,
            *upstream.codex_previous_response_id_rectifier_retried,
            upstream.upstream_body_bytes,
        );
    let need_codex_reasoning_context_scan = !is_count_tokens
        && matches!(ctx.cli_key.as_str(), "codex" | "grok")
        && is_responses_request_path(ctx.forwarded_path.as_str())
        && status == reqwest::StatusCode::BAD_REQUEST
        && !*upstream.codex_reasoning_context_rectifier_retried;
    let need_codex_additional_tools_scan = !is_count_tokens
        && matches!(ctx.cli_key.as_str(), "codex" | "grok")
        && is_responses_request_path(ctx.forwarded_path.as_str())
        && status == reqwest::StatusCode::BAD_REQUEST
        && !*upstream.codex_additional_tools_rectifier_retried;
    let need_codex_agent_message_scan = !is_count_tokens
        && matches!(ctx.cli_key.as_str(), "codex" | "grok")
        && is_responses_request_path(ctx.forwarded_path.as_str())
        && status == reqwest::StatusCode::BAD_REQUEST
        && !*upstream.codex_agent_message_rectifier_retried;
    if need_client_error_scan
        || need_error_body_preview
        || need_codex_previous_response_id_scan
        || need_codex_reasoning_context_scan
        || need_codex_additional_tools_scan
        || need_codex_agent_message_scan
    {
        if let Some(r) = resp.take() {
            let read_result = read_response_body_for_error_scan(r).await;
            if let Ok(BoundedResponseBody {
                body: buffered_body,
                truncated,
            }) = read_result
            {
                abort_body_truncated = truncated;
                let mut headers_for_scan = response_headers.clone();
                strip_hop_headers(&mut headers_for_scan);
                let body_for_scan = maybe_gunzip_response_body_bytes_with_limit(
                    buffered_body,
                    &mut headers_for_scan,
                    error_body_scan_limit_usize(),
                );
                // CX2CC: log upstream error body to console for debugging.
                if cx2cc_active && retry_index == 1 {
                    let preview = String::from_utf8_lossy(&body_for_scan);
                    let truncated: String = preview.chars().take(500).collect();
                    emit_gateway_log(
                        &state.app,
                        "warn",
                        "CX2CC_UPSTREAM_ERROR",
                        format!(
                            "[CX2CC] upstream {}: {} (provider={})",
                            status.as_u16(),
                            truncated,
                            provider_name_base,
                        ),
                    );
                }
                // Extract a bounded body preview for diagnostics on upstream errors.
                if status.is_server_error() || status.is_client_error() {
                    let preview = String::from_utf8_lossy(&body_for_scan);
                    let truncated: String = preview.chars().take(500).collect();
                    if !truncated.is_empty() {
                        upstream_body_preview = Some(truncated);
                    }
                }
                matched_claude_client_restriction = matches_claude_code_client_restriction(
                    ctx.cli_key.as_str(),
                    status,
                    body_for_scan.as_ref(),
                );
                if matched_claude_client_restriction {
                    decision = FailoverDecision::SwitchProvider;
                    emit_gateway_log(
                        &state.app,
                        "debug",
                        "CLAUDE_CODE_CLIENT_RESTRICTION",
                        format!(
                            "[FAILOVER] trace_id={} provider_id={} status={} reason_code={}",
                            ctx.trace_id,
                            provider_id,
                            status.as_u16(),
                            CLAUDE_CODE_CLIENT_RESTRICTION_REASON,
                        ),
                    );
                }
                if need_client_error_scan {
                    if matches!(status.as_u16(), 402 | 429)
                        && upstream_client_error_rules::match_quota_exhausted(
                            body_for_scan.as_ref(),
                        )
                    {
                        category = ErrorCategory::ProviderError;
                        decision = FailoverDecision::SwitchProvider;
                        matched_rule_id = Some("quota_exhausted");
                    }
                    if status.as_u16() == 429 {
                        matched_429_concurrency_limit =
                            upstream_client_error_rules::match_429_concurrency_limit(
                                body_for_scan.as_ref(),
                            );
                    }
                    let matched_non_retryable_rule =
                        upstream_client_error_rules::match_non_retryable_client_error(
                            ctx.cli_key.as_str(),
                            status,
                            body_for_scan.as_ref(),
                        );
                    if matched_non_retryable_rule.is_some() {
                        matched_rule_id = matched_non_retryable_rule;
                    }
                    if matched_non_retryable_rule.is_some() || matched_429_concurrency_limit {
                        category = ErrorCategory::NonRetryableClientError;
                        decision = FailoverDecision::Abort;
                    }
                }
                if let Some(rule_id) =
                    upstream_client_error_rules::match_wrapped_non_retryable_client_error(
                        ctx.cli_key.as_str(),
                        status,
                        body_for_scan.as_ref(),
                    )
                {
                    matched_rule_id = Some(rule_id);
                    category = ErrorCategory::NonRetryableClientError;
                    decision = FailoverDecision::Abort;
                    response_status = reqwest::StatusCode::BAD_REQUEST;
                }
                // Preserve consumed body/headers so downstream (e.g. Abort
                // pass-through) can still use them after resp.take().
                if abort_body_bytes.is_none() {
                    abort_body_bytes = Some(body_for_scan);
                    abort_response_headers = Some(headers_for_scan);
                }
            }
        }
    }

    if need_codex_reasoning_context_scan {
        if let Some(body) = abort_body_bytes.as_deref() {
            if let Some(control) = maybe_rectify_codex_reasoning_context(
                ctx.cli_key.as_str(),
                ctx.forwarded_path.as_str(),
                status,
                body,
                abort_body_truncated,
                upstream.codex_reasoning_context_rectifier_retried,
                upstream.upstream_body_bytes,
            ) {
                *upstream.strip_request_content_encoding = true;
                *upstream.codex_reasoning_context_retry_pending = true;
                response_fixer::push_special_setting(
                    ctx.special_settings,
                    serde_json::json!({
                        "type": "codex_reasoning_context_rectifier",
                        "scope": "attempt",
                        "hit": true,
                        "action": "remove_reasoning_context_and_retry",
                        "providerId": provider_id,
                        "status": status.as_u16(),
                        "retryAttemptNumber": retry_index,
                        "retryAttemptNumberNext": retry_index + 1,
                    }),
                );
                return control;
            }
        }
    }

    if need_codex_additional_tools_scan {
        if let Some(body) = abort_body_bytes.as_deref() {
            if let Some(items_removed) = maybe_rectify_codex_additional_tools(
                ctx.cli_key.as_str(),
                ctx.forwarded_path.as_str(),
                status,
                body,
                abort_body_truncated,
                upstream.codex_additional_tools_rectifier_retried,
                upstream.upstream_body_bytes,
            ) {
                *upstream.strip_request_content_encoding = true;
                *upstream.codex_additional_tools_retry_pending = true;
                response_fixer::push_special_setting(
                    ctx.special_settings,
                    serde_json::json!({
                        "type": "codex_additional_tools_rectifier",
                        "scope": "attempt",
                        "hit": true,
                        "action": "remove_additional_tools_input_items_and_retry",
                        "providerId": provider_id,
                        "status": status.as_u16(),
                        "retryAttemptNumber": retry_index,
                        "retryAttemptNumberNext": retry_index + 1,
                        "itemsRemoved": items_removed,
                    }),
                );
                return LoopControl::ContinueRetry;
            }
        }
    }

    if need_codex_agent_message_scan {
        if let Some(body) = abort_body_bytes.as_deref() {
            if let Some((control, outcome)) = maybe_prepare_codex_agent_message_retry(
                ctx.cli_key.as_str(),
                ctx.forwarded_path.as_str(),
                status,
                body,
                abort_body_truncated,
                upstream.codex_agent_message_rectifier_retried,
                upstream.codex_agent_message_retry_pending,
                upstream.strip_request_content_encoding,
                upstream.upstream_body_bytes,
            ) {
                response_fixer::push_special_setting(
                    ctx.special_settings,
                    codex_agent_message_rectifier_special_setting(
                        provider_id,
                        status,
                        retry_index,
                        outcome,
                    ),
                );
                return control;
            }
        }
    }

    if need_codex_previous_response_id_scan {
        if let Some(body) = abort_body_bytes.as_deref() {
            if matches_codex_previous_response_id_error(status, body)
                && remove_codex_previous_response_id(upstream.upstream_body_bytes)
            {
                *upstream.codex_previous_response_id_rectifier_retried = true;
                *upstream.strip_request_content_encoding = true;
                ctx.special_settings
                    .lock_or_recover()
                    .push(serde_json::json!({
                        "type": "codex_previous_response_id_rectifier",
                        "scope": "attempt",
                        "hit": true,
                        "action": "remove_previous_response_id_and_retry",
                        "providerId": provider_id,
                        "providerName": provider_name_base,
                        "status": status.as_u16(),
                        "retryAttemptNumber": retry_index,
                        "retryAttemptNumberNext": retry_index + 1,
                    }));
                return LoopControl::ContinueRetry;
            }
        }
    }

    // When an upstream returns a 400 with a Responses API schema mismatch
    // (e.g. "input[45].content: array too long"), the provider does not support
    // the Responses format. Treat this as a provider error so failover can
    // try the next provider instead of aborting immediately.
    if !is_count_tokens
        && status.as_u16() == 400
        && matched_rule_id.is_none()
        && !matches!(decision, FailoverDecision::SwitchProvider)
    {
        if let Some(ref bytes) = abort_body_bytes {
            let body_text = String::from_utf8_lossy(bytes);
            if body_text.contains("input[")
                && body_text.contains("content")
                && body_text.contains("array too long")
            {
                category = ErrorCategory::ProviderError;
                decision = FailoverDecision::SwitchProvider;
                matched_rule_id = Some("responses_api_schema_mismatch");
            }
        }
    }

    if !is_count_tokens
        && upstream_client_error_rules::should_abort_unmatched_client_error(status, matched_rule_id)
    {
        category = ErrorCategory::NonRetryableClientError;
        decision = FailoverDecision::Abort;
        // Extract body preview for diagnostic logging when aborting unmatched 4xx.
        if upstream_body_preview.is_none() {
            if let Some(ref bytes) = abort_body_bytes {
                let preview = String::from_utf8_lossy(bytes);
                let truncated: String = preview.chars().take(500).collect();
                if !truncated.is_empty() {
                    upstream_body_preview = Some(truncated);
                }
            }
        }
    }

    let oauth_quota_exhausted = auth_mode == "oauth" && matched_rule_id == Some("quota_exhausted");
    let mut circuit_state_before = Some(circuit_before.state.as_str());
    let mut circuit_state_after: Option<&'static str> = None;
    let mut circuit_failure_count = Some(circuit_before.failure_count);
    let circuit_failure_threshold = Some(circuit_before.failure_threshold);

    let now_unix = now_unix_seconds() as i64;
    if oauth_quota_exhausted {
        save_oauth_quota_exhausted_snapshot(
            &state.db,
            provider_id,
            retry_after_reset_at(&response_headers, now_unix),
        );
    }

    if !is_count_tokens
        && matches!(category, ErrorCategory::ProviderError)
        && !oauth_quota_exhausted
        && !matched_claude_client_restriction
    {
        let change = provider_router::record_failure_and_emit_transition(
            provider_router::RecordCircuitArgs::from_state(
                state,
                ctx.trace_id.as_str(),
                ctx.cli_key.as_str(),
                provider_id,
                provider_name_base.as_str(),
                provider_base_url_base.as_str(),
                now_unix,
            )
            .with_provider_health_neutral(ctx.provider_health_neutral),
        );
        *circuit_snapshot = change.after.clone();
        circuit_state_before = Some(change.before.state.as_str());
        circuit_state_after = Some(change.after.state.as_str());
        circuit_failure_count = Some(change.after.failure_count);

        if change.after.state == circuit_breaker::CircuitState::Open {
            decision = FailoverDecision::SwitchProvider;
        }
    }

    if !is_count_tokens
        && provider_cooldown_secs > 0
        && matches!(category, ErrorCategory::ProviderError)
        && !oauth_quota_exhausted
        && !matched_claude_client_restriction
        && matches!(
            decision,
            FailoverDecision::SwitchProvider | FailoverDecision::Abort
        )
    {
        let snap = provider_router::trigger_cooldown(
            state.circuit.as_ref(),
            provider_id,
            now_unix,
            provider_cooldown_secs,
            ctx.provider_health_neutral,
        );
        *circuit_snapshot = snap;
    }

    let reason = if matched_claude_client_restriction {
        format!(
            "status={} rule={CLAUDE_CODE_CLIENT_RESTRICTION_REASON}",
            status.as_u16()
        )
    } else if matched_429_concurrency_limit {
        format!("status={} rule=429_concurrency_limit", status.as_u16())
    } else {
        let base = match matched_rule_id {
            Some(rule_id) => format!("status={} rule={rule_id}", status.as_u16()),
            None => format!("status={}", status.as_u16()),
        };
        match upstream_body_preview {
            Some(ref preview) => format!("{base}, upstream_body={preview}"),
            None => base,
        }
    };
    let outcome = format!(
        "upstream_error: status={} category={} code={} decision={}",
        status.as_u16(),
        category.as_str(),
        error_code,
        decision.as_str()
    );
    let selection_method = dc::selection_method(provider_index, retry_index, session_reuse);
    let reason_code = if matched_claude_client_restriction {
        CLAUDE_CODE_CLIENT_RESTRICTION_REASON
    } else {
        category.reason_code()
    };

    attempts.push(FailoverAttempt {
        provider_id,
        provider_name: provider_name_base.clone(),
        base_url: provider_base_url_base.clone(),
        outcome: outcome.clone(),
        status: Some(status.as_u16()),
        provider_index: Some(provider_index),
        retry_index: Some(retry_index),
        session_reuse,
        error_category: Some(category.as_str()),
        error_code: Some(error_code),
        decision: Some(decision.as_str()),
        reason: Some(reason),
        selection_method,
        reason_code: Some(reason_code),
        attempt_started_ms: Some(attempt_started_ms),
        attempt_duration_ms: Some(attempt_started.elapsed().as_millis()),
        circuit_state_before,
        circuit_state_after,
        circuit_failure_count,
        circuit_failure_threshold,
        circuit_recover_at_unix: None,
        circuit_trigger_error_code: None,
        provider_bridged: Some(provider_ctx.provider_bridged),
        timeout_secs: None,
        reasoning_effort: attempt_ctx.reasoning_effort.map(str::to_string),
        upstream_sent: attempt_ctx.upstream_sent,
        claude_model_mapping: provider_ctx.claude_model_mapping.cloned(),
        model_redirect: provider_ctx.model_redirect.cloned(),
    });

    emit_attempt_event_and_log(
        ctx,
        provider_ctx,
        attempt_ctx,
        outcome,
        Some(status.as_u16()),
        AttemptCircuitFields {
            state_before: circuit_state_before,
            state_after: circuit_state_after,
            failure_count: circuit_failure_count,
            failure_threshold: circuit_failure_threshold,
        },
    )
    .await;

    *last_outcome = Some(AttemptOutcome::new(category.as_str(), error_code));

    match decision {
        FailoverDecision::RetrySameProvider => {
            if let Some(delay) = retry_backoff_delay(status, retry_index) {
                tokio::time::sleep(delay).await;
            }
            LoopControl::ContinueRetry
        }
        FailoverDecision::SwitchProvider => {
            failed_provider_ids.insert(provider_id);
            LoopControl::BreakRetry
        }
        FailoverDecision::Abort => {
            // On abort, we intentionally do NOT use stream tee finalizers, to avoid triggering

            let CommonCtxOwned {
                cli_key,
                method_hint,
                forwarded_path,
                query,
                trace_id,
                started,
                created_at_ms,
                created_at,
                session_id,
                requested_model,
                special_settings,
                enable_response_fixer,
                response_fixer_non_stream_config,
                ..
            } = CommonCtxOwned::from(ctx);

            if let (Some(mut response_headers), Some(mut body_bytes)) =
                (abort_response_headers, abort_body_bytes)
            {
                let enable_response_fixer_for_this_response =
                    enable_response_fixer && !has_non_identity_content_encoding(&response_headers);
                if enable_response_fixer_for_this_response {
                    response_headers.remove(header::CONTENT_LENGTH);
                    let outcome = response_fixer::process_non_stream(
                        body_bytes,
                        response_fixer_non_stream_config,
                    );
                    response_headers.insert(
                        "x-cch-response-fixer",
                        HeaderValue::from_static(outcome.header_value),
                    );
                    if let Some(setting) = outcome.special_setting {
                        response_fixer::push_special_setting(&special_settings, setting);
                    }
                    body_bytes = outcome.body;
                }

                let special_settings_json =
                    response_fixer::special_settings_json(&special_settings);
                let duration_ms = started.elapsed().as_millis();

                emit_request_event_and_enqueue_request_log(
                    RequestEndArgs::from_context(RequestEndContextArgs {
                        deps: RequestEndDeps::new(
                            &state.app,
                            &state.db,
                            &state.log_tx,
                            &state.plugin_pipeline,
                            &state.active_requests,
                        ),
                        trace_id: trace_id.as_str(),
                        cli_key: cli_key.as_str(),
                        method: method_hint.as_str(),
                        path: forwarded_path.as_str(),
                        observe: ctx.observe,
                        query: query.as_deref(),
                        excluded_from_stats: false,
                        duration_ms,
                        attempts: attempts.as_slice(),
                        special_settings_json,
                        session_id,
                        requested_model,
                        created_at_ms,
                        created_at,
                    })
                    .with_completion(RequestCompletion::failure_with_ttfb(
                        response_status.as_u16(),
                        Some(category.as_str()),
                        error_code,
                        duration_ms,
                    )),
                )
                .await;

                abort_guard.disarm();

                return LoopControl::Return(build_response(
                    response_status,
                    &response_headers,
                    trace_id.as_str(),
                    Body::from(body_bytes),
                ));
            }

            let special_settings_json = response_fixer::special_settings_json(&special_settings);
            let duration_ms = started.elapsed().as_millis();

            emit_request_event_and_enqueue_request_log(
                RequestEndArgs::from_context(RequestEndContextArgs {
                    deps: RequestEndDeps::new(
                        &state.app,
                        &state.db,
                        &state.log_tx,
                        &state.plugin_pipeline,
                        &state.active_requests,
                    ),
                    trace_id: trace_id.as_str(),
                    cli_key: cli_key.as_str(),
                    method: method_hint.as_str(),
                    path: forwarded_path.as_str(),
                    observe: ctx.observe,
                    query: query.as_deref(),
                    excluded_from_stats: false,
                    duration_ms,
                    attempts: attempts.as_slice(),
                    special_settings_json,
                    session_id,
                    requested_model,
                    created_at_ms,
                    created_at,
                })
                .with_completion(RequestCompletion::failure_with_ttfb(
                    response_status.as_u16(),
                    Some(category.as_str()),
                    error_code,
                    duration_ms,
                )),
            )
            .await;

            abort_guard.disarm();

            let mut response_headers = response_headers;
            strip_hop_headers(&mut response_headers);
            let should_gunzip = has_gzip_content_encoding(&response_headers);
            if should_gunzip {
                // 上游可能无视 accept-encoding: identity 返回 gzip；
                response_headers.remove(header::CONTENT_ENCODING);
                response_headers.remove(header::CONTENT_LENGTH);
            }

            let Some(resp) = resp else {
                let client_attempts = if ctx.verbose_provider_error {
                    attempts.clone()
                } else {
                    vec![]
                };
                return LoopControl::Return(error_response(
                    axum::http::StatusCode::BAD_GATEWAY,
                    trace_id.clone(),
                    GatewayErrorCode::UpstreamReadError.as_str(),
                    "failed to stream upstream error body".to_string(),
                    client_attempts,
                ));
            };
            let body = if should_gunzip {
                let upstream = GunzipStream::new(resp.bytes_stream());
                Body::from_stream(upstream)
            } else {
                Body::from_stream(resp.bytes_stream())
            };

            LoopControl::Return(build_response(
                response_status,
                &response_headers,
                trace_id.as_str(),
                body,
            ))
        }
    }
}

pub(super) async fn handle_reqwest_error<R: tauri::Runtime>(
    ctx: CommonCtx<'_, R>,
    provider_ctx: ProviderCtx<'_>,
    attempt_ctx: AttemptCtx<'_>,
    loop_state: LoopState<'_, R>,
    err: reqwest::Error,
) -> LoopControl {
    tracing::warn!(
        trace_id = %ctx.trace_id,
        cli_key = %ctx.cli_key,
        provider_id = provider_ctx.provider_id,
        provider_name = %provider_ctx.provider_name_base,
        base_url = %provider_ctx.provider_base_url_base,
        is_connect = err.is_connect(),
        is_timeout = err.is_timeout(),
        is_request = err.is_request(),
        "reqwest upstream error: {err}"
    );
    let is_count_tokens =
        is_claude_count_tokens_request(ctx.cli_key.as_str(), ctx.forwarded_path.as_str());
    let is_connect = err.is_connect();
    let (_, error_code) = classify_reqwest_error(&err);
    let decision = reqwest_error_decision(
        is_count_tokens,
        is_connect,
        attempt_ctx.retry_index,
        attempt_ctx.provider_max_attempts,
    );
    let outcome = format!(
        "request_error: category={} code={} decision={} err={err}",
        ErrorCategory::SystemError.as_str(),
        error_code,
        decision.as_str(),
    );
    let reason = if is_connect {
        "reqwest connect error"
    } else {
        "reqwest error"
    };

    if is_count_tokens {
        return record_system_failure_and_decide_no_cooldown(RecordSystemFailureArgs {
            ctx,
            provider_ctx,
            attempt_ctx,
            loop_state,
            status: None,
            error_code,
            decision,
            outcome,
            reason: reason.to_string(),
            timeout_secs: None,
        })
        .await;
    }

    record_system_failure_and_decide(RecordSystemFailureArgs {
        ctx,
        provider_ctx,
        attempt_ctx,
        loop_state,
        status: None,
        error_code,
        decision,
        outcome,
        reason: reason.to_string(),
        timeout_secs: None,
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::{
        convert_codex_agent_messages_to_user_messages, error_body_scan_limit_usize,
        matches_claude_code_client_restriction, matches_codex_additional_tools_error,
        matches_codex_agent_message_error, matches_codex_previous_response_id_error,
        matches_codex_reasoning_context_error, maybe_rectify_codex_additional_tools,
        maybe_rectify_codex_agent_messages, maybe_rectify_codex_reasoning_context,
        read_response_body_for_error_scan, remove_codex_additional_tools_input_items,
        remove_codex_previous_response_id, remove_codex_reasoning_context, reqwest_error_decision,
        retry_after_reset_at, should_scan_codex_previous_response_id_error,
        upstream_error_decision, FailoverDecision,
    };
    use axum::body::Bytes;
    use axum::http::{header, HeaderMap, HeaderValue};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn known_length_response(
        body: Vec<u8>,
    ) -> (reqwest::Response, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test upstream");
        let addr = listener.local_addr().expect("local addr");
        let task = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request_buf = [0u8; 1024];
            let _ = socket.read(&mut request_buf).await;
            let headers = format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(headers.as_bytes()).await;
            let _ = socket.write_all(&body).await;
        });
        let response = reqwest::Client::new()
            .get(format!("http://{addr}/error"))
            .send()
            .await
            .expect("fetch test response");
        (response, task)
    }

    async fn unknown_length_response(
        chunks: Vec<Vec<u8>>,
    ) -> (reqwest::Response, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test upstream");
        let addr = listener.local_addr().expect("local addr");
        let task = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request_buf = [0u8; 1024];
            let _ = socket.read(&mut request_buf).await;
            let _ = socket
                .write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await;
            for chunk in chunks {
                let _ = socket
                    .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                    .await;
                let _ = socket.write_all(&chunk).await;
                let _ = socket.write_all(b"\r\n").await;
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
        });
        let response = reqwest::Client::new()
            .get(format!("http://{addr}/error"))
            .send()
            .await
            .expect("fetch test response");
        (response, task)
    }

    #[test]
    fn upstream_error_decision_aborts_for_count_tokens() {
        let decision = upstream_error_decision(true, FailoverDecision::RetrySameProvider, 1, 5);
        assert!(matches!(decision, FailoverDecision::Abort));
    }

    #[test]
    fn upstream_error_decision_keeps_base_decision_before_retry_limit() {
        let decision = upstream_error_decision(false, FailoverDecision::RetrySameProvider, 1, 5);
        assert!(matches!(decision, FailoverDecision::RetrySameProvider));
    }

    #[test]
    fn upstream_error_decision_switches_after_retry_limit() {
        let decision = upstream_error_decision(false, FailoverDecision::RetrySameProvider, 5, 5);
        assert!(matches!(decision, FailoverDecision::SwitchProvider));
    }

    #[test]
    fn upstream_error_decision_keeps_switch_and_abort_decisions() {
        let switch_decision =
            upstream_error_decision(false, FailoverDecision::SwitchProvider, 1, 5);
        assert!(matches!(switch_decision, FailoverDecision::SwitchProvider));

        let abort_decision = upstream_error_decision(false, FailoverDecision::Abort, 1, 5);
        assert!(matches!(abort_decision, FailoverDecision::Abort));
    }

    #[test]
    fn claude_client_restriction_match_is_status_and_cli_specific() {
        let body = br#"{"error":{"message":"No available accounts: this group only allows Claude Code clients","type":"api_error"},"type":"error"}"#;

        assert!(matches_claude_code_client_restriction(
            "claude",
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            body,
        ));
        assert!(!matches_claude_code_client_restriction(
            "codex",
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            body,
        ));
        assert!(!matches_claude_code_client_restriction(
            "claude",
            reqwest::StatusCode::BAD_GATEWAY,
            body,
        ));
        assert!(!matches_claude_code_client_restriction(
            "claude",
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            br#"{"error":{"message":"No available accounts"}}"#,
        ));
    }

    #[test]
    fn claude_client_restriction_match_normalizes_case_and_whitespace() {
        assert!(matches_claude_code_client_restriction(
            "claude",
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            b"upstream: THIS GROUP only allows\nClaude Code clients",
        ));
    }

    #[tokio::test]
    async fn error_body_read_reports_truncation_for_known_oversized_body() {
        let limit = error_body_scan_limit_usize();
        let payload = vec![b'x'; limit + 4096];
        let (response, server_task) = known_length_response(payload).await;

        assert_eq!(response.content_length(), Some((limit + 4096) as u64));
        let result = read_response_body_for_error_scan(response)
            .await
            .expect("read limited body");
        server_task.abort();

        assert_eq!(result.body.len(), limit);
        assert!(result.body.iter().all(|byte| *byte == b'x'));
        assert!(result.truncated);
    }

    #[tokio::test]
    async fn error_body_read_reports_truncation_for_unknown_oversized_body() {
        let limit = error_body_scan_limit_usize();
        let (response, server_task) =
            unknown_length_response(vec![vec![b'x'; limit], vec![b'y']]).await;

        assert_eq!(response.content_length(), None);
        let result = read_response_body_for_error_scan(response)
            .await
            .expect("read limited body");
        server_task.abort();

        assert_eq!(result.body.len(), limit);
        assert!(result.body.iter().all(|byte| *byte == b'x'));
        assert!(result.truncated);
    }

    #[tokio::test]
    async fn error_body_read_reports_truncation_false_at_exact_limit() {
        let limit = error_body_scan_limit_usize();
        let (response, server_task) = unknown_length_response(vec![vec![b'x'; limit]]).await;

        let result = read_response_body_for_error_scan(response)
            .await
            .expect("read limited body");
        server_task.abort();

        assert_eq!(result.body.len(), limit);
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn error_body_read_reports_truncation_false_below_limit() {
        let limit = error_body_scan_limit_usize();
        let (response, server_task) = known_length_response(vec![b'x'; limit - 1]).await;

        let result = read_response_body_for_error_scan(response)
            .await
            .expect("read limited body");
        server_task.abort();

        assert_eq!(result.body.len(), limit - 1);
        assert!(!result.truncated);
    }

    #[test]
    fn matches_exact_reasoning_context_unsupported_field() {
        let body = br#"{"error":{"message":"unknown field in strict mode: 'reasoning.context'","type":"invalid_request_error","param":"reasoning.context","code":"unsupported_field"}}"#;

        assert!(matches_codex_reasoning_context_error(
            reqwest::StatusCode::BAD_REQUEST,
            body,
        ));
    }

    #[test]
    fn matches_exact_reasoning_context_unsupported_field_rejects_near_misses() {
        let exact = br#"{"error":{"param":"reasoning.context","code":"unsupported_field"}}"#;
        assert!(!matches_codex_reasoning_context_error(
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
            exact,
        ));

        for body in [
            br#"{"code":"unsupported_field","param":"reasoning.context"}"#.as_slice(),
            br#"{"response":{"error":{"code":"unsupported_field","param":"reasoning.context"}}}"#
                .as_slice(),
            br#"{"error":{"code":"invalid_field","param":"reasoning.context"}}"#.as_slice(),
            br#"{"error":{"code":"unsupported_field","param":"reasoning.effort"}}"#.as_slice(),
            br#"{"error":{"message":"unknown field in strict mode: 'reasoning.context'"}}"#
                .as_slice(),
            b"not-json".as_slice(),
            b"".as_slice(),
        ] {
            assert!(!matches_codex_reasoning_context_error(
                reqwest::StatusCode::BAD_REQUEST,
                body,
            ));
        }
    }

    #[test]
    fn removes_only_reasoning_context_and_preserves_payload() {
        let original = serde_json::json!({
            "model": "LongCat-2.0",
            "reasoning": {"effort": "low", "context": "all_turns"},
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "KEEP"}]}],
            "tools": [{"type": "function", "name": "lookup"}],
            "store": false
        });
        let mut body = Bytes::from(serde_json::to_vec(&original).expect("serialize request"));

        assert!(remove_codex_reasoning_context(&mut body));
        let next: serde_json::Value = serde_json::from_slice(&body).expect("parse rectified body");
        assert_eq!(next["reasoning"], serde_json::json!({"effort": "low"}));
        assert_eq!(next["input"], original["input"]);
        assert_eq!(next["tools"], original["tools"]);
        assert_eq!(next["model"], original["model"]);
        assert_eq!(next["store"], original["store"]);
        assert!(!remove_codex_reasoning_context(&mut body));
    }

    #[test]
    fn removes_only_reasoning_context_drops_empty_reasoning_object() {
        let mut body =
            Bytes::from_static(br#"{"model":"LongCat-2.0","reasoning":{"context":"all_turns"}}"#);

        assert!(remove_codex_reasoning_context(&mut body));
        let next: serde_json::Value = serde_json::from_slice(&body).expect("parse rectified body");
        assert_eq!(next, serde_json::json!({"model": "LongCat-2.0"}));
    }

    #[test]
    fn removes_only_reasoning_context_rejects_incompatible_shapes() {
        for original in [
            b"not-json".as_slice(),
            br#"[]"#.as_slice(),
            br#"{}"#.as_slice(),
            br#"{"reasoning":null}"#.as_slice(),
            br#"{"reasoning":"all_turns"}"#.as_slice(),
            br#"{"reasoning":{"effort":"low"}}"#.as_slice(),
        ] {
            let mut body = Bytes::copy_from_slice(original);
            assert!(!remove_codex_reasoning_context(&mut body));
            assert_eq!(body.as_ref(), original);
        }
    }

    #[test]
    fn matches_exact_additional_tools_invalid_request() {
        let body = br#"{"error":{"message":"unsupported input item type: additional_tools","type":"invalid_request_error","param":"input[0].type","code":"invalid_request"}}"#;

        assert!(matches_codex_additional_tools_error(
            reqwest::StatusCode::BAD_REQUEST,
            body,
        ));
    }

    #[test]
    fn matches_exact_additional_tools_invalid_request_rejects_near_misses() {
        let exact = br#"{"error":{"message":"unsupported input item type: additional_tools","type":"invalid_request_error","param":"input[12].type","code":"invalid_request"}}"#;
        assert!(!matches_codex_additional_tools_error(
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
            exact,
        ));

        for body in [
            br#"{"message":"unsupported input item type: additional_tools","type":"invalid_request_error","param":"input[0].type","code":"invalid_request"}"#.as_slice(),
            br#"{"error":{"message":"unsupported input item type: custom_tool_call","type":"invalid_request_error","param":"input[0].type","code":"invalid_request"}}"#.as_slice(),
            br#"{"error":{"message":"unsupported input item type: additional_tools","type":"invalid_request_error","param":"input[].type","code":"invalid_request"}}"#.as_slice(),
            br#"{"error":{"message":"unsupported input item type: additional_tools","type":"invalid_request_error","param":"input[0].content","code":"invalid_request"}}"#.as_slice(),
            br#"{"error":{"message":"unsupported input item type: additional_tools","type":"invalid_request_error","param":"input[0].type","code":"unsupported_field"}}"#.as_slice(),
            b"not-json".as_slice(),
        ] {
            assert!(!matches_codex_additional_tools_error(
                reqwest::StatusCode::BAD_REQUEST,
                body,
            ));
        }
    }

    #[test]
    fn removes_only_additional_tools_input_items_and_preserves_payload() {
        let original = serde_json::json!({
            "model": "LongCat-2.0",
            "reasoning": {"effort": "low"},
            "input": [
                {"type": "additional_tools", "tools": [{"name": "spawn_agent"}]},
                {"role": "user", "content": [{"type": "input_text", "text": "KEEP"}]},
                {"type": "additional_tools", "tools": [{"name": "wait_agent"}]},
                {"type": "function_call_output", "call_id": "call-1", "output": "KEEP"}
            ],
            "tools": [{"type": "function", "name": "lookup"}],
            "store": false
        });
        let mut body = Bytes::from(serde_json::to_vec(&original).expect("serialize request"));

        assert_eq!(
            remove_codex_additional_tools_input_items(&mut body),
            Some(2)
        );
        let next: serde_json::Value = serde_json::from_slice(&body).expect("parse rectified body");
        assert_eq!(
            next["input"],
            serde_json::json!([
                {"role": "user", "content": [{"type": "input_text", "text": "KEEP"}]},
                {"type": "function_call_output", "call_id": "call-1", "output": "KEEP"}
            ])
        );
        assert_eq!(next["reasoning"], original["reasoning"]);
        assert_eq!(next["tools"], original["tools"]);
        assert_eq!(next["model"], original["model"]);
        assert_eq!(next["store"], original["store"]);
        assert_eq!(remove_codex_additional_tools_input_items(&mut body), None);
    }

    #[test]
    fn additional_tools_rectifier_retries_once() {
        let error = br#"{"error":{"message":"unsupported input item type: additional_tools","type":"invalid_request_error","param":"input[0].type","code":"invalid_request"}}"#;
        let original = Bytes::from_static(
            br#"{"model":"LongCat-2.0","input":[{"type":"additional_tools","tools":[]},{"role":"user","content":"KEEP"}]}"#,
        );
        let mut body = original.clone();
        let mut already_retried = false;

        assert_eq!(
            maybe_rectify_codex_additional_tools(
                "codex",
                "/v1/responses",
                reqwest::StatusCode::BAD_REQUEST,
                error,
                false,
                &mut already_retried,
                &mut body,
            ),
            Some(1)
        );
        assert!(already_retried);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).expect("parse rectified request"),
            serde_json::json!({
                "model": "LongCat-2.0",
                "input": [{"role": "user", "content": "KEEP"}]
            })
        );

        let mut second_body = original.clone();
        assert_eq!(
            maybe_rectify_codex_additional_tools(
                "codex",
                "/v1/responses",
                reqwest::StatusCode::BAD_REQUEST,
                error,
                false,
                &mut already_retried,
                &mut second_body,
            ),
            None
        );
        assert_eq!(second_body, original);
    }

    #[test]
    fn matches_exact_agent_message_invalid_request() {
        let body = br#"{"error":{"message":"unsupported input item type: agent_message","type":"invalid_request_error","param":"input[219].type","code":"invalid_request"}}"#;

        assert!(matches_codex_agent_message_error(
            reqwest::StatusCode::BAD_REQUEST,
            body,
        ));
    }

    #[test]
    fn matches_exact_agent_message_invalid_request_rejects_near_misses() {
        let exact = br#"{"error":{"message":"unsupported input item type: agent_message","type":"invalid_request_error","param":"input[0].type","code":"invalid_request"}}"#;
        assert!(!matches_codex_agent_message_error(
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
            exact,
        ));

        for body in [
            br#"{"message":"unsupported input item type: agent_message","type":"invalid_request_error","param":"input[0].type","code":"invalid_request"}"#.as_slice(),
            br#"{"error":{"message":"unsupported input item type: message","type":"invalid_request_error","param":"input[0].type","code":"invalid_request"}}"#.as_slice(),
            br#"{"error":{"message":"unsupported input item type: agent_message","type":"invalid_request_error","param":"input[x].type","code":"invalid_request"}}"#.as_slice(),
            br#"{"error":{"message":"unsupported input item type: agent_message","type":"invalid_request_error","param":"input[0].content","code":"invalid_request"}}"#.as_slice(),
            br#"{"error":{"message":"unsupported input item type: agent_message","type":"invalid_request_error","param":"input[0].type","code":"unsupported_field"}}"#.as_slice(),
            b"not-json".as_slice(),
        ] {
            assert!(!matches_codex_agent_message_error(
                reqwest::StatusCode::BAD_REQUEST,
                body,
            ));
        }
    }

    #[test]
    fn converts_agent_messages_to_user_messages_and_preserves_content() {
        let original = serde_json::json!({
            "model": "LongCat-2.0",
            "input": [
                {
                    "type": "agent_message",
                    "author": "/root/reviewer",
                    "recipient": "/root",
                    "content": [{"type": "input_text", "text": "KEEP_REVIEW"}]
                },
                {"type": "function_call_output", "call_id": "call-1", "output": "KEEP_TOOL"},
                {
                    "type": "agent_message",
                    "author": "/root/worker",
                    "recipient": "/root",
                    "content": [{"type": "input_text", "text": "KEEP_RESULT"}]
                }
            ],
            "store": false
        });
        let mut body = Bytes::from(serde_json::to_vec(&original).expect("serialize request"));

        let outcome = convert_codex_agent_messages_to_user_messages(&mut body)
            .expect("rectification outcome");
        assert_eq!(outcome.items_converted, 2);
        assert_eq!(outcome.encrypted_content_parts_removed, 0);
        assert_eq!(outcome.empty_items_removed, 0);
        let next: serde_json::Value = serde_json::from_slice(&body).expect("parse rectified body");
        assert_eq!(
            next["input"],
            serde_json::json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "KEEP_REVIEW"}]},
                {"type": "function_call_output", "call_id": "call-1", "output": "KEEP_TOOL"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "KEEP_RESULT"}]}
            ])
        );
        assert_eq!(next["model"], original["model"]);
        assert_eq!(next["store"], original["store"]);
        assert_eq!(
            convert_codex_agent_messages_to_user_messages(&mut body),
            None
        );
    }

    #[test]
    fn agent_message_rectifier_removes_encrypted_content_and_preserves_portable_parts() {
        let mut body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "LongCat-2.0",
                "input": [{
                    "type": "agent_message",
                    "content": [
                        {"type": "input_text", "text": "KEEP_BEFORE"},
                        {"type": "encrypted_content", "data": "DROP_ONE"},
                        {"type": "output_text", "text": "KEEP_AFTER"},
                        {"type": "encrypted_content", "data": "DROP_TWO"}
                    ]
                }]
            }))
            .expect("serialize request"),
        );

        let outcome = convert_codex_agent_messages_to_user_messages(&mut body)
            .expect("rectification outcome");
        assert_eq!(outcome.items_converted, 1);
        assert_eq!(outcome.encrypted_content_parts_removed, 2);
        assert_eq!(outcome.empty_items_removed, 0);
        let next: serde_json::Value = serde_json::from_slice(&body).expect("parse rectified body");
        assert_eq!(
            next["input"],
            serde_json::json!([{
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "KEEP_BEFORE"},
                    {"type": "output_text", "text": "KEEP_AFTER"}
                ]
            }])
        );
        let serialized = serde_json::to_string(&next).expect("serialize rectified body");
        assert!(!serialized.contains("DROP_ONE"));
        assert!(!serialized.contains("DROP_TWO"));
    }

    #[test]
    fn agent_message_rectifier_removes_encrypted_only_item() {
        let mut body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "LongCat-2.0",
                "input": [
                    {"type": "message", "role": "user", "content": "KEEP_BEFORE"},
                    {
                        "type": "agent_message",
                        "content": [{"type": "encrypted_content", "data": "DROP_ONLY"}]
                    },
                    {"type": "function_call_output", "call_id": "call-1", "output": "KEEP_AFTER"}
                ]
            }))
            .expect("serialize request"),
        );

        let outcome = convert_codex_agent_messages_to_user_messages(&mut body)
            .expect("rectification outcome");
        assert_eq!(outcome.items_converted, 0);
        assert_eq!(outcome.encrypted_content_parts_removed, 1);
        assert_eq!(outcome.empty_items_removed, 1);
        let next: serde_json::Value = serde_json::from_slice(&body).expect("parse rectified body");
        assert_eq!(
            next["input"],
            serde_json::json!([
                {"type": "message", "role": "user", "content": "KEEP_BEFORE"},
                {"type": "function_call_output", "call_id": "call-1", "output": "KEEP_AFTER"}
            ])
        );
        assert!(!serde_json::to_string(&next)
            .expect("serialize rectified body")
            .contains("DROP_ONLY"));
    }

    #[test]
    fn agent_message_rectifier_reports_structured_counts_for_mixed_input() {
        let non_agent_item = serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "encrypted_content", "data": "KEEP_NON_AGENT"}]
        });
        let mut body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "model": "LongCat-2.0",
                "input": [
                    {
                        "type": "agent_message",
                        "content": [
                            {"type": "input_text", "text": "KEEP_PORTABLE"},
                            {"type": "encrypted_content", "data": "DROP_ONE"},
                            {"type": "encrypted_content", "data": "DROP_TWO"}
                        ]
                    },
                    {
                        "type": "agent_message",
                        "content": [{"type": "encrypted_content", "data": "DROP_ONLY"}]
                    },
                    non_agent_item.clone()
                ]
            }))
            .expect("serialize request"),
        );

        let outcome = convert_codex_agent_messages_to_user_messages(&mut body)
            .expect("rectification outcome");
        assert_eq!(outcome.items_converted, 1);
        assert_eq!(outcome.encrypted_content_parts_removed, 3);
        assert_eq!(outcome.empty_items_removed, 1);
        assert!(outcome.changed());

        let next: serde_json::Value = serde_json::from_slice(&body).expect("parse rectified body");
        assert_eq!(
            next["input"],
            serde_json::json!([
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "KEEP_PORTABLE"}]
                },
                non_agent_item
            ])
        );
    }

    #[test]
    fn agent_message_rectifier_preserves_out_of_scope_shapes() {
        let original = serde_json::json!({
            "input": [
                {"type": "agent_message", "content": "not-an-array"},
                {"type": "agent_message", "author": "/root/worker"},
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "encrypted_content", "data": "KEEP_NON_AGENT"}]
                }
            ]
        });
        let mut body = Bytes::from(serde_json::to_vec(&original).expect("serialize request"));

        assert_eq!(
            convert_codex_agent_messages_to_user_messages(&mut body),
            None
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).expect("parse preserved body"),
            original
        );
    }

    #[test]
    fn agent_message_encrypted_only_rectifier_schedules_retry() {
        let error = br#"{"error":{"message":"unsupported input item type: agent_message","type":"invalid_request_error","param":"input[0].type","code":"invalid_request"}}"#;
        let mut body = Bytes::from_static(
            br#"{"model":"LongCat-2.0","input":[{"type":"agent_message","content":[{"type":"encrypted_content","data":"DROP_ONLY"}]}]}"#,
        );
        let mut already_retried = false;
        let mut retry_pending = false;
        let mut strip_request_content_encoding = false;

        let (control, outcome) = super::maybe_prepare_codex_agent_message_retry(
            "codex",
            "/v1/responses",
            reqwest::StatusCode::BAD_REQUEST,
            error,
            false,
            &mut already_retried,
            &mut retry_pending,
            &mut strip_request_content_encoding,
            &mut body,
        )
        .expect("retry preparation");

        assert!(already_retried);
        assert!(retry_pending);
        assert!(strip_request_content_encoding);
        assert!(matches!(control, super::LoopControl::ContinueRetry));
        assert_eq!(outcome.items_converted, 0);
        assert_eq!(outcome.encrypted_content_parts_removed, 1);
        assert_eq!(outcome.empty_items_removed, 1);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).expect("parse rectified body"),
            serde_json::json!({"model": "LongCat-2.0", "input": []})
        );

        let second_original = Bytes::from_static(
            br#"{"model":"LongCat-2.0","input":[{"type":"agent_message","content":[{"type":"input_text","text":"KEEP_SECOND"}]}]}"#,
        );
        let mut second_body = second_original.clone();
        retry_pending = false;
        strip_request_content_encoding = false;
        assert!(super::maybe_prepare_codex_agent_message_retry(
            "codex",
            "/v1/responses",
            reqwest::StatusCode::BAD_REQUEST,
            error,
            false,
            &mut already_retried,
            &mut retry_pending,
            &mut strip_request_content_encoding,
            &mut second_body,
        )
        .is_none());
        assert!(!retry_pending);
        assert!(!strip_request_content_encoding);
        assert_eq!(second_body, second_original);
    }

    #[test]
    fn codex_agent_message_rectifier_special_setting_has_approved_shape() {
        let setting = super::codex_agent_message_rectifier_special_setting(
            30,
            reqwest::StatusCode::BAD_REQUEST,
            2,
            super::CodexAgentMessageRectificationOutcome {
                items_converted: 1,
                encrypted_content_parts_removed: 3,
                empty_items_removed: 1,
            },
        );
        let object = setting.as_object().expect("special setting object");
        let mut keys = object.keys().map(String::as_str).collect::<Vec<_>>();
        keys.sort_unstable();
        let mut expected = vec![
            "action",
            "emptyItemsRemoved",
            "encryptedContentPartsRemoved",
            "hit",
            "itemsConverted",
            "providerId",
            "retryAttemptNumber",
            "retryAttemptNumberNext",
            "scope",
            "status",
            "type",
        ];
        expected.sort_unstable();
        assert_eq!(keys, expected);
        for key in [
            "itemsConverted",
            "encryptedContentPartsRemoved",
            "emptyItemsRemoved",
        ] {
            assert!(
                setting[key].as_u64().is_some(),
                "{key} must be non-negative"
            );
        }
        for forbidden in ["body", "content", "text", "data", "payload", "request"] {
            assert!(!object.contains_key(forbidden));
        }
        let serialized = serde_json::to_string(&setting).expect("serialize special setting");
        for sentinel in ["KEEP_BEFORE", "KEEP_AFTER", "DROP_ONE", "DROP_TWO"] {
            assert!(!serialized.contains(sentinel));
        }
    }

    #[test]
    fn agent_message_rectifier_retries_once() {
        let error = br#"{"error":{"message":"unsupported input item type: agent_message","type":"invalid_request_error","param":"input[0].type","code":"invalid_request"}}"#;
        let original = Bytes::from_static(
            br#"{"model":"LongCat-2.0","input":[{"type":"agent_message","author":"/root/worker","recipient":"/root","content":[{"type":"input_text","text":"KEEP"}]}]}"#,
        );
        let mut body = original.clone();
        let mut already_retried = false;

        let outcome = maybe_rectify_codex_agent_messages(
            "codex",
            "/v1/responses",
            reqwest::StatusCode::BAD_REQUEST,
            error,
            false,
            &mut already_retried,
            &mut body,
        )
        .expect("rectification outcome");
        assert_eq!(outcome.items_converted, 1);
        assert_eq!(outcome.encrypted_content_parts_removed, 0);
        assert_eq!(outcome.empty_items_removed, 0);
        assert!(already_retried);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).expect("parse rectified request"),
            serde_json::json!({
                "model": "LongCat-2.0",
                "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "KEEP"}]}]
            })
        );

        let mut second_body = original.clone();
        assert_eq!(
            maybe_rectify_codex_agent_messages(
                "codex",
                "/v1/responses",
                reqwest::StatusCode::BAD_REQUEST,
                error,
                false,
                &mut already_retried,
                &mut second_body,
            ),
            None
        );
        assert_eq!(second_body, original);
    }

    #[test]
    fn reasoning_context_rectifier_retries_once() {
        let error = br#"{"error":{"code":"unsupported_field","param":"reasoning.context"}}"#;
        let original = Bytes::from_static(
            br#"{"model":"LongCat-2.0","reasoning":{"effort":"low","context":"all_turns"}}"#,
        );
        let mut body = original.clone();
        let mut already_retried = false;

        let first = maybe_rectify_codex_reasoning_context(
            "codex",
            "/v1/responses",
            reqwest::StatusCode::BAD_REQUEST,
            error,
            false,
            &mut already_retried,
            &mut body,
        );
        assert!(matches!(first, Some(super::LoopControl::ContinueRetry)));
        assert!(already_retried);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).expect("parse rectified request"),
            serde_json::json!({
                "model": "LongCat-2.0",
                "reasoning": {"effort": "low"}
            })
        );

        let mut second_body = original.clone();
        let second = maybe_rectify_codex_reasoning_context(
            "codex",
            "/v1/responses",
            reqwest::StatusCode::BAD_REQUEST,
            error,
            false,
            &mut already_retried,
            &mut second_body,
        );
        assert!(second.is_none());
        assert_eq!(second_body, original);

        let mut missing_field_body = Bytes::from_static(br#"{"model":"LongCat-2.0"}"#);
        let mut missing_field_retried = false;
        let missing_field = maybe_rectify_codex_reasoning_context(
            "codex",
            "/v1/responses",
            reqwest::StatusCode::BAD_REQUEST,
            error,
            false,
            &mut missing_field_retried,
            &mut missing_field_body,
        );
        assert!(missing_field.is_none());
        assert!(!missing_field_retried);

        let mut truncated_body = original.clone();
        let mut truncated_retried = false;
        let truncated = maybe_rectify_codex_reasoning_context(
            "codex",
            "/v1/responses",
            reqwest::StatusCode::BAD_REQUEST,
            error,
            true,
            &mut truncated_retried,
            &mut truncated_body,
        );
        assert!(truncated.is_none());
        assert!(!truncated_retried);
        assert_eq!(truncated_body, original);
    }

    #[test]
    fn retry_after_reset_at_accepts_delta_seconds() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("120"));

        assert_eq!(retry_after_reset_at(&headers, 1_000), Some(1_120));
    }

    #[test]
    fn retry_after_reset_at_accepts_http_date() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );

        assert_eq!(retry_after_reset_at(&headers, 1_000), Some(1_445_412_480));
    }

    #[test]
    fn codex_previous_response_id_scan_requires_codex_400_or_404_with_previous_response_id() {
        let body = br#"{"previous_response_id":"resp_old"}"#;

        assert!(should_scan_codex_previous_response_id_error(
            "codex",
            reqwest::StatusCode::BAD_REQUEST,
            false,
            body,
        ));
        assert!(should_scan_codex_previous_response_id_error(
            "codex",
            reqwest::StatusCode::NOT_FOUND,
            false,
            body,
        ));
        assert!(should_scan_codex_previous_response_id_error(
            "grok",
            reqwest::StatusCode::BAD_REQUEST,
            false,
            body,
        ));
        assert!(!should_scan_codex_previous_response_id_error(
            "grok",
            reqwest::StatusCode::BAD_REQUEST,
            true,
            body,
        ));
        assert!(!should_scan_codex_previous_response_id_error(
            "grok",
            reqwest::StatusCode::BAD_REQUEST,
            false,
            br#"{"model":"grok-build"}"#,
        ));
        assert!(!should_scan_codex_previous_response_id_error(
            "claude",
            reqwest::StatusCode::BAD_REQUEST,
            false,
            body,
        ));
        assert!(!should_scan_codex_previous_response_id_error(
            "codex",
            reqwest::StatusCode::BAD_REQUEST,
            true,
            body,
        ));
        assert!(!should_scan_codex_previous_response_id_error(
            "codex",
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            false,
            body,
        ));
    }

    #[test]
    fn codex_previous_response_id_error_match_is_specific_to_missing_previous_response() {
        assert!(matches_codex_previous_response_id_error(
            reqwest::StatusCode::BAD_REQUEST,
            br#"{"error":{"message":"No response found for previous_response_id resp_old"}}"#,
        ));
        assert!(matches_codex_previous_response_id_error(
            reqwest::StatusCode::BAD_REQUEST,
            br#"{"error":{"message":"No response found with id 'resp_old'","param":"previous_response_id"}}"#,
        ));
        assert!(matches_codex_previous_response_id_error(
            reqwest::StatusCode::NOT_FOUND,
            br#"Previous response id does not exist"#,
        ));
        assert!(!matches_codex_previous_response_id_error(
            reqwest::StatusCode::BAD_REQUEST,
            br#"{"error":{"message":"model is required"}}"#,
        ));
    }

    #[test]
    fn remove_codex_previous_response_id_keeps_other_body_fields() {
        let mut body = Bytes::from_static(
            br#"{"model":"gpt-5","previous_response_id":"resp_old","input":"hello"}"#,
        );

        assert!(remove_codex_previous_response_id(&mut body));
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");

        assert_eq!(json.get("previous_response_id"), None);
        assert_eq!(json["model"], "gpt-5");
        assert_eq!(json["input"], "hello");
    }

    #[test]
    fn reqwest_error_decision_aborts_count_tokens_even_for_connect_errors() {
        let decision = reqwest_error_decision(true, true, 1, 5);
        assert!(matches!(decision, FailoverDecision::Abort));
    }

    #[test]
    fn reqwest_error_decision_retries_connect_errors_before_limit() {
        let decision = reqwest_error_decision(false, true, 1, 5);
        assert!(matches!(decision, FailoverDecision::RetrySameProvider));
    }

    #[test]
    fn reqwest_error_decision_retries_non_connect_errors_before_limit() {
        let decision = reqwest_error_decision(false, false, 1, 5);
        assert!(matches!(decision, FailoverDecision::RetrySameProvider));
    }

    #[test]
    fn reqwest_error_decision_switches_non_connect_errors_at_limit() {
        let decision = reqwest_error_decision(false, false, 5, 5);
        assert!(matches!(decision, FailoverDecision::SwitchProvider));
    }
}
