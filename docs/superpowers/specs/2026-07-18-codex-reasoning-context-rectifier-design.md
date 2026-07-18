# Codex `reasoning.context` Compatibility Rectifier Design

## Goal

Allow a Codex Responses request to fail over from a provider that supports
`reasoning.context` to a strict OpenAI-compatible provider such as LongCat
without returning `unsupported_field`, while leaving compatible providers and
successful requests byte-for-byte unchanged.

## Verified Problem

Codex CLI 0.144.4 sends a top-level object shaped like:

```json
{
  "reasoning": {
    "effort": "low",
    "context": "all_turns"
  }
}
```

Direct isolated probes against LongCat produced these results:

- preserving `reasoning.context`: HTTP 400 with
  `code=unsupported_field` and `param=reasoning.context`;
- removing only `reasoning.context`: HTTP 200;
- removing the complete `reasoning` object: HTTP 200, but unnecessarily loses
  the supported `reasoning.effort` setting.

The current API-key provider preparation only applies model mapping, and the
generic non-Claude sanitizer returns the body unchanged. The current Codex 400
rectification path handles `previous_response_id`, but not unsupported nested
request fields.

## Chosen Approach

Use a provider-agnostic, reactive rectifier. Do not proactively strip the
field and do not hard-code LongCat.

The rectifier activates only when all of these conditions are true. Top-level
`code`/`param` values and differently wrapped errors do not match.

1. the client protocol is `codex` or `grok` Responses;
2. the upstream status is HTTP 400;
3. the bounded upstream JSON error has `/error/code` equal to
   `unsupported_field`;
4. the same error object has `/error/param` equal to `reasoning.context`;
5. the current upstream request body contains that nested field;
6. this provider attempt has not already applied this rectifier.

When activated, it removes only `reasoning.context`, preserves every other
semantic JSON value and all array ordering, marks the content encoding for
regeneration, and retries the same provider exactly once. JSON object key order
is not contractual and may be canonicalized during re-encoding. Requests that
succeed without rectification remain byte-for-byte unchanged. If removing
`context` leaves an empty `reasoning` object, the empty top-level object may be
removed as a defensive cleanup; otherwise the object and fields such as
`effort` remain intact.

## Rejected Approaches

### Proactively strip from every API-key provider

This would avoid the first 400 but would silently downgrade providers that
support `reasoning.context`. The isolated compatible-provider simulation proved
that reactive handling can preserve the original request and complete in one
attempt.

### Hard-code provider 30 or LongCat hostnames

This fixes only one provider and couples protocol compatibility to mutable local
provider identity. Other strict providers would continue to fail.

### Remove the complete `reasoning` object

LongCat accepts it, but the direct probe showed that retaining `effort` while
removing only `context` also succeeds. Removing more data is unnecessary.

## Components

### Error matcher and body rectifier

Add small pure helpers near the existing Codex `previous_response_id`
rectifier. The matcher parses only the bounded upstream error body and requires
the exact structured `code` and `param` pair. It must not trigger from an error
message containing similar text when the structured fields differ.

The body rectifier parses the outbound request JSON, removes the one nested
field, preserves all sibling/top-level semantic values and array ordering, and
reports whether it changed the body. Object key ordering is not a protocol
requirement. Invalid JSON and missing/non-object `reasoning` are no-ops.

### Per-provider retry state

Add a dedicated boolean to `RetryLoopState` and thread a mutable reference
through `UpstreamRequestState`. The flag is scoped to one provider retry loop,
so a malformed or repeated response cannot cause an infinite retry.

Provider preparation must also detect a Codex/Grok Responses request that
contains `reasoning.context` and reserve one internal retry in
`provider_max_attempts_for_request`, alongside the existing OAuth-refresh and
`previous_response_id` reserves. A normal provider configured with
`max_attempts_per_provider=1` therefore receives up to two physical attempts:
the original request and, only after the exact structured 400, one corrected
request. This internal reserve is not added to strict model-discovery requests,
which continue to honor their exact configured limit.

### Diagnostics

On a successful rectification, append one internal special setting:

```json
{
  "type": "codex_reasoning_context_rectifier",
  "scope": "attempt",
  "hit": true,
  "action": "remove_reasoning_context_and_retry",
  "providerId": 30,
  "status": 400,
  "retryAttemptNumber": 1,
  "retryAttemptNumberNext": 2
}
```

The provider ID is diagnostic metadata, not matching logic. Do not record
request text, reasoning contents, credentials, or the complete request body.

## Error Handling

- An unmatched 400 follows the existing classification and abort/failover
  behavior.
- A matching error with no removable request field is not retried.
- A second matching response after rectification is not retried again.
- Invalid or oversized error bodies do not trigger the rectifier.
- Existing quota, circuit-breaker, `previous_response_id`, and LongCat-to-GPT
  foreign-history behavior remain unchanged.

## Test Strategy

Follow TDD and prove the regression test fails before implementation.

1. Pure matcher tests for the exact `/error/code` and `/error/param` values,
   plus top-level, wrapped, and similarly worded near misses.
2. Pure body tests proving only `reasoning.context` is removed, `effort`, input,
   tools, and unrelated fields are preserved, and a second pass is a no-op.
3. Retry-state and attempt-budget tests proving at most one rectification retry
   and proving a normal provider limit of one still permits the corrected
   second attempt.
4. Route-level mock test with the ordinary provider limit set to one: first
   upstream response returns the real LongCat 400;
   the second validates the corrected body and returns 200. Assert two attempts,
   the special setting, and no circuit failure.
5. Regression tests for `previous_response_id` and LongCat-to-GPT handoff.
6. Full Rust and frontend checks required by the repository.
7. Build an isolated Dev bundle and run a fresh real GPT-to-LongCat Codex
   session before replacing the installed app.

## Runtime Isolation

Implementation and automated tests must not stop or restart the currently
running `/Applications/AIO Coding Hub Dev.app`. Live validation must use an
isolated build process or a separate loopback port and must snapshot and restore
any temporary provider, route, or circuit state. Only after all tests pass will
the installed Dev bundle be replaced, and replacing files on disk must not kill
the current process.

## Success Criteria

- Providers that accept `reasoning.context` receive the original request once.
- A strict provider returning the exact LongCat error receives one corrected
  retry and can return 200.
- The corrected retry preserves `reasoning.effort` and all unrelated payload
  data.
- No request can enter an infinite rectification loop.
- Existing LongCat-to-GPT handoff tests remain green.
- A fresh real GPT-to-LongCat session succeeds on the built version without
  disrupting the user's running Dev application.
