# Codex `agent_message` Encrypted Content Rectifier Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make complex legacy Codex sessions continue from GPT to strict Responses providers such as LongCat by removing provider-private `encrypted_content` parts while converting rejected `agent_message` history, without changing successful requests or stored session JSONL.

**Architecture:** Extend the existing reactive `agent_message` rectifier in the upstream 400 handler. A structured outcome records converted items, removed encrypted parts, and encrypted-only items; a small retry-preparation helper applies the one-shot and pending flags and returns `ContinueRetry`, so the encrypted-only path is directly testable. No proactive request rewrite and no second retry matcher are introduced.

**Tech Stack:** Rust, `serde_json`, Axum `Bytes`, Reqwest, Tauri gateway failover loop, Cargo tests, SQLite request logs, Codex CLI isolated black-box regression.

---

## File Map

- Modify/Test `src-tauri/src/gateway/proxy/handler/failover_loop/response/upstream_error.rs`: add the structured rectification outcome, sanitize converted content, schedule the retry for encrypted-only history, emit approved diagnostics, and add focused/handler-level tests.
- Read/Verify `src-tauri/src/gateway/proxy/handler/failover_loop/attempt/retry_engine.rs`: confirm the pending flag still reserves and consumes the same provider attempt; no production change expected.
- Read/Verify `src-tauri/src/gateway/proxy/handler/failover_loop/attempt/attempt_executor.rs`: confirm existing one-shot and pending state remains sufficient; no production change expected.
- Verify `docs/superpowers/specs/2026-07-19-codex-agent-message-encrypted-content-design.md`: use as the acceptance contract.
- Create temporary evidence only under `/tmp/aio-encrypted-content-e2e-<run-id>` during live regression; do not add or modify user session JSONL.

### Task 1: Add transformer regression tests and prove RED

**Files:**
- Modify/Test: `src-tauri/src/gateway/proxy/handler/failover_loop/response/upstream_error.rs`

- [ ] **Step 1: Add a failing mixed-content test**

Add a test named `agent_message_rectifier_removes_encrypted_content_and_preserves_portable_parts` using one `agent_message` whose content is:

```rust
serde_json::json!([
    {"type": "input_text", "text": "KEEP_BEFORE"},
    {"type": "encrypted_content", "data": "DROP_ONE"},
    {"type": "output_text", "text": "KEEP_AFTER"},
    {"type": "encrypted_content", "data": "DROP_TWO"}
])
```

Assert that the result is a `message/user`, only the two portable parts remain in original order, and neither opaque value exists in the serialized corrected body.

- [ ] **Step 2: Add a failing encrypted-only test**

Add `agent_message_rectifier_removes_encrypted_only_item`. The input contains one normal non-agent item, one `agent_message` containing only `encrypted_content`, and another normal item. Assert that the encrypted-only item is deleted and the two normal items retain their order.

- [ ] **Step 3: Run the two behavior tests and verify assertion RED before introducing the new outcome type**

Run from `src-tauri` without `--exact` so Cargo's unique substring filter
matches the module-qualified unit-test name:

```bash
CARGO_TARGET_DIR=target-tests cargo test --locked agent_message_rectifier_removes_encrypted_content_and_preserves_portable_parts -- --nocapture
CARGO_TARGET_DIR=target-tests cargo test --locked agent_message_rectifier_removes_encrypted_only_item -- --nocapture
```

Each command must print `running 1 test`. Zero matched tests is a failed test
procedure, even if Cargo exits 0. Both tests must execute and fail on assertions:
the first because the old implementation preserves `encrypted_content`, and
the second because it keeps the encrypted-only item. Fix test setup errors until
the failures are behavioral.

- [ ] **Step 4: Add a failing structured-count and mixed-input test**

Add `agent_message_rectifier_reports_structured_counts_for_mixed_input` with:

- one surviving `agent_message` containing one portable and two encrypted parts;
- one encrypted-only `agent_message` containing one encrypted part;
- one non-agent item with similarly named nested fields.

Assert the desired outcome is:

```rust
CodexAgentMessageRectificationOutcome {
    items_converted: 1,
    encrypted_content_parts_removed: 3,
    empty_items_removed: 1,
}
```

Assert the non-agent item is byte-semantically unchanged and final ordering is preserved.

- [ ] **Step 5: Preserve malformed/out-of-scope behavior**

Extend or add a test proving non-array or missing `content` on an `agent_message` is unchanged, and non-`agent_message` items containing `encrypted_content` are unchanged.

- [ ] **Step 6: Run the structured API test and verify RED**

Run from `src-tauri`:

