# Codex Interleaved Tool History Repair Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Repair invalid interleaved `function_call` history in legacy Codex Responses requests sent to non-ChatGPT providers such as LongCat, while preserving real tool results and leaving GPT OAuth and Grok requests unchanged.

**Architecture:** Add a focused pure transformer under provider preparation that closes pending Codex function-call transactions before later history barriers. Provider preparation invokes it only for `cli_key == "codex"`, Responses requests, and non-ChatGPT targets; changed bodies stay attempt-local, strip stale content encoding, and emit count-only diagnostics. Existing reactive strict-schema rectifiers remain unchanged and run on the normalized attempt body.

**Tech Stack:** Rust, `serde_json`, Axum `Bytes`, Tauri gateway failover loop, Cargo unit/integration tests, SQLite request-log evidence, isolated Codex CLI black-box E2E.

---

## File Map

- Create/Test `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/codex_tool_history.rs`: pure interleaved-history transformer, structured outcome, diagnostic builder, and focused tests.
- Modify `src-tauri/src/gateway/proxy/handler/failover_loop/mod.rs`: register the new preparation module.
- Modify/Test `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/provider_iterator.rs`: apply the transformer only to Codex non-ChatGPT Responses attempts, strip request content encoding when changed, and record diagnostics.
- Modify/Test `src-tauri/src/gateway/routes.rs`: add mock-upstream request capture proving the physical request is normalized and the terminal request log contains privacy-safe diagnostics.
- Verify `src-tauri/src/gateway/proxy/handler/failover_loop/response/upstream_error.rs`: the existing `reasoning.context`, `additional_tools`, and `agent_message` rectifiers must remain green; no production change expected.
- Verify `docs/superpowers/specs/2026-07-19-codex-interleaved-tool-history-design.md`: acceptance contract.
- Create temporary E2E evidence only under `/tmp/aio-codex-tool-history-e2e-<run-id>`; never modify live Dev files or original session JSONL.

## Task 1: Add pure transformer tests and prove RED

**Files:**
- Create/Test: `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/codex_tool_history.rs`
- Modify: `src-tauri/src/gateway/proxy/handler/failover_loop/mod.rs`

- [ ] **Step 1: Register an empty focused module**

Add to `failover_loop/mod.rs` beside `codex_chatgpt`:

```rust
#[path = "prepare/codex_tool_history.rs"]
mod codex_tool_history;
```

Create the module with only the outcome API required by tests:

```rust
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
```

- [ ] **Step 2: Add the verified interleaving test**

Add `relocates_late_output_before_assistant_barrier` with this input:

```rust
json!({
    "model": "LongCat-2.0",
    "input": [
        {"type":"function_call","call_id":"call_a","name":"exec_command","arguments":"{\"cmd\":\"a\"}"},
        {"type":"function_call","call_id":"call_b","name":"wait","arguments":"{}"},
        {"type":"function_call_output","call_id":"call_b","output":"B_REAL"},
        {"type":"reasoning","summary":[]},
        {"type":"function_call_output","call_id":"call_a","output":"A_REAL"}
    ]
})
```

Assert final types/call IDs are exactly:

```text
function_call call_a
function_call call_b
function_call_output call_b
function_call_output call_a
reasoning
```

Assert `A_REAL`, `B_REAL`, both call arguments, and all untouched top-level fields remain JSON-value identical. Assert outcome counts are `calls_examined=2`, `outputs_relocated=1`, `barriers_repaired=1`, and zero synthesized outputs.

- [ ] **Step 3: Add parallel and no-op tests**

Add:

- `preserves_valid_parallel_calls_and_reverse_order_outputs`: calls A/B followed by outputs B/A and then a message; assert original bytes and a default/no-change outcome.
- `does_not_reserialize_legal_history`: use deliberately formatted JSON bytes and assert exact byte equality after a no-op scan.
- `preserves_non_tool_history_relative_order`: use two messages around a repaired transaction and assert their sentinels remain ordered.

- [ ] **Step 4: Add missing-output and end-of-input tests**

Add:

- `synthesizes_aborted_before_barrier_for_truly_missing_output`;
- `synthesizes_aborted_at_end_of_input`;
- `does_not_synthesize_when_real_output_exists_after_barrier`.

The synthetic value must be exactly:

```rust
json!({
    "type": "function_call_output",
    "call_id": "call_missing",
    "output": "aborted"
})
```

- [ ] **Step 5: Add malformed and ambiguous-shape tests**

Add one table-driven test proving these shapes remain unchanged and do not cause a guessed repair:

