//! Usage: normalize interleaved Codex Responses function-call history for strict providers.

use axum::body::Bytes;

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

pub(super) fn normalize_interleaved_function_history(
    _body: &mut Bytes,
) -> CodexToolHistoryNormalization {
    CodexToolHistoryNormalization::default()
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
}
