# Shape-Aware Codex Provider Handoff Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Allow a LongCat-generated Codex Responses transcript to continue on a ChatGPT OAuth provider by removing only foreign plaintext reasoning while preserving assistant and tool history.

**Architecture:** Add a pure shape-normalization helper in the CX2CC Responses compatibility module and invoke it only in the ChatGPT backend preparation path. Return structured outcome counts to the provider iterator for internal request logging. Remove the current global assistant stripping from standard providers, the generic sanitizer, the final send defense, and Responses bridge serialization.

**Tech Stack:** Rust, serde_json, Axum Bytes, Tauri gateway failover loop, cargo test, Codex CLI, SQLite request logs.

**Design:** `docs/superpowers/specs/2026-07-17-shape-aware-codex-provider-handoff-design.md`

**Worktree constraint:** The relevant Rust files already contain overlapping uncommitted compatibility work. Preserve those edits, stage nothing outside the design/plan documents, and do not commit implementation files until the user reviews the final diff.

---

## File Map

- Modify `src-tauri/src/gateway/proxy/protocol_bridge/cx2cc/mod.rs`: pure normalization helper, outcome type, ChatGPT compatibility behavior, unit tests.
- Modify `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/codex_chatgpt.rs`: apply normalization to ChatGPT Responses bodies, return outcome, build internal special-setting metadata, unit tests.
- Modify `src-tauri/src/gateway/proxy/handler/failover_loop/mod.rs`: remove the obsolete assistant-strip re-export/import.
- Modify `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/provider_iterator.rs`: remove standard-provider assistant stripping and record applied normalization metadata.
- Modify `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/request_sanitizer.rs`: remove generic Responses assistant stripping; retain Claude OAuth empty-text cleanup only.
- Modify `src-tauri/src/gateway/proxy/handler/failover_loop/attempt/attempt_executor.rs`: remove the final unconditional assistant-stripping block.
- Modify `src-tauri/src/gateway/proxy/protocol_bridge/outbound/openai_responses.rs`: restore assistant text serialization as `output_text` for normal Responses bridges.
- Modify `src-tauri/src/gateway/proxy/protocol_bridge/e2e_tests.rs`: restore/update bridge expectations so assistant content is preserved.

### Task 0: Capture the dirty-worktree and runtime baseline

**Files:**
- Read-only baseline: Git index/worktree and `~/.aio-coding-hub/aio-coding-hub.db`.

- [ ] **Step 1: Capture Git before-images**

Before editing, record all three independently:

```bash
git status --short
git diff --cached --binary
git diff --binary
```

Save the outputs as turn evidence under `/tmp/aio-handoff-baseline/` and record their SHA-256 digests. The cached diff must be empty unless the user already staged something. Never stage an implementation file wholesale.

- [ ] **Step 2: Capture an informational provider-state baseline**

Run against `~/.aio-coding-hub/aio-coding-hub.db` with `.timeout 15000`:

```sql
SELECT id,name,enabled,updated_at FROM providers WHERE id IN (10,12,21,30) ORDER BY id;
SELECT cli_key,provider_id,sort_order,created_at,updated_at
FROM default_route_providers
WHERE cli_key='codex' AND provider_id IN (10,12,21,30)
ORDER BY provider_id;
SELECT * FROM provider_circuit_breakers WHERE provider_id IN (10,12,21,30) ORDER BY provider_id;
```

Store this JSON only as an early drift check. It is not authoritative for restoration because the installed gateway remains active during implementation and build work. The authoritative quiescent snapshot is captured after the installed app exits in Task 6.

- [ ] **Step 3: Define the cleanup gate before mutations**

No live setup starts until the exact inverse SQL has been prepared from the quiescent Task 6 before-image. On any failed CLI, gateway, or assertion command: stop further tests, stop the generated bundle and wait for process exit, execute restoration, relaunch `/Applications/AIO Coding Hub Dev.app`, and verify `/health` before reporting.

### Task 1: RED tests for shape-aware normalization

**Files:**
- Test: `src-tauri/src/gateway/proxy/protocol_bridge/cx2cc/mod.rs`

- [ ] **Step 1: Add a failing LongCat-shape test**

Add `normalizes_plaintext_reasoning_without_rewriting_other_items`. Build an input containing developer, user, plaintext reasoning, assistant marker, function call, function output, and current user items plus `previous_response_id`.

Expected assertions:

```rust
assert_eq!(outcome.reasoning_items_removed, 1);
assert!(outcome.previous_response_id_removed);
assert_eq!(next["input"], expected_input_without_only_plaintext_reasoning);
assert_eq!(next["input"][2]["role"], "assistant");
assert_eq!(next["input"][2]["content"][0]["text"], "AU_MARKER");
assert!(next.get("previous_response_id").is_none());
```