- missing, empty, or non-string `call_id`;
- duplicate `function_call` IDs;
- more than one output for the same call ID;
- orphan output without a call;
- non-array `input` and invalid JSON.

Count malformed IDs separately from duplicate/ambiguous IDs. Do not log or return the actual IDs.

- [ ] **Step 6: Run focused tests and verify behavioral RED**

From `src-tauri`:

```bash
CARGO_TARGET_DIR=target-tests cargo test --locked codex_tool_history -- --nocapture
```

Expected: Cargo prints a non-zero number of matching tests. The verified interleaving and missing-output assertions fail because the placeholder returns no changes. Zero matched tests is not an acceptable RED.

- [ ] **Step 7: Commit the RED tests**

```bash
git add src-tauri/src/gateway/proxy/handler/failover_loop/mod.rs \
  src-tauri/src/gateway/proxy/handler/failover_loop/prepare/codex_tool_history.rs
git commit -m "test(codex): reproduce interleaved tool history"
```

## Task 2: Implement stable tool-transaction normalization

**Files:**
- Modify/Test: `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/codex_tool_history.rs`

- [ ] **Step 1: Add exact item classifiers**

Implement small helpers with no provider knowledge:

```rust
fn item_type(item: &serde_json::Value) -> Option<&str> {
    item.get("type").and_then(serde_json::Value::as_str)
}

fn non_empty_call_id(item: &serde_json::Value) -> Option<&str> {
    item.get("call_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

fn is_history_barrier(item: &serde_json::Value) -> bool {
    !matches!(
        item_type(item),
        Some("function_call")
            | Some("function_call_output")
            | Some("additional_tools")
    )
}
```

`function_call` and `function_call_output` remain transaction items.
`additional_tools` stays neutral because its existing rectifier owns that
provider incompatibility. Every other known or unknown item is conservative
barrier history.

- [ ] **Step 2: Pre-index eligible call/output pairs**

Build counts and source positions before rewriting:

```rust
let mut call_counts: HashMap<String, usize> = HashMap::new();
let mut output_positions: HashMap<String, Vec<usize>> = HashMap::new();
```

Only IDs with exactly one `function_call` and at most one matching output are
eligible. IDs with duplicate calls or duplicate outputs are ambiguous and must
be skipped. Preserve the source array for exact JSON-value cloning.

- [ ] **Step 3: Build the repaired array in one stable pass**

Use:

```rust
let mut next = Vec::with_capacity(items.len());
let mut pending_order: Vec<String> = Vec::new();
let mut pending: HashSet<String> = HashSet::new();
let mut relocated_positions: HashSet<usize> = HashSet::new();
```

For each source index:

1. skip an index already placed through relocation;
2. add an eligible `function_call` to `pending` and `pending_order`, then copy it;
3. copy an ordinary `function_call_output` and remove its ID from pending;
4. before copying a barrier with pending calls, collect unique matching output positions greater than the barrier index;
5. sort those positions numerically and clone each real output into `next`;
6. mark relocated positions and close those pending IDs;
7. synthesize `aborted` for remaining eligible pending IDs in call-opening order;
8. increment `barriers_repaired` once for that boundary and then copy the barrier.

At end-of-input, synthesize `aborted` for any remaining eligible pending calls.
Do not modify call arguments, output values, or non-tool history objects.

- [ ] **Step 4: Commit body bytes only on change**

Parse a cloned JSON value, transform only `root["input"]`, and serialize back
into `Bytes` only when `outcome.changed()` is true:

```rust
if !outcome.changed() {
    return outcome;
}

let Ok(encoded) = serde_json::to_vec(&root) else {
    return CodexToolHistoryNormalization::default();
};
*body = Bytes::from(encoded);
outcome
```

Ensure an encoding failure cannot leave a partially modified body or report a
false hit.

- [ ] **Step 5: Run the focused suite and verify GREEN**

```bash
cd src-tauri
CARGO_TARGET_DIR=target-tests cargo test --locked codex_tool_history -- --nocapture
```

Expected: every focused test passes, including exact no-op bytes, parallel calls, real-output relocation, missing-output synthesis, malformed inputs, and idempotency.

- [ ] **Step 6: Commit the transformer**

```bash
git add src-tauri/src/gateway/proxy/handler/failover_loop/prepare/codex_tool_history.rs
git commit -m "fix(codex): normalize interleaved tool history"
```

## Task 3: Integrate Codex-only provider preparation and diagnostics

**Files:**
- Modify/Test: `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/codex_tool_history.rs`
- Modify/Test: `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/provider_iterator.rs`

