# Codex Interleaved Tool History Compatibility Design

## Goal

Allow complex legacy Codex sessions to continue from GPT history through a
non-ChatGPT Responses provider such as LongCat when concurrent turns have
interleaved a `function_call` and its later `function_call_output` with other
assistant history.

The repair must preserve real tool calls and outputs, leave GPT OAuth requests
unchanged, avoid modifying stored session JSONL, and remain isolated to Codex.
Grok is explicitly out of scope.

## Verified Problem

The encrypted-content repair allows the complex legacy session snapshot based
on `019f74a5-085d-7241-b9f8-21efe6570919` to pass LongCat's request schema
checks. The corrected fourth physical request receives HTTP 200 after these
existing rectifiers run:

- remove `reasoning.context`;
- remove `additional_tools`;
- convert `agent_message` items and remove their `encrypted_content` parts.

The stream then fails with:

```text
invalid replay history:
assistant message appeared before completing tool responses
for call_CUkUduBupZ5H3B0qTwiSqora
```

The referenced call is not dangling. The source session contains both items:

```text
05:54:35.467  function_call         call_CUkUduBupZ5H3B0qTwiSqora
05:55:01.142  function_call_output  call_CUkUduBupZ5H3B0qTwiSqora
```

The real ordering between them is equivalent to:

```text
function_call A
function_call B
function_call_output B
reasoning or other assistant history
function_call_output A
```

Concurrent turns therefore produced a complete but interleaved tool
transaction. The strict replay validator rejects the assistant item that
appears while call A is still pending. Deleting only calls without an output
would not repair this sequence and would target the wrong condition.

## Scope

The normalizer runs only when all of these conditions hold:

- `cli_key == "codex"`;
- the forwarded request is a Responses API request;
- the selected target is not the official Codex ChatGPT OAuth backend;
- `input` is an array;
- the array contains a verified invalid `function_call` transaction boundary.

The following are out of scope:

- Grok requests;
- GPT OAuth request history;
- Claude protocol history;
- `custom_tool_call` and provider-specific tool item families;
- rewriting or repairing the source session JSONL;
- provider- or session-ID special cases;
- unrelated history compaction or chronological cleanup.

## Chosen Approach

Add a provider-attempt-local, proactive Codex Responses history normalizer. It
repairs only invalid `function_call` sequencing before the request is sent to a
non-ChatGPT Responses provider.

The normalizer treats one or more contiguous or interleaved function calls as
an open tool transaction. A transaction remains open until every observed
`call_id` has a corresponding `function_call_output`.

When a non-tool history barrier appears while calls remain pending:

1. find matching real outputs later in the same `input` array;
2. relocate those outputs immediately before the barrier;
3. preserve the real output JSON values unchanged;
4. preserve the relative order of relocated outputs as they appeared in the
   source array;
5. remove the outputs from their original later positions so no duplicates
   remain;
6. synthesize `function_call_output` with output text `aborted` only for calls
   that have no matching real output anywhere in the request.

If the array ends while calls are still pending, treat end-of-input as a
boundary and append one `aborted` output for each truly missing eligible call.

After repair, the example becomes:

```text
function_call A
function_call B
function_call_output B
function_call_output A
reasoning or other assistant history
```

This preserves both real tool results and creates the replay boundary required
by strict providers. It changes ordering only when the original ordering is
already invalid for the target protocol.

## History Classification

### Tool transaction items

- `function_call` opens a pending call keyed by its non-empty `call_id`.
- `function_call_output` closes the matching pending call.
- Multiple calls may be pending at the same time.
- Outputs may close pending calls in any order.

### Barriers

A barrier is any model-visible history item that starts or represents another
conversation step while one or more tool calls remain pending. At minimum this
includes:

- `reasoning`;
- `message`;
- `agent_message`.

Unsupported metadata such as `additional_tools` is not itself used to decide
tool completion ordering; its existing rectifier remains responsible for that
item family.

Unknown item types are treated conservatively as barriers. This prevents the
normalizer from moving a delayed tool output across an unrecognized semantic
history item without first closing the open transaction.

## Missing, Duplicate, And Malformed IDs

### Truly missing output

If a pending non-empty `call_id` has no matching real output anywhere later in
the request, insert exactly one synthetic item before the barrier:

```json
{
  "type": "function_call_output",
  "call_id": "call_example",
  "output": "aborted"
}
```

This follows Codex's own recovery meaning for an interrupted call and avoids
inventing a successful tool result.

### Empty or malformed call ID

A call or output without a non-empty string `call_id` is not repaired. The
normalizer records the malformed count and leaves the item unchanged so it
does not guess an identity.

### Duplicate call ID

Duplicate `function_call` IDs are ambiguous. The normalizer must not relocate
or synthesize outputs for that ID. It records the ambiguity and leaves those
items unchanged for normal provider error handling.

### Orphan output

An output with no matching call is outside the verified defect. It remains in
place and is not removed.

## Placement In The Request Flow

Run the normalizer after the selected provider attempt has its own decoded
request-body copy and after target identity is known, but before the physical
request is sent.

This placement provides these boundaries:

- the shared original request body remains unchanged;
- one provider candidate cannot mutate another candidate's request;
- ChatGPT OAuth detection can skip the normalizer explicitly;
- the repair happens before HTTP headers are committed, unlike a reactive
  repair triggered by an error embedded in an HTTP 200 stream;
- existing 400-based rectifiers can continue mutating the same attempt-local
  body on subsequent physical retries.

The normalizer does not consume a retry and does not add a retry state flag. It
is deterministic and idempotent: a second pass over the repaired body reports
no relocation or synthesis.