- [ ] **Step 2: Add failing no-op and idempotence tests**

Cover encrypted reasoning with absent/null/empty content, assistant-only Xunfei shape, non-array input, tool-only input, no-user input, and a second normalization pass.

Also include image content and assert all non-reasoning JSON values remain structurally equal and ordered.

Add `codex_chatgpt_invalid_json_is_byte_identical_and_reports_diagnostic` before changing the byte-level preparation wrapper. It must prove invalid input bytes are unchanged and the diagnostic outcome/event contains only path and body length.

- [ ] **Step 3: Run RED tests**

Run:

```bash
cd src-tauri
cargo test --locked --lib foreign_history
cargo test --locked --lib codex_chatgpt_invalid_json
```

Expected: compilation/test failure because the new outcome type and normalization function do not exist. Confirm the failure is about missing behavior, not malformed fixtures.

### Task 2: GREEN pure normalization and ChatGPT compatibility

**Files:**
- Modify: `src-tauri/src/gateway/proxy/protocol_bridge/cx2cc/mod.rs`
- Modify: `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/codex_chatgpt.rs`

- [ ] **Step 1: Implement the minimal outcome type and pure helper**

Add an outcome such as:

```rust
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ForeignHistoryNormalization {
    pub(crate) reasoning_items_removed: usize,
    pub(crate) previous_response_id_removed: bool,
}

impl ForeignHistoryNormalization {
    pub(crate) fn applied(self) -> bool {
        self.reasoning_items_removed > 0
    }
}
```

Implement `normalize_foreign_responses_history_for_chatgpt(&mut Value)`. Trigger only when `input` contains a reasoning object whose `content` is a non-empty array. Retain every other input value in order. Remove `previous_response_id` only after the trigger matches.

- [ ] **Step 2: Stop stripping assistant messages in the allow-list filter**

Remove the unconditional assistant retention filter from `codex_chatgpt_request_compat_value`. Keep the existing allowed top-level keys, `stream: true`, `store: false`, and instruction coercion.

- [ ] **Step 3: Apply normalization in the ChatGPT body preparation helper**

Change `maybe_apply_codex_chatgpt_request_compat` to normalize the parsed mutable body before allow-list filtering and return `ForeignHistoryNormalization`. Non-Responses paths and invalid JSON return the default outcome. Invalid JSON must remain byte-identical and emit a debug event containing only path and body length.

- [ ] **Step 4: Replace assistant-removal tests with preservation tests**

Update the CX2CC and `codex_chatgpt.rs` tests so assistant messages remain when there is no plaintext reasoning, while a LongCat-shaped body removes only plaintext reasoning.

- [ ] **Step 5: Run GREEN tests**

Run:

```bash
cd src-tauri
cargo test --locked --lib foreign_history
cargo test --locked --lib codex_chatgpt_request_compat
cargo test --locked --lib codex_chatgpt_invalid_json
```

Expected: all selected tests pass.

### Task 3: Remove global assistant stripping from unrelated paths

**Files:**
- Modify: `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/provider_iterator.rs`
- Modify: `src-tauri/src/gateway/proxy/handler/failover_loop/mod.rs`
- Modify: `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/request_sanitizer.rs`
- Modify: `src-tauri/src/gateway/proxy/handler/failover_loop/attempt/attempt_executor.rs`
- Modify: `src-tauri/src/gateway/proxy/protocol_bridge/outbound/openai_responses.rs`
- Modify: `src-tauri/src/gateway/proxy/protocol_bridge/e2e_tests.rs`

- [ ] **Step 1: Add/restore RED preservation tests**

Add a generic sanitizer test proving a standard Responses body containing assistant output is unchanged. Restore `ir_to_request_assistant_text_becomes_output_text` so it expects `output_text`. Add or update an E2E bridge fixture that preserves assistant output.

Add a ChatGPT no-trigger test proving assistant, image, function-call, and function-call-output values remain structurally equal and ordered. The invalid-JSON RED test already exists from Task 1 and must turn green in Task 2; do not add it after implementation.

- [ ] **Step 2: Run RED preservation tests**

Run:

```bash
cd src-tauri
cargo test --locked --lib assistant_text_becomes_output_text
cargo test --locked --lib standard_responses_body_preserves_assistant
```

Expected: fail because current code skips/strips assistant content.

- [ ] **Step 3: Remove all unrelated strip calls**

Remove:

- the standard API-key `else` branch in provider preparation;
- the generic Responses safety net in `request_sanitizer`;
- the last-line assistant defense in `attempt_executor`;
- assistant-text suppression in `openai_responses`.