- [ ] **Step 1: Add the privacy-safe diagnostic builder**

Add:

```rust
pub(super) fn special_setting(
    provider_id: i64,
    outcome: CodexToolHistoryNormalization,
) -> Option<serde_json::Value> {
    outcome.changed().then(|| serde_json::json!({
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
    }))
}
```

Add an exact-key-set test and assert serialized diagnostics contain none of:
`call_a`, `call_b`, `A_REAL`, `B_REAL`, `arguments`, `output`, `body`, `content`,
`text`, `data`, `payload`, or `request`.

- [ ] **Step 2: Add a provider-iterator application helper**

In `provider_iterator.rs`, add a small helper rather than embedding policy in
the large preparation function:

```rust
fn apply_codex_tool_history_normalization_if_needed(
    cli_key: &str,
    forwarded_path: &str,
    use_codex_chatgpt_backend: bool,
    provider_id: i64,
    special_settings: &Arc<Mutex<Vec<serde_json::Value>>>,
    body: &mut Bytes,
    strip_request_content_encoding: &mut bool,
) -> codex_tool_history::CodexToolHistoryNormalization {
    if cli_key != "codex"
        || use_codex_chatgpt_backend
        || !is_responses_request_path(forwarded_path)
    {
        return Default::default();
    }

    let outcome = codex_tool_history::normalize_interleaved_function_history(body);
    if outcome.changed() {
        *strip_request_content_encoding = true;
        if let Some(setting) = codex_tool_history::special_setting(provider_id, outcome) {
            crate::gateway::response_fixer::push_special_setting(special_settings, setting);
        }
    }
    outcome
}
```

- [ ] **Step 3: Invoke it on the attempt-local body**

At the existing compatibility point around `provider_iterator.rs:459`, keep the
ChatGPT path unchanged and invoke the new helper only in the non-ChatGPT branch,
before `request_body_mutated_before_attempt` is calculated:

```rust
if use_codex_chatgpt_backend {
    apply_chatgpt_compat_and_record(...);
} else {
    apply_codex_tool_history_normalization_if_needed(
        input.cli_key.as_str(),
        upstream_forwarded_path.as_str(),
        false,
        provider_id,
        ctx.special_settings,
        &mut upstream_body_bytes,
        &mut strip_request_content_encoding,
    );
}
```

Do not change shared request state, retry budgets, or Grok branches elsewhere.

- [ ] **Step 4: Add provider-scope tests before relying on integration tests**

Add focused tests that call the helper directly and prove:

- Codex + `/v1/responses` + non-ChatGPT repairs and records one setting;
- Codex + ChatGPT backend is exact no-op;
- Grok + non-ChatGPT is exact no-op;
- Claude is exact no-op;
- Codex non-Responses path is exact no-op;
- a legal Codex history is exact no-op with no diagnostic;
- changed bodies set `strip_request_content_encoding=true`;
- the shared source `Bytes` clone remains byte-identical.

- [ ] **Step 5: Run preparation tests**

```bash
cd src-tauri
CARGO_TARGET_DIR=target-tests cargo test --locked codex_tool_history
CARGO_TARGET_DIR=target-tests cargo test --locked provider_iterator
CARGO_TARGET_DIR=target-tests cargo test --locked chatgpt_preparation
```

Expected: all pass; the Grok and ChatGPT exclusion assertions execute rather
than matching zero tests.

- [ ] **Step 6: Commit integration and diagnostics**

```bash
git add src-tauri/src/gateway/proxy/handler/failover_loop/prepare/codex_tool_history.rs \
  src-tauri/src/gateway/proxy/handler/failover_loop/prepare/provider_iterator.rs
git commit -m "fix(codex): apply tool history repair to foreign providers"
```

## Task 4: Add mock-runtime physical request coverage

**Files:**
- Modify/Test: `src-tauri/src/gateway/routes.rs`

- [ ] **Step 1: Add a capturing HTTP 200 event-stream upstream**

Reuse existing mock upstream and request-log helpers in `routes.rs`. Capture the
physical request body and return a minimal successful Responses SSE sequence or
the established successful stub shape used by neighboring Codex route tests.

- [ ] **Step 2: Add the LongCat-like Codex route test**

Add `mock_runtime_router_normalizes_interleaved_codex_tool_history_before_send`.
Send the verified A/B/B-output/barrier/A-output body through a non-ChatGPT Codex
provider. Assert:

