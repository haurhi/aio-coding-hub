# Codex `agent_message` Encrypted Content Compatibility Design

## Goal

Allow complex legacy Codex sessions to continue from GPT-compatible history to
strict Responses providers such as LongCat when historical `agent_message`
items contain private `encrypted_content` content parts, without changing GPT
requests, successful provider requests, or the stored session JSONL.

## Verified Problem

The current strict-provider compatibility path reacts to this LongCat error:

```json
{
  "error": {
    "message": "unsupported input item type: agent_message",
    "type": "invalid_request_error",
    "param": "input[482].type",
    "code": "invalid_request"
  }
}
```

It converts every matching item from:

```json
{
  "type": "agent_message",
  "content": [
    { "type": "input_text", "text": "visible result" },
    { "type": "encrypted_content", "data": "opaque" }
  ]
}
```

to:

```json
{
  "type": "message",
  "role": "user",
  "content": [
    { "type": "input_text", "text": "visible result" },
    { "type": "encrypted_content", "data": "opaque" }
  ]
}
```

The second LongCat attempt then fails with:

```json
{
  "error": {
    "message": "unsupported content part type: encrypted_content",
    "type": "invalid_request_error",
    "param": "input[482].content[1].type",
    "code": "invalid_request"
  }
}
```

This was reproduced twice with a snapshot of legacy session
`019f74a5-085d-7241-b9f8-21efe6570919` on the current `0.60.15` code at commit
`795de582`. The request log showed that the first rectifier converted 32
`agent_message` items, but preserved their `encrypted_content` parts. GPT could
continue the same snapshot with HTTP 200, proving that the failure belongs to
strict-provider history normalization rather than session corruption or OAuth.

An independent legacy session snapshot,
`019f51cc-b787-7c63-8708-bd4fa00ae004`, completed GPT to LongCat, LongCat to
GPT, and GPT back to LongCat with HTTP 200. The defect is therefore
shape-dependent: it occurs when the active request window contains converted
`agent_message` items with `encrypted_content` content parts.

## Chosen Approach

Extend the existing reactive `agent_message` rectifier. When the exact
structured `agent_message` rejection activates the rectifier, convert the item
and sanitize its content in the same mutation and retry budget.

For each `input` item whose `type` is exactly `agent_message`:

1. require `content` to be an array, as today;
2. remove only content parts whose `type` is exactly `encrypted_content`;
3. preserve all remaining content parts at the JSON-value level and in their
   original order;
4. if at least one content part remains, convert the item to
   `{"type":"message","role":"user","content":[...]}`;
5. if no content part remains, remove the whole history item instead of
   emitting an empty message;
6. preserve all non-`agent_message` input items and unrelated top-level fields
   in their original semantic order and shape.

The sanitizer is not a proactive LongCat-specific rewrite. It runs only after
an upstream provider has returned the exact structured
`unsupported input item type: agent_message` error already recognized by the
existing rectifier. GPT and providers that accept the original request remain
unchanged.

No second `encrypted_content` retry state is added. The existing internal
`agent_message` retry already provides exactly one corrected physical attempt;
the corrected body must be complete before that attempt is sent.

## Why `encrypted_content` Can Be Removed

The content is opaque provider-private state and cannot be interpreted or
translated by AIO Coding Hub. LongCat explicitly rejects it. The accompanying
visible `input_text` is the portable semantic history needed for continuation.
Copying, decrypting, logging, or re-roling the opaque payload is not allowed.

If an `agent_message` contains only opaque content, removing the whole item is
safer than sending an empty message or fabricating placeholder text.

## Rejected Approaches

### Add a separate error-driven `encrypted_content` retry

This would require another matcher, per-provider retry flag, pending-retry
flag, retry-budget reserve, and diagnostics path. It fixes a body that the
existing rectifier just created instead of making the first rectification
complete. The extra retry and state are unnecessary for the verified defect.

### Strip `encrypted_content` from every Responses request before send

This would mutate successful requests and providers that support the content
part. It also broadens the behavior beyond the exact strict-provider failure
that has been reproduced.

