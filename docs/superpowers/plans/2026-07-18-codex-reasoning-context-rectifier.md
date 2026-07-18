# Codex `reasoning.context` Rectifier Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Retry a strict Responses provider once after it explicitly rejects `reasoning.context`, removing only that nested field while preserving compatible-provider behavior.

**Architecture:** Extend the existing Codex retry-rectifier pattern in the failover loop. Provider preparation reserves one internal attempt only when a Codex/Grok Responses body contains `reasoning.context`; the bounded 400 handler matches the exact `/error/code` and `/error/param`, mutates the prepared outbound body, records diagnostics, and retries the same provider once.

**Tech Stack:** Rust, Axum, Reqwest, Serde JSON, Tauri test runtime, SQLite request logs, Codex CLI black-box tests.

---

## File Map

- Modify `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/provider_iterator.rs`: detect the nested field and reserve one internal attempt.
- Modify `src-tauri/src/gateway/proxy/handler/failover_loop/attempt/attempt_executor.rs`: hold one-shot retry state.
- Modify `src-tauri/src/gateway/proxy/handler/failover_loop/response/response_router.rs`: thread retry state into error handling.
- Modify `src-tauri/src/gateway/proxy/handler/failover_loop/response/upstream_error.rs`: exact matcher, minimal mutation, retry, and diagnostics.
- Modify `src-tauri/src/gateway/proxy/handler/failover_loop/response/thinking_signature_rectifier_400.rs`: consume the bounded-reader body/truncation result without changing Claude rectifier behavior.
- Modify `src-tauri/src/gateway/routes.rs`: route-level strict and compatible upstream regression tests.

### Task 1: Create an isolated implementation worktree

**Files:**
- Read: `.gitignore`
- Read: repository worktree configuration

- [ ] **Step 1: Invoke the worktree workflow**

Read and follow `using-git-worktrees`. Prefer the repository's ignored `.worktrees/` parent if it is configured; otherwise use the recommended global location.

- [ ] **Step 2: Create the implementation branch**

Create `fix/codex-reasoning-context-rectifier` from approved spec commit `0f47933c`. Do not switch or alter the original workspace.

- [ ] **Step 3: Verify isolation**

Run `git status -sb` and `git rev-parse HEAD` in both workspaces. Expected: original workspace remains on `feat/cc2cx-session-titles`; worktree is on the fix branch; both begin at `0f47933c`.

### Task 2: Reserve the internal rectifier attempt

**Files:**
- Modify/Test: `src-tauri/src/gateway/proxy/handler/failover_loop/prepare/provider_iterator.rs`

- [ ] **Step 1: Write failing request-shape tests**

Add tests for the desired helper, including the Responses path:

```rust
#[test]
fn codex_body_reasoning_context_requires_internal_retry_budget() {
    let body = serde_json::to_vec(&serde_json::json!({
        "reasoning": {"effort": "low", "context": "all_turns"}
    })).unwrap();
    assert!(codex_body_has_reasoning_context("codex", "/v1/responses", &body));
    assert!(codex_body_has_reasoning_context("grok", "/responses", &body));
    assert!(!codex_body_has_reasoning_context("codex", "/v1/models", &body));
    assert!(!codex_body_has_reasoning_context("claude", "/v1/responses", &body));
}
```

Also cover invalid JSON, absent `context`, and non-object `reasoning`.

- [ ] **Step 2: Write failing budget tests**

Extend the wished-for function signature with `needs_codex_reasoning_context_retry` and assert:

```rust
assert_eq!(provider_max_attempts_for_request(1, 1, false, false, true, false), 2);
assert_eq!(provider_max_attempts_for_request(1, 1, false, false, true, true), 1);
```

The final boolean remains the strict model-discovery flag. Update existing test calls with `false` for the new reasoning-context argument.

- [ ] **Step 3: Run focused tests and verify RED**

```bash
cd src-tauri
cargo test --locked codex_body_reasoning_context
cargo test --locked provider_max_attempts_reserves_reasoning_context
```

Expected: failure because the helper/signature do not yet exist.

- [ ] **Step 4: Implement the minimal detector and budget reservation**