- response status is HTTP 200;
- captured upstream history has A output before the barrier;
- call arguments and both real outputs remain unchanged;
- exactly one physical request was made for this proactive repair;
- captured request no longer contains the invalid interleaving;
- terminal request log status is 200;
- `special_settings_json` contains exactly one
  `codex_interleaved_tool_history_normalizer` entry with the expected counts;
- diagnostics contain no call IDs, arguments, or output values.

- [ ] **Step 3: Add a route-level legal-history control**

Send valid parallel calls followed by their outputs. Assert the captured body is
semantically identical, there is no normalizer diagnostic, and status is 200.

- [ ] **Step 4: Run route tests**

```bash
cd src-tauri
CARGO_TARGET_DIR=target-tests cargo test --locked mock_runtime_router_normalizes_interleaved_codex_tool_history_before_send -- --nocapture
CARGO_TARGET_DIR=target-tests cargo test --locked codex_tool_history -- --nocapture
```

Expected: each named command prints `running 1 test` for the exact route case;
all focused tests pass.

- [ ] **Step 5: Commit runtime coverage**

```bash
git add src-tauri/src/gateway/routes.rs
git commit -m "test(codex): cover interleaved history request repair"
```

## Task 5: Run neighboring and complete Rust verification

**Files:**
- Verify: `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/codex_tool_history.rs`
- Verify: `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/provider_iterator.rs`
- Verify: `src-tauri/src/gateway/proxy/handler/failover_loop/response/upstream_error.rs`
- Verify: `src-tauri/src/gateway/routes.rs`

- [ ] **Step 1: Format and inspect the exact diff**

```bash
cd src-tauri && cargo fmt --all
cd ..
git diff --check
git status --short --branch
git diff --stat HEAD~4..HEAD
```

Confirm no Grok production file, stored session file, app bundle, or unrelated
provider adapter changed.

- [ ] **Step 2: Run focused and neighboring suites**

```bash
cd src-tauri
CARGO_TARGET_DIR=target-tests cargo test --locked codex_tool_history
CARGO_TARGET_DIR=target-tests cargo test --locked agent_message
CARGO_TARGET_DIR=target-tests cargo test --locked additional_tools
CARGO_TARGET_DIR=target-tests cargo test --locked reasoning_context
CARGO_TARGET_DIR=target-tests cargo test --locked previous_response_id
CARGO_TARGET_DIR=target-tests cargo test --locked foreign_history
CARGO_TARGET_DIR=target-tests cargo test --locked provider_max_attempts_
```

Expected: all commands exit 0 with zero failures. Record matched test counts so a
zero-test filter cannot be mistaken for proof.

- [ ] **Step 3: Run the complete suite serially**

```bash
cd src-tauri
CARGO_TARGET_DIR=target-tests cargo test --locked -- --test-threads=1
```

Expected: exit 0; record passed, failed, and ignored totals.

- [ ] **Step 4: Run Clippy with warnings denied**

```bash
cd src-tauri
CARGO_TARGET_DIR=target-tests cargo clippy --all-targets --locked -- -D warnings
```

Expected: exit 0 and no warnings.

- [ ] **Step 5: Commit formatting-only changes if any**

If `cargo fmt` changed touched Rust files after their task commits:

```bash
git add src-tauri/src/gateway/proxy/handler/failover_loop/mod.rs \
  src-tauri/src/gateway/proxy/handler/failover_loop/prepare/codex_tool_history.rs \
  src-tauri/src/gateway/proxy/handler/failover_loop/prepare/provider_iterator.rs \
  src-tauri/src/gateway/routes.rs
git commit -m "style: format Codex tool history repair"
```

Do not claim the compatibility defect fixed from Rust evidence alone.

## Task 6: Run isolated complex legacy-session E2E

**Files/Artifacts:**
- Read-only source: `/Users/gaorongvc/.codex/sessions/2026/07/18/rollout-2026-07-18T17-53-20-019f74a5-085d-7241-b9f8-21efe6570919.jsonl`
- Read-only control session: resolve the current JSONL for `019f51cc-b787-7c63-8708-bd4fa00ae004`
- Create/Delete: `/tmp/aio-codex-tool-history-e2e-<run-id>/...`
- Do not modify: `/Applications/AIO Coding Hub Dev.app`, port `37123`, live application DB/settings, or original session JSONL

- [ ] **Step 1: Record the protected baseline**

Record for both original JSONL files: absolute path, SHA-256, byte size, and
nanosecond mtime. Because the complex source is the currently active Codex
session and can grow from this conversation, immediately copy a static snapshot
and use that snapshot's hash as the immutable test baseline. Distinguish normal
live-session appends from writes made by the E2E clone.