```bash
CARGO_TARGET_DIR=target-tests cargo test --locked agent_message_rectifier_reports_structured_counts_for_mixed_input -- --nocapture
```

The two behavioral tests were already executed in Step 3. The structured-count
command must not report `running 0 tests`. A
compile failure because `CodexAgentMessageRectificationOutcome` or the new
return shape does not exist is an acceptable API-design RED, recorded
separately from the two behavioral assertion failures.

- [ ] **Step 7: Record RED evidence before changing production logic**

Save the exact failing test names and assertion differences in the implementation notes or terminal transcript. Do not modify or restart `/Applications/AIO Coding Hub Dev.app`.

### Task 2: Implement the minimal structured transformation

**Files:**
- Modify/Test: `src-tauri/src/gateway/proxy/handler/failover_loop/response/upstream_error.rs`

- [ ] **Step 1: Add the outcome type**

Add a private, comparable outcome near the converter:

```rust
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
```

`items_converted` counts only items that become and remain `message/user`.

- [ ] **Step 2: Replace in-place iteration with order-preserving filtering**

Change `convert_codex_agent_messages_to_user_messages` to return `Option<CodexAgentMessageRectificationOutcome>`. Parse the root once, take or drain the `input` array, and rebuild it in order:

- pass non-agent and malformed agent items through unchanged;
- for a valid `agent_message.content[]`, remove only parts whose object field `type` is exactly `encrypted_content`;
- convert to `message/user` when portable parts remain;
- omit the entire item when no part remains;
- serialize and replace `body` only when `outcome.changed()` is true.

Do not inspect, copy, decode, or log the encrypted `data` value.

- [ ] **Step 3: Keep the matcher and retry scope unchanged**

Keep `matches_codex_agent_message_error` exact and keep the rectifier limited to `codex | grok` Responses requests, non-truncated error bodies, and one physical retry. Do not add an `encrypted_content` error matcher.

- [ ] **Step 4: Run transformer tests and verify GREEN**

```bash
cd src-tauri
CARGO_TARGET_DIR=target-tests cargo test --locked agent_message_rectifier_ -- --nocapture
CARGO_TARGET_DIR=target-tests cargo test --locked converts_agent_messages_to_user_messages_and_preserves_content
```

Expected: new sanitization/count tests and existing preservation/idempotency tests pass.

- [ ] **Step 5: Commit the transformation unit**

```bash
git add src-tauri/src/gateway/proxy/handler/failover_loop/response/upstream_error.rs
git commit -m "fix(codex): sanitize converted agent message content"
```

### Task 3: Prove encrypted-only history schedules the existing retry

**Files:**
- Modify/Test: `src-tauri/src/gateway/proxy/handler/failover_loop/response/upstream_error.rs`
- Read/Verify: `src-tauri/src/gateway/proxy/handler/failover_loop/attempt/retry_engine.rs`

- [ ] **Step 1: Add a failing retry-preparation test**

Add `agent_message_encrypted_only_rectifier_schedules_retry`. Use a body whose only `agent_message` content is encrypted and the exact structured upstream 400. The test must observe all of:

```rust
assert!(already_retried);
assert!(retry_pending);
assert!(strip_request_content_encoding);
assert!(matches!(control, Some(LoopControl::ContinueRetry)));
assert_eq!(outcome.items_converted, 0);
assert_eq!(outcome.empty_items_removed, 1);
```

Immediately run before the helper exists:

```bash
cd src-tauri
CARGO_TARGET_DIR=target-tests cargo test --locked agent_message_encrypted_only_rectifier_schedules_retry -- --nocapture
```

Expected RED: the helper/API does not exist yet. In the later GREEN run, require
the output to show `running 1 test` and all listed state assertions to pass.

- [ ] **Step 2: Extract a narrow retry-preparation helper**

Introduce a private synchronous helper used by `handle_non_success_response` that:

- performs the existing match and transformation;
- gates success on `outcome.changed()`, never on `items_converted > 0`;
- sets `already_retried`, `retry_pending`, and `strip_request_content_encoding`;
- returns `(LoopControl::ContinueRetry, outcome)` or an equivalent directly testable result.

Keep diagnostics emission in the handler so request/provider metadata stays at the existing call site.

- [ ] **Step 3: Add/retain one-shot coverage**

Extend `agent_message_rectifier_retries_once` to prove a second call with `already_retried=true` leaves the body and pending flags unchanged.

- [ ] **Step 4: Run retry tests and verify GREEN**

```bash
cd src-tauri
CARGO_TARGET_DIR=target-tests cargo test --locked agent_message_encrypted_only_rectifier_schedules_retry
CARGO_TARGET_DIR=target-tests cargo test --locked agent_message_rectifier_retries_once
CARGO_TARGET_DIR=target-tests cargo test --locked agent_message
```