## Structured Diagnostics

When a body changes, append an attempt-scoped special setting with count-only
metadata:

```json
{
  "type": "codex_interleaved_tool_history_normalizer",
  "scope": "attempt",
  "hit": true,
  "action": "close_tool_transactions_before_history_barriers",
  "providerId": 30,
  "callsExamined": 2,
  "outputsRelocated": 1,
  "abortedOutputsSynthesized": 0,
  "barriersRepaired": 1,
  "malformedIdsSkipped": 0,
  "duplicateIdsSkipped": 0
}
```

Do not log call IDs, arguments, tool outputs, message text, complete request
bodies, credentials, or encrypted content.

If the scan finds no invalid boundary, do not add a hit diagnostic. Existing
request-log limits and approved special-setting key whitelists remain in force.

## Invariants

- GPT OAuth request bytes remain unchanged by this normalizer.
- Grok behavior and tests remain unchanged.
- Stored session JSONL is never modified.
- Legal Codex tool history remains semantically and byte-for-byte unchanged by
  the transformer result path.
- A no-op scan returns the original body bytes instead of parsing and
  reserializing them.
- Real call arguments and real output content are never edited.
- Calls are never deleted by this normalizer.
- Real outputs are never deleted except for removing the original copy after
  the same JSON value has been relocated.
- Non-tool history preserves its relative order.
- Relocated real outputs preserve their relative order.
- Synthetic output is used only when no real matching output exists anywhere
  in the request.
- Provider IDs, provider names, model names, and session IDs are not hardcoded.
- Invalid JSON, missing `input`, and non-array `input` are no-ops.
- The transformation is idempotent.

## Rejected Approaches

### Delete calls whose output appears too late

The verified call has a real output. Deleting the call and output would discard
valid tool execution evidence and reduce continuation quality.

### Replace delayed real outputs with `aborted`

`aborted` is appropriate only when no real result exists. Replacing a completed
result would fabricate failure and lose useful context.

### Trim the complete history span around the invalid transaction

Removing everything from the open call to the next consistent boundary could
discard many unrelated messages, reasoning items, and completed tool calls in
a complex legacy session.

### Repair after the HTTP 200 stream error

The failure arrives after the upstream has accepted the request and begun a
stream. At that point the gateway cannot safely guarantee a transparent retry
before downstream response state is committed. The request must be normalized
before sending.

### Apply the repair to every provider, including GPT OAuth

GPT already accepts the original history and is the source of the valid real
outputs. Mutating GPT requests would add risk without solving the verified
LongCat compatibility defect.

## Testing

### Pure transformer tests

1. Reproduce the verified `call A`, `call B`, `output B`, barrier, `output A`
   order and prove output A moves before the barrier.
2. Verify the calls, outputs, arguments, and output JSON values are unchanged.
3. Verify multiple pending calls and reverse-order outputs remain valid.
4. Verify a truly missing output produces exactly one `aborted` output before
   the barrier.
5. Verify no synthetic output is created when a real output exists later.
6. Verify legal call/output history is an exact no-op.
7. Verify non-tool history retains relative order.
8. Verify malformed IDs, duplicate IDs, and orphan outputs remain unchanged and
   are counted as skipped where applicable.
9. Verify invalid JSON and non-array input are no-ops.
10. Verify a second normalization pass is a no-op.

### Request preparation tests

1. Codex plus non-ChatGPT Responses provider applies the repair to the
   provider-attempt-local body.
2. Codex ChatGPT OAuth backend receives the original request unchanged.
3. Grok receives the original request unchanged.
4. Claude and non-Responses requests remain unchanged.
5. Another provider candidate receives an independent unmodified body.
6. Structured diagnostics contain only approved count fields and no content.

### Regression tests

Run the existing focused suites for:

- `agent_message` and encrypted content;
- `additional_tools`;
- `reasoning.context`;
- `previous_response_id` and foreign-history handoff;
- request preparation and retry-budget behavior.

Then run:

```bash
cargo test --locked -- --test-threads=1
cargo clippy --all-targets --locked -- -D warnings
```

### Isolated real-session E2E

Do not replace or restart `/Applications/AIO Coding Hub Dev.app` during the
implementation and initial verification.

Use a separately identified Tauri build, isolated application home, copied
database, isolated `CODEX_HOME`, and newly generated session UUIDs. Never run
the source legacy session directly.

Required scenarios:

1. Control legacy session: GPT, LongCat, GPT all return HTTP 200 with exact
   markers.
2. Complex legacy snapshot: GPT baseline returns HTTP 200.
3. Same complex clone: GPT to LongCat returns HTTP 200 and the exact marker;
   request logs show the interleaved-history normalizer and all required
   strict-schema rectifiers.
4. Same complex clone: LongCat back to GPT returns HTTP 200 with the exact
   marker.
5. Confirm the original static session snapshots were not modified.
6. Confirm the installed Dev PID, port, version, and health remain unchanged.

Passing unit tests and builds are not sufficient. The fix is accepted only
after the complex legacy clone completes the real GPT to LongCat to GPT flow.

## Completion Criteria

The implementation is ready for integration only when:

- focused tests, full Rust tests, and clippy pass;
- the isolated control session still passes both directions;
- the isolated complex legacy clone passes GPT to LongCat to GPT;
- diagnostics prove whether real outputs were relocated or missing outputs
  were synthesized without exposing content;
- GPT OAuth and Grok request preparation tests prove zero mutation;
- the current installed Dev was not changed during validation;
- independent code review reports no critical or important issues.