Record current Dev bundle version, PID, listening port `37123`, and `/health`.

- [ ] **Step 2: Build an isolated executable without touching Dev**

Clean only the isolated worktree test/build cache selected for this run. Use a
separate Tauri identifier such as:

```text
io.aio.codinghub.tool-history-e2e
```

Use an isolated application home/dotdir, copied database, and port `37124`.
Do not build into or copy over `/Applications/AIO Coding Hub Dev.app`.

- [ ] **Step 3: Create isolated session clones**

Under the temporary `CODEX_HOME`, create one clone of the control snapshot and
one clone of the complex static snapshot. Generate fresh UUIDs, replace only
`session_meta.payload.id` and the filename UUID, and preserve all subsequent
history items unchanged. Do not register or open the original UUIDs.

- [ ] **Step 4: Run the control legacy session**

Using GPT OAuth provider 12 and LongCat provider 30 from the copied database,
send exact-marker prompts in this order:

```text
GPT -> LongCat -> GPT
```

Require HTTP 200 and exact markers at each step. This proves the new proactive
normalizer and encrypted-content fix did not regress a previously valid old
session.

- [ ] **Step 5: Run the complex legacy session**

On one fresh complex clone:

1. GPT baseline must return HTTP 200 with an exact marker.
2. Switch to LongCat and require HTTP 200 with an exact marker.
3. Switch back to GPT and require HTTP 200 with an exact marker.

For the LongCat request, inspect request logs and require:

- `reasoning.context` rectifier evidence when present;
- `additional_tools` rectifier evidence when present;
- `agent_message`/encrypted-content rectifier evidence when present;
- one proactive `codex_interleaved_tool_history_normalizer` entry;
- `outputsRelocated >= 1` for the reproduced complex history;
- no `invalid replay history`, `unsupported input item type`,
  `unsupported content part type`, `GW_STREAM_ERROR`, or leaked content.

- [ ] **Step 6: Verify isolation and clean temporary runtime state**

Stop only the isolated PID by exact recorded PID and verify port `37124` closes.
Do not use broad `pkill`, `killall`, or process-name matching. Recheck the
installed Dev PID, port `37123`, version, and health are unchanged.

Compare the static source snapshot hashes and the control source hashes. Delete
the temporary E2E root only after preserving a concise evidence summary outside
the app/runtime directories.

- [ ] **Step 7: Record the evidence boundary**

Report separately:

- transformer tests passed;
- full Rust suite passed;
- clippy passed;
- control old-session E2E passed;
- complex old-session GPT to LongCat to GPT passed;
- current Dev remained untouched.

Do not state universal or 100% provider compatibility.

## Task 7: Review the complete branch before integration

**Files:**
- Review all commits after `7c823480`
- Compare against `docs/superpowers/specs/2026-07-19-codex-interleaved-tool-history-design.md`

- [ ] **Step 1: Inspect commit and diff scope**

```bash
git status --short --branch
git log --oneline --decorate 7c823480..HEAD
git diff --check 7c823480..HEAD
git diff --stat 7c823480..HEAD
git diff --name-only 7c823480..HEAD
```

Expected production scope:

```text
src-tauri/src/gateway/proxy/handler/failover_loop/mod.rs
src-tauri/src/gateway/proxy/handler/failover_loop/prepare/codex_tool_history.rs
src-tauri/src/gateway/proxy/handler/failover_loop/prepare/provider_iterator.rs
src-tauri/src/gateway/routes.rs
```

- [ ] **Step 2: Review correctness and privacy**

Reject integration if review finds any of:

- GPT OAuth or Grok request mutation;
- deletion or modification of real call/output content;
- duplicate output emission;
- non-idempotent rewrite;
- hardcoded provider/session IDs in production code;
- diagnostic leakage;
- mutation of shared request bodies or source JSONL;
- broad process termination in E2E scripts or commands.

- [ ] **Step 3: Re-run any verification affected by review changes**

Any production correction requires its focused tests plus full Rust tests and
clippy again. Any change to ordering behavior requires the isolated complex
session E2E again.

- [ ] **Step 4: Stop before merge, push, build, or Dev deployment**

Return the implementation commits, test totals, E2E request-log evidence, and
review result to the user. Merging into `feat/cc2cx-session-titles`, pushing,
cleaning the deployment build cache, compiling the release/Dev bundle, and
overwriting `/Applications/AIO Coding Hub Dev.app` require the user's next
explicit instruction after evidence review.