Implement a JSON parser limited to `codex | grok` **and**
`is_responses_request_path(forwarded_path)` that returns true only for an
object-valued `reasoning` containing the `context` key. Include the new boolean
in `required_internal_retries`, preserving the strict model-discovery early
return. Pass the detector result and request path at the provider preparation
call site.

- [ ] **Step 5: Verify GREEN and neighboring behavior**

```bash
cd src-tauri
cargo test --locked provider_max_attempts_
cargo test --locked codex_request_has_previous_response_id
cargo test --locked codex_body_reasoning_context
```

Expected: all selected tests pass.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/gateway/proxy/handler/failover_loop/prepare/provider_iterator.rs
git commit -m "fix(codex): reserve reasoning context retry"
```

### Task 3: Add exact matching and minimal body mutation

**Files:**
- Modify/Test: `src-tauri/src/gateway/proxy/handler/failover_loop/response/upstream_error.rs`
- Modify/Test: `src-tauri/src/gateway/proxy/handler/failover_loop/response/thinking_signature_rectifier_400.rs`

- [ ] **Step 0: Write failing bounded-reader truncation tests**

Change the wished-for reader result to expose both `body: Bytes` and
`truncated: bool`. Add tests for a known oversized content length, an unknown
length stream exceeding the limit, a body exactly at the limit, and a body
below the limit. The first two must set `truncated=true`; the latter two must
set `false`. Run `cargo test --locked error_body_read_reports_truncation` and
verify RED before changing the reader.

- [ ] **Step 1: Write failing matcher tests**

Require HTTP 400 plus exact JSON pointers:

```rust
#[test]
fn matches_exact_reasoning_context_unsupported_field() {
    let body = br#"{"error":{"message":"unknown field in strict mode: 'reasoning.context'","type":"invalid_request_error","param":"reasoning.context","code":"unsupported_field"}}"#;
    assert!(matches_codex_reasoning_context_error(StatusCode::BAD_REQUEST, body));
}
```

Add near misses for HTTP 422, top-level `code`/`param`, a differently wrapped error, wrong code, wrong param, message-only text, invalid JSON, and empty body.

- [ ] **Step 2: Write failing mutation tests**

```rust
#[test]
fn removes_only_reasoning_context_and_preserves_payload() {
    let original = serde_json::json!({
        "model": "LongCat-2.0",
        "reasoning": {"effort": "low", "context": "all_turns"},
        "input": [{"role": "user", "content": [{"type": "input_text", "text": "KEEP"}]}],
        "tools": [{"type": "function", "name": "lookup"}],
        "store": false
    });
    let mut body = Bytes::from(serde_json::to_vec(&original).unwrap());
    assert!(remove_codex_reasoning_context(&mut body));
    let next: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(next["reasoning"], serde_json::json!({"effort": "low"}));
    assert_eq!(next["input"], original["input"]);
    assert_eq!(next["tools"], original["tools"]);
    assert!(!remove_codex_reasoning_context(&mut body));
}
```

Add cases for an empty `reasoning` object after removal, invalid JSON, non-object reasoning, and missing context.

- [ ] **Step 3: Run tests and verify RED**

```bash
cd src-tauri
cargo test --locked matches_exact_reasoning_context_unsupported_field
cargo test --locked removes_only_reasoning_context_and_preserves_payload
```

Expected: missing helper failures.

- [ ] **Step 4: Implement the pure helpers**

The matcher must use `root.pointer("/error/code")` and `root.pointer("/error/param")`. The mutator removes only `context`, removes the top-level `reasoning` object only if it becomes empty, serializes back to `Bytes`, and returns true only after a real removal. Assert semantic equality for all sibling and top-level values plus exact array ordering; do not assert JSON object key order because it is not contractual and serde_json may canonicalize it.

Update `read_response_body_with_limit` and
`read_response_body_for_error_scan` to return the body plus a truncation flag
without buffering beyond the configured maximum. For unknown content length,
read at most one extra chunk to determine whether more bytes exist. Thread the
flag through both `upstream_error.rs` and
`thinking_signature_rectifier_400.rs`; existing consumers continue to inspect
the bounded body. The new reasoning-context matcher must be gated by
`truncated == false`, so an oversized error never triggers even if its prefix is
valid JSON.

- [ ] **Step 5: Verify GREEN and commit**

```bash
cd src-tauri
cargo test --locked reasoning_context
git add src/gateway/proxy/handler/failover_loop/response/upstream_error.rs src/gateway/proxy/handler/failover_loop/response/thinking_signature_rectifier_400.rs
git commit -m "fix(codex): detect unsupported reasoning context"
```

### Task 4: Wire one-shot same-provider retry

**Files:**
- Modify: `src-tauri/src/gateway/proxy/handler/failover_loop/attempt/attempt_executor.rs`
- Modify: `src-tauri/src/gateway/proxy/handler/failover_loop/response/response_router.rs`
- Modify/Test: `src-tauri/src/gateway/proxy/handler/failover_loop/response/upstream_error.rs`

- [ ] **Step 1: Write a failing one-shot decision test**

Prove the first exact 400 with a removable field returns `ContinueRetry`, sets retry state, and mutates the body; a second identical 400 and a matching error without a removable field do not retry.

- [ ] **Step 2: Run and verify RED**

```bash
cd src-tauri
cargo test --locked reasoning_context_rectifier_retries_once
```

- [ ] **Step 3: Thread retry state**

Add `codex_reasoning_context_rectifier_retried: bool` to `RetryLoopState`, initialize it false, add a mutable reference to `UpstreamRequestState`, and pass it in `response_router.rs` beside the existing previous-response rectifier flag.

- [ ] **Step 4: Add the handler branch**

After bounded error-body capture and before unmatched-400 abort, require
`codex | grok`, `is_responses_request_path(ctx.forwarded_path)`, an untruncated
error body, false retry state, the exact matcher, and successful removal. Then
set the flag and `strip_request_content_encoding`, append exactly these fields
and no request content: `type=codex_reasoning_context_rectifier`,
`scope=attempt`, `hit=true`,
`action=remove_reasoning_context_and_retry`, `providerId`, `status`,
`retryAttemptNumber`, and `retryAttemptNumberNext`. Return `ContinueRetry` and
do not record a circuit failure for this repaired attempt.

- [ ] **Step 5: Verify GREEN and regressions**

```bash
cd src-tauri
cargo test --locked reasoning_context_rectifier
cargo test --locked previous_response_id_rectifier
```

- [ ] **Step 6: Commit**

```bash
git add src/gateway/proxy/handler/failover_loop/attempt/attempt_executor.rs src/gateway/proxy/handler/failover_loop/response/response_router.rs src/gateway/proxy/handler/failover_loop/response/upstream_error.rs
git commit -m "fix(codex): retry unsupported reasoning context"
```

### Task 5: Prove route-boundary behavior

**Files:**
- Modify/Test: `src-tauri/src/gateway/routes.rs`

- [ ] **Step 1: Write the failing strict-upstream test**

Add a sequential mock that captures two request bodies, returns the real LongCat 400 first, validates the corrected second body, then returns 200. Configure ordinary provider attempts to one. Assert final 200, exactly two calls to the same provider, preserved `effort`/input/tools, one special setting, and no circuit failure.

- [ ] **Step 2: Run and verify RED**

```bash
cd src-tauri
cargo test --locked mock_runtime_router_reasoning_context_rectifier_retries_same_provider
```

Expected before wiring: final 400 or one upstream call.

- [ ] **Step 3: Add the compatible-provider test**

Use a capturing 200 upstream and assert one call, original `reasoning.context`, and no rectifier metadata. This proves reserved budget never creates a gratuitous retry.

- [ ] **Step 4: Verify GREEN and handoff regressions**

```bash
cd src-tauri
cargo test --locked mock_runtime_router_reasoning_context
cargo test --locked foreign_history
cargo test --locked responses_400_schema_mismatch
cargo test --locked previous_response_id
```

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/gateway/routes.rs
git commit -m "test(codex): cover reasoning context rectification"
```