Delete the obsolete assistant-strip helper functions in `cx2cc/mod.rs` and `codex_chatgpt.rs`, plus their import/re-export in `failover_loop/mod.rs`. Confirm `rg 'strip_responses_api_assistant_messages|maybe_strip_responses_api_assistant_messages' src-tauri/src` returns no production call sites.

Keep Claude OAuth empty-text cleanup, ChatGPT top-level allow-list filtering, and bridge-specific behavior unrelated to assistant stripping.

- [ ] **Step 4: Run GREEN preservation tests**

Run the two focused commands again. Expected: pass.

### Task 4: Internal observability without upstream leakage

**Files:**
- Modify: `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/codex_chatgpt.rs`
- Modify: `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/provider_iterator.rs`

- [ ] **Step 1: Write failing metadata and preparation-wiring tests**

Test a helper that converts an applied normalization outcome into:

```json
{
  "type": "foreign_history_handoff",
  "trigger": "plaintext_reasoning_content",
  "reasoning_items_removed": 1,
  "previous_response_id_removed": true
}
```

Assert that this value is absent from the serialized upstream body.

Before implementing provider-iterator wiring, add `chatgpt_preparation_records_foreign_history_handoff_without_mutating_shared_body`. It must exercise the actual preparation boundary and assert the normalized outbound body, unchanged shared body, preserved assistant/image/tool/top-level/header values, and exactly one internal special setting.

- [ ] **Step 2: Run RED metadata test**

Run:

```bash
cd src-tauri
cargo test --locked --lib foreign_history_handoff_special_setting
cargo test --locked --lib chatgpt_preparation_records_foreign_history_handoff_without_mutating_shared_body
```

Expected: both fail for missing metadata/wiring behavior rather than fixture errors.

- [ ] **Step 3: Implement metadata recording**

After ChatGPT preparation returns an applied outcome, call `response_fixer::push_special_setting` through `ctx.special_settings`. The special-setting JSON must match the approved schema verbatim. Put provider ID only in the structured tracing event. Do not log request text or reasoning content.

- [ ] **Step 4: Run GREEN metadata and preparation tests**

Run both focused commands from Step 2 again. Expected: pass. The preparation test must assert:

- the prepared outbound JSON has no non-empty plaintext reasoning;
- assistant marker, image, tool items, ordering, allowed top-level fields, and headers remain unchanged;
- the shared input body remains byte-identical;
- exactly one internal special setting is present with the approved schema;
- the special setting does not appear in the outbound JSON.

### Task 5: Rust regression verification

**Files:**
- Verify all modified Rust files.

- [ ] **Step 1: Format check**

Run:

```bash
cd src-tauri
cargo fmt -- --check
```

- [ ] **Step 2: Focused compatibility suite**

Run:

```bash
cd src-tauri
cargo test --locked --lib foreign_history
cargo test --locked --lib codex_chatgpt_request_compat
cargo test --locked --lib assistant
cargo test --locked --lib apply_codex_api_key_model_mapping
```

- [ ] **Step 3: Full Rust library suite**

Run:

```bash
cd src-tauri
cargo test --locked --lib
```

Expected: zero failures.

- [ ] **Step 4: Inspect the complete worktree diff**

Run `git diff --check`, `git diff --cached --name-only`, and review every touched compatibility hunk. Compare the current cached and unstaged diffs against the Task 0 before-images. Confirm the index contains only already-approved docs, no implementation files are staged, and all unrelated pre-existing hunks remain present.

### Task 6: Build the test bundle

**Files:**
- Build output only: `dist/`, `src-tauri/target/release/`, and the generated Dev app bundle.

- [ ] **Step 1: Frontend/build verification**

Run `pnpm build`. Expected: exit 0.

- [ ] **Step 2: Build current Dev bundle**

Run from the repository root:

```bash
pnpm tauri:build -- -c .local/tauri.build.dev.json
```

Expected bundle:

```text
src-tauri/target/release/bundle/macos/AIO Coding Hub Dev.app
```

- [ ] **Step 3: Quiesce the installed runtime and capture the authoritative before-image**

Quit the installed Dev app and wait until its gateway process has exited. Only now capture the authoritative provider/route/circuit JSON for providers 10, 12, 21, and 30 and generate exact inverse SQL. Apply the setup SQL while no gateway process is running.

- [ ] **Step 4: Launch the generated bundle without installing it**

Launch the generated bundle directly, confirm `/health` returns version `0.60.14` with HTTP 200, verify the process path points into `src-tauri/target/release/bundle`, and read back that setup state is loaded. If launch or readback fails, stop the bundle before restoring the database.

### Task 7: Three new Codex Session regressions

**Files:**
- Runtime data only: `~/.aio-coding-hub/aio-coding-hub.db`, logs, and Codex session JSONL files.

- [ ] **Step 1: Verify the quiescent snapshot and loaded setup state**