Expected: encrypted-only schedules exactly one same-provider retry and all existing matcher/one-shot tests pass.

- [ ] **Step 5: Commit the retry behavior**

```bash
git add src-tauri/src/gateway/proxy/handler/failover_loop/response/upstream_error.rs
git commit -m "fix(codex): retry encrypted-only agent history"
```

### Task 4: Add privacy-safe diagnostics and verify the whitelist

**Files:**
- Modify/Test: `src-tauri/src/gateway/proxy/handler/failover_loop/response/upstream_error.rs`

- [ ] **Step 1: Extend the existing special setting**

Keep the existing keys and add only:

```json
{
  "encryptedContentPartsRemoved": 3,
  "emptyItemsRemoved": 1
}
```

Use `outcome.items_converted` for `itemsConverted`. Do not include request-body fragments or content values.

- [ ] **Step 2: Add a strict diagnostic-shape test**

Move construction into a small pure helper if required for direct testing. Assert the key set is exactly:

```text
action
emptyItemsRemoved
encryptedContentPartsRemoved
hit
itemsConverted
providerId
retryAttemptNumber
retryAttemptNumberNext
scope
status
type
```

Assert all three counts are non-negative JSON integers and assert forbidden keys are absent: `body`, `content`, `text`, `data`, `payload`, `request`. Also serialize the setting and assert it contains none of the request sentinels such as `KEEP_BEFORE`, `KEEP_AFTER`, `DROP_ONE`, or `DROP_TWO`, proving values do not leak request content.

- [ ] **Step 3: Run diagnostic tests**

```bash
cd src-tauri
CARGO_TARGET_DIR=target-tests cargo test --locked codex_agent_message_rectifier_special_setting
CARGO_TARGET_DIR=target-tests cargo test --locked agent_message
```

Expected: exact whitelist and all agent-message tests pass.

- [ ] **Step 4: Commit diagnostics**

```bash
git add src-tauri/src/gateway/proxy/handler/failover_loop/response/upstream_error.rs
git commit -m "test(codex): verify agent rectifier diagnostics"
```

### Task 5: Run related and complete Rust verification

**Files:**
- Verify: `src-tauri/src/gateway/proxy/handler/failover_loop/response/upstream_error.rs`
- Verify: full `src-tauri` crate

- [ ] **Step 1: Run formatting and diff checks**

```bash
cd src-tauri && cargo fmt --check
cd .. && git diff --check
```

- [ ] **Step 2: Run focused and neighboring rectifier suites**

```bash
cd src-tauri
CARGO_TARGET_DIR=target-tests cargo test --locked agent_message
CARGO_TARGET_DIR=target-tests cargo test --locked additional_tools
CARGO_TARGET_DIR=target-tests cargo test --locked reasoning_context
CARGO_TARGET_DIR=target-tests cargo test --locked previous_response_id
CARGO_TARGET_DIR=target-tests cargo test --locked provider_max_attempts_
CARGO_TARGET_DIR=target-tests cargo test --locked foreign_history
```

Expected: every selected suite exits 0 with zero failures.

- [ ] **Step 3: Run the complete Rust suite serially**

```bash
cd src-tauri
CARGO_TARGET_DIR=target-tests cargo test --locked -- --test-threads=1
```

Expected: exit 0; record passed/failed/ignored totals.

- [ ] **Step 4: Run Clippy with warnings denied**

```bash
cd src-tauri
CARGO_TARGET_DIR=target-tests cargo clippy --all-targets --locked -- -D warnings
```

Expected: exit 0 with no warnings.

- [ ] **Step 5: Re-run repository state checks**

```bash
git status --short --branch
git diff --check
```

Do not claim the bug fixed from unit/build evidence alone.

### Task 6: Run isolated real-session bidirectional regression

**Files/Artifacts:**
- Read-only originals: the source JSONL files for `019f74a5-085d-7241-b9f8-21efe6570919` and `019f51cc-b787-7c63-8708-bd4fa00ae004`
- Create/Delete: `/tmp/aio-encrypted-content-e2e-<run-id>/...`
- Do not modify: `/Applications/AIO Coding Hub Dev.app`, live port `37123`, live app DB/settings, or original session JSONL

- [ ] **Step 1: Record the protected baseline**

Record for both original JSONL files: absolute path, SHA-256, byte size, and nanosecond mtime. Record current Dev version, PID, port `37123`, and `/health` response.

- [ ] **Step 2: Create the explicit isolated root**

Create one unique root `/tmp/aio-encrypted-content-e2e-<run-id>` containing:

```text
app-home/
codex-home/sessions/YYYY/MM/DD/
database-copy/
logs/
evidence/
```

Copy the app database/settings and each target JSONL into this root. Generate
two fresh UUIDs. For each clone, replace the first
`session_meta.payload.id` with its fresh UUID and replace the original UUID in
the cloned filename with the same fresh UUID. Do not copy any global session
index/cache; let the isolated Codex home discover the clone from its own
`sessions/YYYY/MM/DD` tree. Every `codex resume` command must use only the fresh
UUID. After resume, read the opened clone's `session_meta.payload.id` and its
newly appended marker from the isolated file to prove the CLI opened that
clone. The original UUIDs may appear only as read-only source/evidence labels,
never as resume targets.

- [ ] **Step 3: Build/start an alternate test app without replacing Dev**

Use an alternate bundle identifier/data-dotdir and port `37124` (or another confirmed-free port). Explicitly set for the isolated process:

```bash
HOME=<run-root>/app-home
AIO_CODING_HUB_HOME_DIR=<run-root>/app-home
AIO_CODING_HUB_DOTDIR_NAME=.aio-coding-hub-encrypted-e2e
CODEX_HOME=<run-root>/codex-home
```

In the copied app settings, set `codex_home_mode=FollowCodexHome` and clear any
Codex home override before startup. Alternatively, if the current schema
requires `Custom`, set the override to the exact absolute
`<run-root>/codex-home`; do not leave the copied mode unchanged. Before
requests, read back the listener, PID environment/configuration, database path,
cloned session IDs, configured mode, and the application's resolved Codex home.
The resolved path must equal `<run-root>/codex-home` exactly. Confirm `37123`
still serves the installed Dev health endpoint.

- [ ] **Step 4: Test the complex legacy clone**

Use unique exact markers for each step and force the expected provider in the isolated copied DB:

1. GPT baseline: HTTP 200 and marker A.
2. GPT to LongCat: HTTP 200 and marker B.
3. LongCat to GPT: HTTP 200 and marker C; verify `foreign_history_handoff` when required.
4. GPT back to LongCat: HTTP 200 and marker D.

For each step verify the exact marker and `request_logs.final_provider_id`. For
the first complex GPT-to-LongCat transition, require exactly one
`codex_agent_message_rectifier`, verify its three counts, verify the corrected
attempt stays on the same provider, and confirm neither protocol error appears
again in corrected/final logs. Do not persist or log full request bodies merely
for E2E proof. The absence of converted `agent_message` and their
`encrypted_content` parts is proven by the transformer and retry-preparation
tests; E2E proves the resulting request is accepted and the marker continues.

- [ ] **Step 5: Test the independent passing control clone**

Repeat GPT to LongCat, LongCat to GPT, and GPT back to LongCat on the `019f51cc...` clone with different unique markers. Require HTTP 200, exact markers, and expected provider IDs. Do not require a rectifier event if the active window is already compatible.

- [ ] **Step 6: Prove protected state is unchanged**

Recompute SHA-256, byte size, and nanosecond mtime for both original JSONL files and require exact equality with Step 1. Recheck the installed Dev PID/port/health and confirm no live provider setting was changed.

- [ ] **Step 7: Clean only the isolated artifacts**

Stop the alternate process, confirm its port is no longer listening, and delete only the exact run root and alternate app bundle. Do not use globs and do not delete any user session directory.

- [ ] **Step 8: Record the evidence boundary**

Report separately:

- focused tests passing;
- full suite and Clippy passing;
- complex old-session clone switching both directions;
- control old-session clone not regressing;
- original sessions and current Dev unchanged.

Do not claim universal provider compatibility. A provider that accepts `agent_message` but directly rejects `encrypted_content` remains outside this verified design.

### Task 7: Final commit and handoff without deployment

**Files:**
- Verify: all changed files

- [ ] **Step 1: Review the final diff against the approved spec**

Check every requirement in `docs/superpowers/specs/2026-07-19-codex-agent-message-encrypted-content-design.md` and confirm no proactive sanitization, second matcher, session rewrite, or unrelated refactor was added.

- [ ] **Step 2: Commit any remaining test/evidence-safe source changes**

```bash
git add src-tauri/src/gateway/proxy/handler/failover_loop/response/upstream_error.rs
git commit -m "fix(codex): support encrypted legacy agent history"
```

Skip this commit if the working tree is already clean from the focused commits.

- [ ] **Step 3: Report current branch and commit IDs**

Report the implementation commits and verification evidence. Do not push, rebuild, replace, or restart `/Applications/AIO Coding Hub Dev.app` unless the user separately authorizes deployment after reviewing the isolated E2E result.
