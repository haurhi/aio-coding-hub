//! Usage: Codex (OpenAI / ChatGPT) OAuth adapter.

use crate::gateway::oauth::provider_trait::*;
use crate::gateway::upstream_identity;
use crate::shared::http_body::read_text_with_limit;
use axum::http::{HeaderMap, HeaderValue};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use std::future::Future;
use std::pin::Pin;

pub(crate) struct CodexOAuthProvider {
    endpoints: OAuthEndpoints,
}

const CODEX_LIMITS_RESPONSE_BODY_LIMIT: usize = 1024 * 1024;

impl CodexOAuthProvider {
    pub(crate) fn new() -> Self {
        Self {
            endpoints: OAuthEndpoints {
                auth_url: "https://auth.openai.com/oauth/authorize",
                token_url: "https://auth.openai.com/oauth/token",
                client_id: "app_EMoamEEZ73f0CkXaXp7hrann".to_string(),
                client_secret: None,
                scopes: vec![
                    "openid",
                    "profile",
                    "email",
                    "offline_access",
                    "api.connectors.read",
                    "api.connectors.invoke",
                ],
                redirect_host: "localhost",
                callback_path: "/auth/callback",
                default_callback_port: 1455,
            },
        }
    }
}

impl OAuthProvider for CodexOAuthProvider {
    fn cli_key(&self) -> &'static str {
        "codex"
    }

    fn provider_type(&self) -> &'static str {
        "codex_oauth"
    }

    fn endpoints(&self) -> &OAuthEndpoints {
        &self.endpoints
    }

    fn default_base_url(&self) -> &'static str {
        "https://chatgpt.com/backend-api/codex"
    }

    fn extra_authorize_params(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", upstream_identity::CODEX_CLI_ORIGINATOR),
        ]
    }

    fn resolve_effective_token(
        &self,
        token_set: &OAuthTokenSet,
        stored_id_token: Option<&str>,
    ) -> (String, Option<String>) {
        // Store the raw access_token as the effective token (used for Bearer auth and limits queries).
        // The id_token is stored separately for extracting chatgpt-account-id header.
        let id_token = token_set
            .id_token
            .as_deref()
            .or(stored_id_token)
            .filter(|v| !v.trim().is_empty())
            .map(str::to_string);
        (token_set.access_token.clone(), id_token)
    }

    fn inject_upstream_headers(
        &self,
        headers: &mut HeaderMap,
        access_token: &str,
    ) -> Result<(), String> {
        insert_bearer_auth(headers, access_token, "codex oauth")?;
        headers.insert(
            "originator",
            HeaderValue::from_static(upstream_identity::CODEX_CLI_ORIGINATOR),
        );
        Ok(())
    }

    fn inject_model_discovery_headers(
        &self,
        headers: &mut HeaderMap,
        access_token: &str,
        id_token: Option<&str>,
    ) -> Result<(), String> {
        self.inject_upstream_headers(headers, access_token)?;
        headers.insert(
            axum::http::header::USER_AGENT,
            HeaderValue::from_static(upstream_identity::CODEX_CLI_USER_AGENT),
        );
        if let Some(account_id) = parse_chatgpt_account_id(id_token) {
            if let Ok(value) = HeaderValue::from_str(&account_id) {
                headers.insert("chatgpt-account-id", value);
            }
        }
        Ok(())
    }

    fn fetch_limits(
        &self,
        client: &reqwest::Client,
        access_token: &str,
    ) -> Pin<Box<dyn Future<Output = Result<OAuthLimitsResult, String>> + Send + '_>> {
        let token = access_token.to_string();
        let client = client.clone();
        Box::pin(async move {
            let resp = client
                .get("https://chatgpt.com/backend-api/wham/usage")
                .header("Authorization", format!("Bearer {}", token))
                .header(
                    "User-Agent",
                    format!(
                        "{} (Debian 13.0.0; x86_64) WindowsTerminal",
                        crate::gateway::oauth::DEFAULT_OAUTH_USER_AGENT
                    ),
                )
                .header("Content-Type", "application/json")
                .send()
                .await
                .map_err(|e| format!("codex limits fetch failed: {e}"))?;

            if !resp.status().is_success() {
                return Err(format!("codex limits fetch status: {}", resp.status()));
            }

            let body = read_text_with_limit(resp, CODEX_LIMITS_RESPONSE_BODY_LIMIT, "codex limits")
                .await
                .map_err(|e| format!("codex limits body read failed: {e}"))?;
            let json: serde_json::Value = serde_json::from_str(&body)
                .map_err(|e| format!("codex limits parse failed: {e}"))?;

            Ok(OAuthLimitsResult {
                raw_json: Some(json),
                ..Default::default()
            })
        })
    }
}

pub(crate) fn parse_chatgpt_account_id(id_token: Option<&str>) -> Option<String> {
    let token = id_token.map(str::trim).filter(|value| !value.is_empty())?;
    let payload_part = token.split('.').nth(1)?;
    // RFC 7515 JWT segments are unpadded base64url; NO_PAD rejects padded input,
    // so there is no fallback worth attempting.
    let payload = URL_SAFE_NO_PAD.decode(payload_part).ok()?;
    let json: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    json.get("https://api.openai.com/auth")
        .and_then(|value| value.get("chatgpt_account_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header;

    #[test]
    fn authorize_params_use_centralized_originator() {
        let provider = CodexOAuthProvider::new();

        assert!(provider
            .extra_authorize_params()
            .contains(&("originator", upstream_identity::CODEX_CLI_ORIGINATOR)));
    }

    #[test]
    fn inject_upstream_headers_uses_centralized_originator() {
        let provider = CodexOAuthProvider::new();
        let mut headers = HeaderMap::new();

        provider
            .inject_upstream_headers(&mut headers, "access-token")
            .expect("inject headers");

        assert_eq!(
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer access-token")
        );
        assert_eq!(
            headers.get("originator").and_then(|v| v.to_str().ok()),
            Some(upstream_identity::CODEX_CLI_ORIGINATOR)
        );
    }

    #[test]
    fn inject_model_discovery_headers_uses_id_token_account_id() {
        let provider = CodexOAuthProvider::new();
        let payload = URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"account-new"}}"#);
        let id_token = format!("header.{payload}.signature");
        let mut headers = HeaderMap::new();

        provider
            .inject_model_discovery_headers(&mut headers, "new-access", Some(&id_token))
            .expect("inject discovery headers");

        assert_eq!(
            headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer new-access")
        );
        assert_eq!(
            headers
                .get("chatgpt-account-id")
                .and_then(|value| value.to_str().ok()),
            Some("account-new")
        );
        assert_eq!(
            headers
                .get(header::USER_AGENT)
                .and_then(|value| value.to_str().ok()),
            Some(upstream_identity::CODEX_CLI_USER_AGENT)
        );
    }
}