Use the authoritative Task 6 before-image. Verify its saved digest and verify the generated bundle has loaded the intended temporary enabled flags, routes, and cleared test circuit state before sending any provider request.

For providers 21 and 30, the setup SQL applied while the gateway was stopped in Task 6 is:

```sql
BEGIN IMMEDIATE;
UPDATE providers SET enabled=1 WHERE id IN (21,30);
INSERT OR IGNORE INTO default_route_providers(cli_key,provider_id,sort_order,created_at,updated_at)
VALUES('codex',21,999,strftime('%s','now'),strftime('%s','now'));
INSERT OR IGNORE INTO default_route_providers(cli_key,provider_id,sort_order,created_at,updated_at)
VALUES('codex',30,1000,strftime('%s','now'),strftime('%s','now'));
DELETE FROM provider_circuit_breakers WHERE provider_id IN (10,12,21,30);
COMMIT;
```

Only run this after generating inverse SQL that restores every touched row exactly from the authoritative quiescent snapshot. Forced provider URLs are:

```text
ikuncode: http://127.0.0.1:37123/codex/_aio/provider/10/v1
GPT OAuth: http://127.0.0.1:37123/codex/_aio/provider/12/v1
Xunfei: http://127.0.0.1:37123/codex/_aio/provider/21/v1
LongCat: http://127.0.0.1:37123/codex/_aio/provider/30/v1
```

- [ ] **Step 2: LongCat to GPT OAuth**

Create a new Codex session through provider 30 using CLI model `gpt-5.5` and an assistant-only random marker. Resume the same session through provider 12.

Use this command shape for every source, replacing the forced URL and marker prefix:

```bash
codex exec --ignore-user-config --skip-git-repo-check --json -s read-only -m gpt-5.5 \
  -c 'model_provider="aio"' \
  -c 'model_providers.aio={ name="aio", base_url="FORCED_URL", wire_api="responses", requires_openai_auth=true }' \
  'Invent one random assistant-only marker ... Reply only with the marker.' </dev/null
```

Extract `thread_id` and the assistant marker from JSONL, then resume:

```bash
codex exec --ignore-user-config --skip-git-repo-check --json -s read-only -m gpt-5.5 \
  -c 'model_provider="aio"' \
  -c 'model_providers.aio={ name="aio", base_url="http://127.0.0.1:37123/codex/_aio/provider/12/v1", wire_api="responses", requires_openai_auth=true }' \
  resume SESSION_ID \
  'Without tools, reply exactly with the marker invented by the previous assistant.' </dev/null
```

Expected:

- source and target request logs are both 200;
- GPT output exactly equals the marker;
- target request contains one `foreign_history_handoff` special setting;
- no `array_above_max_length`, cooldown, or circuit row;
- target provider chain has exactly one upstream attempt.

Read back by exact `session_id`:

```sql
SELECT id,trace_id,status,error_code,session_id,final_provider_id,requested_model,
       special_settings_json,provider_chain_json,error_details_json
FROM request_logs
WHERE session_id='SESSION_ID'
ORDER BY id;
```

The preparation-level wiring test is the authoritative assertion that normalized outbound JSON contains no non-empty plaintext reasoning while preserving the assistant marker. The live log must corroborate it with the special setting and one attempt.

- [ ] **Step 3: ikuncode to GPT OAuth**

Create another assistant-only marker session through provider 10 and resume through provider 12. Expected: both 200, exact marker recall, and no handoff special setting.

- [ ] **Step 4: Xunfei cc2cx to GPT OAuth**

Temporarily enable provider 21, create another assistant-only marker session, and resume through provider 12. Expected: both 200, exact marker recall, and no handoff special setting.

### Task 8: Restore and report

**Files:**
- Runtime configuration only.

- [ ] **Step 1: Restore provider/runtime state**

In a guaranteed cleanup path, stop the generated bundle and wait for process exit first. Then restore providers 10, 12, 21, and 30, their route rows, and their circuit rows exactly from the authoritative Task 6 snapshot. Do not merely delete all circuit rows if one existed before testing. Only after database restoration succeeds, relaunch `/Applications/AIO Coding Hub Dev.app` whether tests pass or fail.

- [ ] **Step 2: Fresh verification**

Confirm installed Dev `/health` HTTP 200, provider/route/circuit JSON matches the authoritative Task 6 snapshot, all three Session logs have the expected results, `git diff --cached --name-only` contains no implementation file, and the final unstaged diff still contains every pre-existing Task 0 Git baseline hunk plus only the intended implementation changes.

- [ ] **Step 3: Deliver evidence**

Report session IDs, markers, request-log IDs, handoff metadata, unit/full-suite counts, build result, and any residual risks. Do not claim success if marker recall or cleanup verification fails.
