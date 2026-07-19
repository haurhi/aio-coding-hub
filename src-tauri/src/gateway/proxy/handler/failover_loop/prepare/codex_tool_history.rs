//! Usage: normalize interleaved Codex Responses function-call history for strict providers.

use axum::body::Bytes;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct CodexToolHistoryNormalization {
    pub(super) calls_examined: usize,
    pub(super) outputs_relocated: usize,
    pub(super) aborted_outputs_synthesized: usize,
    pub(super) barriers_repaired: usize,
    pub(super) malformed_ids_skipped: usize,
    pub(super) duplicate_ids_skipped: usize,
}

impl CodexToolHistoryNormalization {
    pub(super) fn changed(self) -> bool {
        self.outputs_relocated > 0 || self.aborted_outputs_synthesized > 0
    }
}

pub(super) fn special_setting(
    provider_id: i64,
    outcome: CodexToolHistoryNormalization,
) -> Option<Value> {
    outcome.changed().then(|| {
        json!({
            "type": "codex_interleaved_tool_history_normalizer",
            "scope": "attempt",
            "hit": true,
            "action": "close_tool_transactions_before_history_barriers",
            "providerId": provider_id,
            "callsExamined": outcome.calls_examined,
            "outputsRelocated": outcome.outputs_relocated,
            "abortedOutputsSynthesized": outcome.aborted_outputs_synthesized,
            "barriersRepaired": outcome.barriers_repaired,
            "malformedIdsSkipped": outcome.malformed_ids_skipped,
            "duplicateIdsSkipped": outcome.duplicate_ids_skipped,
        })
    })
}

pub(super) fn normalize_interleaved_function_history(
    body: &mut Bytes,
) -> CodexToolHistoryNormalization {
    let Ok(mut root) = serde_json::from_slice::<Value>(body) else {
        return CodexToolHistoryNormalization::default();
    };
    let Some(items) = root.get("input").and_then(Value::as_array).cloned() else {
        return CodexToolHistoryNormalization::default();
    };

    let mut outcome = CodexToolHistoryNormalization::default();
    let mut call_counts: HashMap<String, usize> = HashMap::new();
    let mut output_positions: HashMap<String, Vec<usize>> = HashMap::new();

    for (index, item) in items.iter().enumerate() {
        match item_type(item) {
            Some("function_call") => {
                outcome.calls_examined += 1;
                if let Some(call_id) = non_empty_call_id(item) {
                    *call_counts.entry(call_id.to_string()).or_default() += 1;
                } else {
                    outcome.malformed_ids_skipped += 1;
                }
            }
            Some("function_call_output") => {
                if let Some(call_id) = non_empty_call_id(item) {
                    output_positions
                        .entry(call_id.to_string())
                        .or_default()
                        .push(index);
                } else {
                    outcome.malformed_ids_skipped += 1;
                }
            }
            _ => {}
        }
    }

    let ambiguous_ids: HashSet<String> = call_counts
        .iter()
        .filter_map(|(call_id, count)| {
            let output_count = output_positions.get(call_id).map_or(0, Vec::len);
            (*count > 1 || output_count > 1).then(|| call_id.clone())
        })
        .collect();
    outcome.duplicate_ids_skipped = ambiguous_ids.len();

    let eligible_ids: HashSet<String> = call_counts
        .iter()
        .filter_map(|(call_id, count)| {
            (*count == 1 && !ambiguous_ids.contains(call_id)).then(|| call_id.clone())
        })
        .collect();

    let mut next = Vec::with_capacity(items.len());
    let mut pending_order: Vec<String> = Vec::new();
    let mut pending: HashSet<String> = HashSet::new();
    let mut relocated_positions: HashSet<usize> = HashSet::new();

    for (index, item) in items.iter().enumerate() {
        if relocated_positions.contains(&index) {
            continue;
        }

        match item_type(item) {
            Some("function_call") => {
                if let Some(call_id) = non_empty_call_id(item) {
                    if eligible_ids.contains(call_id) && pending.insert(call_id.to_string()) {
                        pending_order.push(call_id.to_string());
                    }
                }
                next.push(item.clone());
            }
            Some("function_call_output") => {
                if let Some(call_id) = non_empty_call_id(item) {
                    pending.remove(call_id);
                }
                next.push(item.clone());
            }
            Some("additional_tools") => next.push(item.clone()),
            _ => {
                if close_pending_calls(
                    &items,
                    index,
                    &output_positions,
                    &mut pending,
                    &pending_order,
                    &mut relocated_positions,
                    &mut next,
                    &mut outcome,
                ) {
                    outcome.barriers_repaired += 1;
                }
                next.push(item.clone());
            }
        }
    }

    if close_pending_calls(
        &items,
        items.len(),
        &output_positions,
        &mut pending,
        &pending_order,
        &mut relocated_positions,
        &mut next,
        &mut outcome,
    ) {
        outcome.barriers_repaired += 1;
    }

    if !outcome.changed() {
        return outcome;
    }

    root["input"] = Value::Array(next);
    let Ok(encoded) = serde_json::to_vec(&root) else {
        return CodexToolHistoryNormalization::default();
    };
    *body = Bytes::from(encoded);
    outcome
}