### Task 6: Run full repository verification

**Files:**
- No intentional changes

- [ ] **Step 1: Review scope**

```bash
git diff --check
git status --short
git diff 0f47933c...HEAD --stat
git diff 0f47933c...HEAD -- src-tauri/src/gateway/proxy src-tauri/src/gateway/routes.rs
```

Expected: only planned implementation/test files.

- [ ] **Step 2: Run formatting, lint, Rust, and frontend gates**

```bash
cd src-tauri && cargo fmt --check
cd src-tauri && cargo clippy --all-targets --locked -- -D warnings
pnpm tauri:test
pnpm test:unit
pnpm check:generated-bindings
pnpm tauri:clippy
```

Expected: every command exits 0. Do not start, stop, or restart the installed Dev app.

### Task 7: Build and validate in isolation

**Files:**
- Build output only in the isolated worktree
- Temporary app data outside `~/.aio-coding-hub`

- [ ] **Step 1: Capture installed Dev invariants**

Record `/health`, listener PID, provider 12/30 rows, Codex routes, and circuit rows. Do not terminate that PID.

- [ ] **Step 2: Build from the worktree root**

The overlay is intentionally gitignored and will not exist in a fresh
worktree. Before building, create `.local/` in the worktree and copy only
`.local/tauri.build.dev.json` from the original workspace. Compare its SHA-256
digest to the original and verify it contains product name
`AIO Coding Hub Dev`, identifier `io.aio.codinghub.dev`, and
`createUpdaterArtifacts=false`. Do not copy any other ignored file.

