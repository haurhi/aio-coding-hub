pub(super) fn token_total(total: Option<i64>, input: Option<i64>, output: Option<i64>) -> i64 {
    if let Some(t) = total {
        return t;
    }
    input.unwrap_or(0).saturating_add(output.unwrap_or(0))
}

pub(super) const SQL_EFFECTIVE_INPUT_TOKENS_EXPR: &str = "CASE WHEN cli_key IN ('codex','gemini') OR EXISTS (SELECT 1 FROM providers p WHERE p.id = final_provider_id AND (p.source_provider_id IS NOT NULL OR p.bridge_type = 'cx2cc')) THEN MAX(COALESCE(input_tokens, 0) - COALESCE(cache_read_input_tokens, 0), 0) ELSE COALESCE(input_tokens, 0) END";

/// Rust twin of the EXISTS predicate inside [`SQL_EFFECTIVE_INPUT_TOKENS_EXPR`].
/// Locked to the SQL by `effective_input_tokens_matches_sql_expression`.
pub(crate) fn is_bridged_input_semantics(
    source_provider_id: Option<i64>,
    bridge_type: Option<&str>,
) -> bool {
    crate::providers::is_cx2cc_bridge(source_provider_id, bridge_type)
}

/// Rust twin of [`SQL_EFFECTIVE_INPUT_TOKENS_EXPR`] — the single source of
/// truth for "effective input tokens". codex/gemini responses and bridged
/// (cx2cc) providers report cache reads inside `input_tokens`, so those reads
/// are subtracted; claude reports them separately and keeps raw input.
/// Locked to the SQL by `effective_input_tokens_matches_sql_expression`.
pub(crate) fn effective_input_tokens(
    cli_key: &str,
    bridged: bool,
    input_tokens: Option<i64>,
    cache_read_input_tokens: Option<i64>,
) -> i64 {
    let input = input_tokens.unwrap_or(0);
    if cli_key == "codex" || cli_key == "gemini" || bridged {
        input
            .saturating_sub(cache_read_input_tokens.unwrap_or(0))
            .max(0)
    } else {
        input
    }
}

/// Display variant of [`effective_input_tokens`]: keeps "usage unknown"
/// (`input_tokens: None`) as `None` instead of collapsing it to 0, so rows and
/// events without usage render as "—" rather than "0". Aggregates keep the
/// COALESCE(..., 0) semantics of the SQL expression.
pub(crate) fn effective_input_tokens_display(
    cli_key: &str,
    bridged: bool,
    input_tokens: Option<i64>,
    cache_read_input_tokens: Option<i64>,
) -> Option<i64> {
    input_tokens
        .is_some()
        .then(|| effective_input_tokens(cli_key, bridged, input_tokens, cache_read_input_tokens))
}

pub(super) fn sql_effective_input_tokens_expr_with_alias(alias: &str) -> String {
    format!(
        "CASE WHEN {alias}.cli_key IN ('codex','gemini') OR EXISTS (SELECT 1 FROM providers p WHERE p.id = {alias}.final_provider_id AND (p.source_provider_id IS NOT NULL OR p.bridge_type = 'cx2cc')) THEN MAX(COALESCE({alias}.input_tokens, 0) - COALESCE({alias}.cache_read_input_tokens, 0), 0) ELSE COALESCE({alias}.input_tokens, 0) END"
    )
}

pub(super) fn sql_effective_total_tokens_expr() -> String {
    format!(
        "({effective_input_expr}) + COALESCE(output_tokens, 0) + COALESCE(cache_creation_input_tokens, 0) + COALESCE(cache_read_input_tokens, 0)",
        effective_input_expr = SQL_EFFECTIVE_INPUT_TOKENS_EXPR
    )
}

pub(super) fn sql_effective_total_tokens_expr_with_alias(alias: &str) -> String {
    let effective_input_expr = sql_effective_input_tokens_expr_with_alias(alias);
    format!(
        "({effective_input_expr}) + COALESCE({alias}.output_tokens, 0) + COALESCE({alias}.cache_creation_input_tokens, 0) + COALESCE({alias}.cache_read_input_tokens, 0)",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_input_tokens_display_preserves_unknown_usage() {
        // Unknown usage stays unknown (frontend renders "—", not "0").
        assert_eq!(
            effective_input_tokens_display("claude", false, None, None),
            None
        );
        assert_eq!(
            effective_input_tokens_display("codex", false, None, Some(500)),
            None
        );
        // Present usage matches the SSOT formula, including the 0 clamp.
        assert_eq!(
            effective_input_tokens_display("claude", false, Some(1200), Some(800)),
            Some(1200)
        );
        assert_eq!(
            effective_input_tokens_display("codex", false, Some(800), Some(1200)),
            Some(0)
        );
        assert_eq!(
            effective_input_tokens_display("claude", true, Some(1200), Some(800)),
            Some(400)
        );
    }

    /// Lock-step guard: the Rust twin must agree with the SQL expression for
    /// every cli/bridge/token combination. If this fails, one side changed
    /// without the other.
    #[test]
    fn effective_input_tokens_matches_sql_expression() {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            r#"
CREATE TABLE providers (id INTEGER PRIMARY KEY, source_provider_id INTEGER, bridge_type TEXT);
-- provider 1: plain, 2: bridged via source id, 3: bridged via cx2cc type, 4: both
INSERT INTO providers VALUES (1, NULL, NULL);
INSERT INTO providers VALUES (2, 99, NULL);
INSERT INTO providers VALUES (3, NULL, 'cx2cc');
INSERT INTO providers VALUES (4, 99, 'cx2cc');
CREATE TABLE r (cli_key TEXT, final_provider_id INTEGER, input_tokens INTEGER, cache_read_input_tokens INTEGER);
"#,
        )
        .expect("create schema");

        let provider_cases: [(i64, Option<i64>, Option<&str>); 5] = [
            (1, None, None),
            (2, Some(99), None),
            (3, None, Some("cx2cc")),
            (4, Some(99), Some("cx2cc")),
            // provider id 5 does not exist in the providers table at all
            (5, None, None),
        ];
        let token_cases: [(Option<i64>, Option<i64>); 5] = [
            (None, None),
            (Some(1200), None),
            (Some(1200), Some(800)),
            (Some(800), Some(1200)), // read > input clamps to 0
            (None, Some(500)),
        ];

        let sql = format!("SELECT {SQL_EFFECTIVE_INPUT_TOKENS_EXPR} FROM r");
        for cli_key in ["claude", "codex", "gemini"] {
            for (provider_id, source_provider_id, bridge_type) in provider_cases {
                for (input, cache_read) in token_cases {
                    conn.execute("DELETE FROM r", []).expect("clear r");
                    conn.execute(
                        "INSERT INTO r VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![cli_key, provider_id, input, cache_read],
                    )
                    .expect("insert row");

                    let sql_value: i64 = conn
                        .query_row(&sql, [], |row| row.get(0))
                        .expect("evaluate sql expression");
                    let bridged = is_bridged_input_semantics(source_provider_id, bridge_type);
                    let rust_value = effective_input_tokens(cli_key, bridged, input, cache_read);

                    assert_eq!(
                        sql_value, rust_value,
                        "cli_key={cli_key} provider={provider_id} input={input:?} cache_read={cache_read:?}"
                    );
                }
            }
        }
    }
}