### Drop the complete `agent_message`

This would avoid the LongCat error but discard portable visible text. The
existing behavior intentionally preserves agent results as user history, so
only the opaque incompatible content part should be removed.

### Replace the opaque content with placeholder text

Fabricated text changes transcript semantics and can influence the model. No
replacement is needed when visible content remains, and an opaque-only item can
be safely omitted.

## Components

### Pure body transformer

Replace the current count-only return value from
`convert_codex_agent_messages_to_user_messages` with structured outcome
metadata containing at least:

- `items_converted`;
- `encrypted_content_parts_removed`;
- `empty_items_removed`.

The count semantics are exact:

- `items_converted` counts only items that produce a surviving
  `message/user`;
- `encrypted_content_parts_removed` counts every removed content part;
- `empty_items_removed` counts converted source items that are removed because
  no portable content remains;
- `changed()` is true when `items_converted > 0 || empty_items_removed > 0`.

`maybe_rectify_codex_agent_messages` must use the explicit `changed()` outcome,
not `items_converted > 0`, to decide whether to set the one-shot flag and retry.
An encrypted-only request can therefore report `items_converted=0` and
`empty_items_removed=1` while still committing the transformed body and
triggering the corrected attempt.

The helper remains a pure JSON-body transformer. Invalid JSON, non-array
`input`, non-array content, and bodies without convertible items remain no-ops.
Re-encoding may canonicalize JSON object key order; array ordering and semantic
values are contractual.

### Existing reactive matcher and retry

Keep `matches_codex_agent_message_error`, the one-shot retry flag, and the
existing retry-budget reserve unchanged. The matcher must continue requiring
the exact nested error object, HTTP 400, `code=invalid_request`, and numeric
`input[N].type` parameter.

### Diagnostics

Extend the existing special setting without logging request content:

```json
{
  "type": "codex_agent_message_rectifier",
  "scope": "attempt",
  "hit": true,
  "action": "convert_agent_messages_to_user_messages_and_retry",
  "providerId": 30,
  "status": 400,
  "retryAttemptNumber": 1,
  "retryAttemptNumberNext": 2,
  "itemsConverted": 32,
  "encryptedContentPartsRemoved": 32,
  "emptyItemsRemoved": 0
}
```

Counts are safe diagnostics. Do not log encrypted payloads, visible request
text, credentials, or complete bodies.

The diagnostic serializer uses an approved field whitelist. Its key set is
limited to `type`, `scope`, `hit`, `action`, `providerId`, `status`,
`retryAttemptNumber`, `retryAttemptNumberNext`, `itemsConverted`,
`encryptedContentPartsRemoved`, and `emptyItemsRemoved`. Count fields must be
non-negative integers. Fields named `body`, `content`, `text`, `data`,
`payload`, or `request` are forbidden.

## Error Handling And Invariants

- The stored Codex session JSONL is never modified by the gateway rectifier.
- The shared request body is not mutated across provider candidates; only the
  current provider attempt body is changed.
- The transformation is idempotent: after conversion, another call finds no
  `agent_message` item and is a no-op.
- Non-`agent_message` items, including function calls and outputs, retain their
  order.
- Non-encrypted content parts retain their order and complete JSON value.
- A malformed or non-array `content` remains unchanged and is not counted as a
  conversion. That malformed shape is outside this narrowly verified repair;
  if a provider rejects it, normal error handling remains responsible.
- The existing one-shot retry prevents loops.
- A different upstream error remains handled by normal error classification.

## Testing

### Red-green unit tests

1. Add a failing test with `input_text + encrypted_content` and verify the
   converted message keeps only `input_text`.
2. Add a failing test with multiple portable content parts around multiple
   encrypted parts and verify portable parts remain in order.
3. Add a failing test with encrypted-only content and verify the whole item is
   removed.
4. Verify non-`agent_message` items containing similarly named fields are not
   changed.
5. Verify structured outcome counts for converted items, removed encrypted
   parts, and removed empty items.