```bash
pnpm tauri:build -- -c .local/tauri.build.dev.json
```

Do not copy the bundle to `/Applications` yet.

- [ ] **Step 3: Start an isolated runtime**

Use a temporary HOME/app-data directory and separate port such as 37125. Never point it at `~/.aio-coding-hub/aio-coding-hub.db`.

- [ ] **Step 4: Run deterministic GPT-to-strict-provider validation**

Create a fresh Codex session through a compatible GPT-like stub that accepts `reasoning.context`; resume it through a strict LongCat-like stub that returns the exact 400 once and 200 after correction. Assert marker continuity, two strict-provider attempts, and rectifier metadata.

- [ ] **Step 5: Run real LongCat confirmation**

With isolated provider state, send the captured Codex 0.144.4 shape to real LongCat through the isolated runtime. Verify internal correction and final 200. Restore all temporary state in a guaranteed cleanup path.

- [ ] **Step 6: Regress LongCat-to-GPT**

Use a new marker session and verify the reverse direction still records `foreign_history_handoff`, removes plaintext reasoning, and succeeds. If GPT quota is unavailable, run the deterministic GPT-compatible stub plus the existing real-session evidence boundary explicitly.

- [ ] **Step 7: Prove installed Dev stayed untouched**

Recheck health, PID, provider/route/circuit snapshots, Git state, and isolated port shutdown. Expected: exact baseline restoration.

### Task 8: Review, integrate, push, and update the bundle on disk

**Files:**
- Git integration
- `/Applications/AIO Coding Hub Dev.app` only after all gates pass

- [ ] **Step 1: Request code review**

Invoke `requesting-code-review` for `0f47933c...HEAD`; fix only verified issues and rerun affected tests.

- [ ] **Step 2: Merge into the original feature branch**

Confirm the original workspace is clean and at the expected spec/plan commits, then merge `fix/codex-reasoning-context-rectifier` without rewriting history.

- [ ] **Step 3: Push normally**

Push `feat/cc2cx-session-titles` through mandatory hooks. Use authenticated HTTPS only if the SSH deploy key lacks write access. Never force-push.

- [ ] **Step 4: Replace and sign the installed bundle without killing it**

Copy the verified app bundle onto `/Applications/AIO Coding Hub Dev.app`, ad-hoc sign, and run `codesign --verify --deep --strict --verbose=2`. Verify bundle ID and version. Do not quit or kill the active process; the new binary loads on the user's next normal restart.

- [ ] **Step 5: Report evidence**

Report commit/push hashes, test counts, isolated session IDs, rectifier metadata, restoration proof, installed signature/metadata, and unchanged active PID.