fn item_type(item: &Value) -> Option<&str> {
    item.get("type").and_then(Value::as_str)
}

fn non_empty_call_id(item: &Value) -> Option<&str> {
    item.get("call_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|call_id| !call_id.is_empty())
}

#[allow(clippy::too_many_arguments)]
fn close_pending_calls(
    source: &[Value],
    boundary_index: usize,
    output_positions: &HashMap<String, Vec<usize>>,
    pending: &mut HashSet<String>,
    pending_order: &[String],
    relocated_positions: &mut HashSet<usize>,
    next: &mut Vec<Value>,
    outcome: &mut CodexToolHistoryNormalization,
) -> bool {
    if pending.is_empty() {
        return false;
    }

    let mut relocations: Vec<(usize, String)> = pending_order
        .iter()
        .filter(|call_id| pending.contains(call_id.as_str()))
        .filter_map(|call_id| {
            let positions = output_positions.get(call_id)?;
            let position = *positions.first()?;
            (position > boundary_index && !relocated_positions.contains(&position))
                .then(|| (position, call_id.clone()))
        })
        .collect();
    relocations.sort_by_key(|(position, _)| *position);

    let mut changed = false;
    for (position, call_id) in relocations {
        next.push(source[position].clone());
        relocated_positions.insert(position);
        pending.remove(call_id.as_str());
        outcome.outputs_relocated += 1;
        changed = true;
    }

    for call_id in pending_order {
        if !pending.remove(call_id.as_str()) {
            continue;
        }
        next.push(json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": "aborted"
        }));
        outcome.aborted_outputs_synthesized += 1;
        changed = true;
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(value: serde_json::Value) -> Bytes {
        Bytes::from(serde_json::to_vec(&value).expect("serialize request"))
    }

    fn decoded(body: &Bytes) -> serde_json::Value {
        serde_json::from_slice(body).expect("request JSON")
    }

    #[test]
    fn relocates_late_output_before_assistant_barrier() {
        let root = json!({
            "model": "LongCat-2.0",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_a",
                    "name": "exec_command",
                    "arguments": "{\"cmd\":\"a\"}"
                },
                {
                    "type": "function_call",
                    "call_id": "call_b",
                    "name": "wait",
                    "arguments": "{}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_b",
                    "output": "B_REAL"
                },
                {"type": "reasoning", "summary": []},
                {
                    "type": "function_call_output",
                    "call_id": "call_a",
                    "output": "A_REAL"
                }
            ],
            "stream": true
        });
        let mut body = body(root);

        let outcome = normalize_interleaved_function_history(&mut body);

        assert_eq!(
            outcome,
            CodexToolHistoryNormalization {
                calls_examined: 2,
                outputs_relocated: 1,
                aborted_outputs_synthesized: 0,
                barriers_repaired: 1,
                malformed_ids_skipped: 0,
                duplicate_ids_skipped: 0,
            }
        );
        let repaired: serde_json::Value =
            serde_json::from_slice(&body).expect("repaired request JSON");
        assert_eq!(repaired["model"], "LongCat-2.0");
        assert_eq!(repaired["stream"], true);
        assert_eq!(
            repaired["input"],
            json!([
                {
                    "type": "function_call",
                    "call_id": "call_a",
                    "name": "exec_command",
                    "arguments": "{\"cmd\":\"a\"}"
                },
                {
                    "type": "function_call",
                    "call_id": "call_b",
                    "name": "wait",
                    "arguments": "{}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_b",
                    "output": "B_REAL"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_a",
                    "output": "A_REAL"
                },
                {"type": "reasoning", "summary": []}
            ])
        );
    }

    #[test]
    fn preserves_valid_parallel_calls_and_reverse_order_outputs() {
        let original = body(json!({
            "input": [
                {"type":"function_call","call_id":"call_a","name":"a","arguments":"{}"},
                {"type":"function_call","call_id":"call_b","name":"b","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_b","output":"B"},
                {"type":"function_call_output","call_id":"call_a","output":"A"},
                {"type":"message","role":"assistant","content":[]}
            ]
        }));
        let mut candidate = original.clone();

        let outcome = normalize_interleaved_function_history(&mut candidate);

        assert!(!outcome.changed());
        assert_eq!(candidate, original);
    }

    #[test]
    fn does_not_reserialize_legal_history() {
        let original = Bytes::from_static(
            br#"{ "model" : "LongCat-2.0", "input" : [ { "type" : "message", "role" : "user", "content" : [] } ] }"#,
        );
        let mut candidate = original.clone();

        let outcome = normalize_interleaved_function_history(&mut candidate);

        assert_eq!(outcome, CodexToolHistoryNormalization::default());
        assert_eq!(candidate, original);
    }

    #[test]
    fn synthesizes_aborted_before_barrier_for_truly_missing_output() {
        let mut candidate = body(json!({
            "input": [
                {"type":"function_call","call_id":"call_missing","name":"exec","arguments":"{}"},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"after"}]}
            ]
        }));

        let outcome = normalize_interleaved_function_history(&mut candidate);

        assert_eq!(outcome.calls_examined, 1);
        assert_eq!(outcome.outputs_relocated, 0);
        assert_eq!(outcome.aborted_outputs_synthesized, 1);
        assert_eq!(outcome.barriers_repaired, 1);
        assert_eq!(
            decoded(&candidate)["input"],
            json!([
                {"type":"function_call","call_id":"call_missing","name":"exec","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_missing","output":"aborted"},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"after"}]}
            ])
        );
    }

    #[test]
    fn synthesizes_aborted_at_end_of_input() {
        let mut candidate = body(json!({
            "input": [
                {"type":"message","role":"user","content":[]},
                {"type":"function_call","call_id":"call_missing","name":"exec","arguments":"{}"}
            ]
        }));

        let outcome = normalize_interleaved_function_history(&mut candidate);

        assert_eq!(outcome.aborted_outputs_synthesized, 1);
        assert_eq!(outcome.barriers_repaired, 1);
        assert_eq!(
            decoded(&candidate)["input"][2],
            json!({"type":"function_call_output","call_id":"call_missing","output":"aborted"})
        );
    }

    #[test]
    fn does_not_synthesize_when_real_output_exists_after_barrier() {
        let mut candidate = body(json!({
            "input": [
                {"type":"function_call","call_id":"call_late","name":"exec","arguments":"{}"},
                {"type":"agent_message","content":[{"type":"input_text","text":"barrier"}]},
                {"type":"function_call_output","call_id":"call_late","output":"REAL"}
            ]
        }));

        let outcome = normalize_interleaved_function_history(&mut candidate);

        assert_eq!(outcome.outputs_relocated, 1);
        assert_eq!(outcome.aborted_outputs_synthesized, 0);
        let input = decoded(&candidate)["input"].as_array().unwrap().clone();
        assert_eq!(input.len(), 3);
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["output"], "REAL");
        assert_eq!(input[2]["type"], "agent_message");
    }

    #[test]
    fn preserves_non_tool_history_relative_order() {
        let mut candidate = body(json!({
            "input": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":"BEFORE"}]},
                {"type":"function_call","call_id":"call_a","name":"exec","arguments":"{}"},
                {"type":"reasoning","summary":[{"type":"summary_text","text":"MIDDLE"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"AFTER"}]},
                {"type":"function_call_output","call_id":"call_a","output":"REAL"}
            ]
        }));

        normalize_interleaved_function_history(&mut candidate);

        let serialized = String::from_utf8(candidate.to_vec()).unwrap();
        assert!(serialized.find("BEFORE").unwrap() < serialized.find("MIDDLE").unwrap());
        assert!(serialized.find("MIDDLE").unwrap() < serialized.find("AFTER").unwrap());
        assert!(serialized.find("REAL").unwrap() < serialized.find("MIDDLE").unwrap());
    }

    #[test]
    fn skips_malformed_duplicate_and_orphan_shapes() {
        let original = body(json!({
            "input": [
                {"type":"function_call","name":"missing","arguments":"{}"},
                {"type":"function_call","call_id":"","name":"empty","arguments":"{}"},
                {"type":"function_call","call_id":7,"name":"number","arguments":"{}"},
                {"type":"function_call","call_id":"call_dup","name":"first","arguments":"{}"},
                {"type":"function_call","call_id":"call_dup","name":"second","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_dup","output":"one"},
                {"type":"function_call","call_id":"call_multi_output","name":"multi","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_multi_output","output":"one"},
                {"type":"function_call_output","call_id":"call_multi_output","output":"two"},
                {"type":"function_call_output","call_id":"call_orphan","output":"orphan"},
                {"type":"reasoning","summary":[]}
            ]
        }));
        let mut candidate = original.clone();

        let outcome = normalize_interleaved_function_history(&mut candidate);

        assert!(!outcome.changed());
        assert_eq!(candidate, original);
        assert_eq!(outcome.calls_examined, 6);
        assert_eq!(outcome.malformed_ids_skipped, 3);
        assert_eq!(outcome.duplicate_ids_skipped, 2);
    }

    #[test]
    fn invalid_json_and_non_array_input_are_noops() {
        for original in [
            Bytes::from_static(b"not-json"),
            Bytes::from_static(br#"{"input":"not-an-array"}"#),
        ] {
            let mut candidate = original.clone();
            assert_eq!(
                normalize_interleaved_function_history(&mut candidate),
                CodexToolHistoryNormalization::default()
            );
            assert_eq!(candidate, original);
        }
    }

    #[test]
    fn second_pass_is_noop_after_repair() {
        let mut candidate = body(json!({
            "input": [
                {"type":"function_call","call_id":"call_late","name":"exec","arguments":"{}"},
                {"type":"reasoning","summary":[]},
                {"type":"function_call_output","call_id":"call_late","output":"REAL"}
            ]
        }));

        let first = normalize_interleaved_function_history(&mut candidate);
        let after_first = candidate.clone();
        let second = normalize_interleaved_function_history(&mut candidate);

        assert!(first.changed());
        assert!(!second.changed());
        assert_eq!(candidate, after_first);
    }

    #[test]
    fn diagnostic_setting_has_exact_privacy_safe_shape() {
        let setting = special_setting(
            30,
            CodexToolHistoryNormalization {
                calls_examined: 2,
                outputs_relocated: 1,
                aborted_outputs_synthesized: 0,
                barriers_repaired: 1,
                malformed_ids_skipped: 0,
                duplicate_ids_skipped: 0,
            },
        )
        .expect("changed outcome should produce a setting");
        let object = setting.as_object().expect("setting object");
        let keys: std::collections::BTreeSet<&str> = object.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            std::collections::BTreeSet::from([
                "abortedOutputsSynthesized",
                "action",
                "barriersRepaired",
                "callsExamined",
                "duplicateIdsSkipped",
                "hit",
                "malformedIdsSkipped",
                "outputsRelocated",
                "providerId",
                "scope",
                "type",
            ])
        );
        let serialized = serde_json::to_string(&setting).unwrap();
        for forbidden in [
            "call_a",
            "call_b",
            "A_REAL",
            "B_REAL",
            "arguments",
            "\"output\"",
            "\"body\"",
            "\"content\"",
            "\"text\"",
            "\"data\"",
            "\"payload\"",
            "\"request\"",
        ] {
            assert!(!serialized.contains(forbidden), "leaked {forbidden}");
        }
    }
}