6. Add a rectifier-level encrypted-only test that proves `changed()` sets the
   one-shot retry flag, marks a pending retry, and produces
   `ContinueRetry` even when `items_converted=0`.
7. Add a mixed-input test with one surviving converted item, one
   encrypted-only removed item, and one non-agent item; verify counts and final
   ordering.
8. Preserve the existing content-preservation, idempotency, exact matcher, and
   one-shot retry tests.
9. Verify the diagnostics key set exactly matches the approved whitelist and
   contains no opaque or visible content.

The new tests must fail against commit `795de582` for the expected missing
sanitization before implementation changes are made.

### Focused and full verification

Run:

- focused `agent_message` and `encrypted_content` Rust tests;
- related `additional_tools`, `reasoning_context`, and provider retry-budget
  tests;
- `cargo fmt --check`;
- the full Rust test suite;
- Clippy with warnings denied;
- repository diff checks.

### Isolated live regression

Use the current code in an alternate Bundle ID, isolated database copy, and
isolated Codex home so the installed Dev, provider settings, and original
session JSONL files are not changed.

The test harness must:

1. create one explicit isolated root, such as
   `/tmp/aio-encrypted-content-e2e-<run-id>`;
2. place the copied application database and settings under that root;
3. create an isolated Codex home under the same root and copy each target JSONL
   into its corresponding `sessions/YYYY/MM/DD` path;
4. assign a new session ID inside each clone and use that clone ID for every
   resume command;
5. set `CODEX_HOME` for every Codex CLI invocation and set the isolated app's
   `HOME`/`AIO_CODING_HUB_HOME_DIR` plus `AIO_CODING_HUB_DOTDIR_NAME` explicitly;
6. read back the isolated app listener, database path, session IDs, and process
   environment/configuration before sending requests;
7. record hash, byte size, and modification time for both original JSONL files
   before and after the test and require exact equality;
8. clean only the explicit run root and alternate app bundle, never a glob or
   user session directory.

For a snapshot of complex legacy session `019f74a5...`:

1. GPT baseline returns HTTP 200 and its unique exact marker.
2. GPT to LongCat returns HTTP 200 and a different unique exact marker.
3. LongCat to GPT returns HTTP 200 and a third unique exact marker, with
   `foreign_history_handoff` when
   required.
4. GPT back to LongCat returns HTTP 200 and a fourth unique exact marker.
5. Each step's request log records the expected forced provider ID.
6. The first complex GPT to LongCat request records exactly one
   `codex_agent_message_rectifier`; its three counts agree with the transformed
   request shape.
7. The corrected attempt contains neither an `agent_message` item nor an
   `encrypted_content` content part originating from converted agent history.
8. No corrected attempt or final request log contains
   `unsupported input item type: agent_message` or
   `unsupported content part type: encrypted_content`.

Repeat the three-direction sequence on independent legacy session
`019f51cc...` with unique per-step markers to ensure the existing passing path
remains passing. That control path is not required to emit rectifier counts;
absence of the rectifier is acceptable when its active request window already
contains only compatible history.

This design does not add a matcher for a provider that accepts
`agent_message` but directly rejects an `encrypted_content` part. That shape
has not been observed in the verified failure chain. If it appears later, it
requires separate evidence and design rather than broadening this repair by
assumption.

## Rollout And Recovery

- Do not replace `/Applications/AIO Coding Hub Dev.app` until unit, full-suite,
  Clippy, and isolated live regressions pass.
- Keep the installed Dev process on port `37123`; use an alternate app
  identifier, data directory, and port for E2E.
- Temporarily enable providers only in the copied database.
- Stop the isolated process and delete its temporary app, database, and session
  clones after evidence is collected.
- If verification fails, leave the installed Dev and live provider settings
  unchanged and report the exact remaining protocol shape.
- Installing or replacing `/Applications/AIO Coding Hub Dev.app` is outside
  this design's completion scope. It requires a separate user-authorized
  deployment step after the isolated compatibility evidence is accepted.
