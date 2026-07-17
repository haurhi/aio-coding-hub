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
- Modify `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/provider_iterator.rs`: remove standard-provider assistant stripping and record applied normalization metadata.
- Modify `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/request_sanitizer.rs`: remove generic Responses assistant stripping; retain Claude OAuth empty-text cleanup only.
- Modify `src-tauri/src/gateway/proxy/handler/failover_loop/attempt/attempt_executor.rs`: remove the final unconditional assistant-stripping block.
- Modify `src-tauri/src/gateway/proxy/protocol_bridge/outbound/openai_responses.rs`: restore assistant text serialization as `output_text` for normal Responses bridges.
- Modify `src-tauri/src/gateway/proxy/protocol_bridge/e2e_tests.rs`: restore/update bridge expectations so assistant content is preserved.

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

- [ ] **Step 3: Run RED tests**

Run:

```bash
cd src-tauri
cargo test --locked --lib foreign_history
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

Change `maybe_apply_codex_chatgpt_request_compat` to normalize the parsed mutable body before allow-list filtering and return `ForeignHistoryNormalization`. Non-Responses paths and invalid JSON return the default outcome.

- [ ] **Step 4: Replace assistant-removal tests with preservation tests**

Update the CX2CC and `codex_chatgpt.rs` tests so assistant messages remain when there is no plaintext reasoning, while a LongCat-shaped body removes only plaintext reasoning.

- [ ] **Step 5: Run GREEN tests**

Run:

```bash
cd src-tauri
cargo test --locked --lib foreign_history
cargo test --locked --lib codex_chatgpt_request_compat
```

Expected: all selected tests pass.

### Task 3: Remove global assistant stripping from unrelated paths

**Files:**
- Modify: `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/provider_iterator.rs`
- Modify: `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/request_sanitizer.rs`
- Modify: `src-tauri/src/gateway/proxy/handler/failover_loop/attempt/attempt_executor.rs`
- Modify: `src-tauri/src/gateway/proxy/protocol_bridge/outbound/openai_responses.rs`
- Modify: `src-tauri/src/gateway/proxy/protocol_bridge/e2e_tests.rs`

- [ ] **Step 1: Add/restore RED preservation tests**

Add a generic sanitizer test proving a standard Responses body containing assistant output is unchanged. Restore `ir_to_request_assistant_text_becomes_output_text` so it expects `output_text`. Add or update an E2E bridge fixture that preserves assistant output.

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

Keep Claude OAuth empty-text cleanup, ChatGPT top-level allow-list filtering, and bridge-specific behavior unrelated to assistant stripping.

- [ ] **Step 4: Run GREEN preservation tests**

Run the two focused commands again. Expected: pass.

### Task 4: Internal observability without upstream leakage

**Files:**
- Modify: `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/codex_chatgpt.rs`
- Modify: `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/provider_iterator.rs`

- [ ] **Step 1: Write a failing metadata-shape test**

Test a helper that converts an applied normalization outcome into:

```json
{
  "type": "foreign_history_handoff",
  "scope": "request",
  "hit": true,
  "providerId": 12,
  "trigger": "plaintext_reasoning_content",
  "reasoningItemsRemoved": 1,
  "previousResponseIdRemoved": true
}
```

Assert that this value is absent from the serialized upstream body.

- [ ] **Step 2: Run RED metadata test**

Run `cargo test --locked --lib foreign_history_handoff_special_setting`. Expected: fail because the helper is missing.

- [ ] **Step 3: Implement metadata recording**

After ChatGPT preparation returns an applied outcome, call `response_fixer::push_special_setting` through `ctx.special_settings`. Log provider ID and counts only; do not log request text or reasoning content.

- [ ] **Step 4: Run GREEN metadata test**

Run the focused test again. Expected: pass.

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

Run `git diff --check` and review every touched compatibility hunk. Confirm no implementation files are staged and no unrelated user changes were reverted.

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

- [ ] **Step 3: Run the bundle without installing it**

Quit the installed Dev app, launch the generated bundle directly, confirm `/health` returns version `0.60.14` with HTTP 200, and verify the process path points into `src-tauri/target/release/bundle`.

### Task 7: Three new Codex Session regressions

**Files:**
- Runtime data only: `~/.aio-coding-hub/aio-coding-hub.db`, logs, and Codex session JSONL files.

- [ ] **Step 1: Snapshot provider state**

Record enabled flags, default-route membership, and circuit rows for providers 10, 12, 21, and 30. Temporarily enable/add only disabled providers required for forced-route tests.

- [ ] **Step 2: LongCat to GPT OAuth**

Create a new Codex session through provider 30 using CLI model `gpt-5.5` and an assistant-only random marker. Resume the same session through provider 12.

Expected:

- source and target request logs are both 200;
- GPT output exactly equals the marker;
- target request contains one `foreign_history_handoff` special setting;
- no `array_above_max_length`, cooldown, or circuit row;
- target provider chain has exactly one upstream attempt.

- [ ] **Step 3: ikuncode to GPT OAuth**

Create another assistant-only marker session through provider 10 and resume through provider 12. Expected: both 200, exact marker recall, and no handoff special setting.

- [ ] **Step 4: Xunfei cc2cx to GPT OAuth**

Temporarily enable provider 21, create another assistant-only marker session, and resume through provider 12. Expected: both 200, exact marker recall, and no handoff special setting.

### Task 8: Restore and report

**Files:**
- Runtime configuration only.

- [ ] **Step 1: Restore provider/runtime state**

Restore provider 21 and 30 enabled flags and route membership to their snapshots. Remove only test-created circuit rows. Stop the generated bundle and relaunch `/Applications/AIO Coding Hub Dev.app`.

- [ ] **Step 2: Fresh verification**

Confirm installed Dev `/health` HTTP 200, provider state matches the snapshot, all three Session logs have the expected results, and `git diff --name-only` contains only the intended existing/implementation files.

- [ ] **Step 3: Deliver evidence**

Report session IDs, markers, request-log IDs, handoff metadata, unit/full-suite counts, build result, and any residual risks. Do not claim success if marker recall or cleanup verification fails.
