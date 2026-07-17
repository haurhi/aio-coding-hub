# Shape-Aware Codex Provider Handoff Design

**Status:** Approved for specification on 2026-07-17

## Problem

A Codex session can continue from ikuncode or the Xunfei `cc2cx` provider to a ChatGPT OAuth provider without rewriting history. A LongCat session cannot: ChatGPT returns `array_above_max_length` for `input[n].content`.

The live reproduction isolated the incompatible item shape. LongCat emits both an assistant message and a plaintext Responses reasoning item:

```json
{
  "type": "reasoning",
  "content": [{"type": "reasoning_text", "text": "..."}]
}
```

The current compatibility attempt removes assistant messages globally. In session `019f6c05-8881-7413-9788-898cd9e014a0`, the gateway removed one assistant message but left the plaintext reasoning item. It then became `input[3]`, and ChatGPT still rejected the request. Removing assistant messages also discards useful context and changes providers that were already compatible.

A separate minimum request proved that ChatGPT OAuth accepts an inline assistant message with non-empty `output_text` and can recall its marker. Therefore assistant content is not the incompatible shape and must remain in its original role. The verified incompatibility is the foreign reasoning item with non-empty plaintext `content`.

## Goals

- Continue a LongCat-generated Codex session on a ChatGPT OAuth provider without a visible retry or provider switch.
- Preserve assistant messages, tool calls, tool results, user messages, and their ordering exactly.
- Never forward foreign plaintext reasoning to ChatGPT.
- Leave ikuncode and Xunfei `cc2cx` histories unchanged when they already use a compatible shape.
- Detect incompatibility from the request structure, not a hard-coded provider ID or name.
- Preserve the Codex session identity and current user request.

## Non-Goals

- Reconstruct or expose hidden chain-of-thought.
- Ask any provider to generate a handoff summary.
- Make every provider emit ChatGPT-native response IDs.
- Add a generic transcript compaction system in this iteration.
- Retry every arbitrary upstream 400 response.

## Selected Approach

Perform a deterministic preflight normalization only while preparing a Responses request for a ChatGPT OAuth backend.

The trigger is all of the following:

1. The target uses the ChatGPT/Codex backend.
2. The outgoing path is `/v1/responses` or `/responses`.
3. `input` contains at least one `type: "reasoning"` item whose `content` is a non-empty array.

This shape is the verified LongCat incompatibility signal. Encrypted ChatGPT-compatible reasoning with no plaintext `content` does not trigger normalization. An assistant message by itself does not trigger normalization, which preserves the verified ikuncode and Xunfei behavior.

## Transformation

The transformation operates on the per-provider request clone before it is sent upstream.

1. Scan the original `input` in order.
2. Remove only reasoning items whose `content` is a non-empty array. Their plaintext content is discarded and is never copied elsewhere.
3. Preserve encrypted reasoning items whose `content` is absent, null, or empty.
4. Preserve every non-reasoning item byte-for-byte at the JSON value level and in the same order. This includes developer messages, user messages, assistant messages, images, function calls, and function-call outputs.
5. Remove `previous_response_id` when present, since the detected foreign history proves that the response chain is not native to the target ChatGPT backend.
6. Preserve `model`, `prompt_cache_key`, the current session header, tools, stream settings, headers, and all other ChatGPT-compatible top-level fields.

The algorithm does not need to locate a current-user boundary and works for user turns, tool-only continuations, and inputs with no user item because it never moves or re-roles transcript items.

## Placement

The shape-aware transformation belongs in the ChatGPT request compatibility layer, before the final allow-list filtering and before the upstream request is sent. It must return structured outcome metadata so the provider preparation layer can log whether it was applied.

The current global assistant stripping must be removed from:

- standard API-key provider preparation;
- the generic request sanitizer safety net;
- unconditional ChatGPT compatibility filtering.

Standard providers must continue receiving the original Responses transcript unless their own bridge explicitly requires a translation.

## Observability

When applied, add one structured special setting and one log event:

```json
{
  "type": "foreign_history_handoff",
  "trigger": "plaintext_reasoning_content",
  "reasoning_items_removed": 1,
  "previous_response_id_removed": true
}
```

This special setting is internal request-log metadata and must never be forwarded upstream. `previous_response_id_removed` is true only when the field was present and actually deleted. Logs contain counts and provider ID, but never reasoning content or other request text.

## Error Handling

- Invalid or non-JSON bodies remain unchanged and produce a debug log.
- A non-array `input` remains unchanged.
- Transformation is idempotent: after non-empty plaintext reasoning items are removed, a second pass is a no-op.
- No extra upstream call is made, so the compatibility path cannot create a circuit-breaker cooldown before the transformed request is tried.
- Existing upstream error classification remains responsible for genuine provider failures.

## Testing

### Unit tests

1. A LongCat-shaped `reasoning + assistant + current user` input preserves the assistant-only marker and item ordering, removes only plaintext reasoning, and clears `previous_response_id`.
2. Plaintext reasoning is discarded and never copied into any surviving item.
3. An ikuncode-shaped encrypted reasoning item with `content: null` is a no-op.
4. A Xunfei-shaped assistant message without plaintext reasoning is a no-op.
5. Developer, user, assistant, image, function-call, and function-call-output items remain structurally equal and in their original order.
6. Running the transformer twice produces the same output as running it once.
7. Tool-only and no-user inputs normalize safely without moving items.
8. The per-provider clone changes without mutating the shared request body.
9. Headers and unrelated top-level fields remain unchanged.
10. Internal handoff metadata is logged but never added to the upstream JSON body.

Each behavior is developed red-green: the test must fail for the expected missing behavior before production code changes.

### Live regression

Use three new Codex sessions and assistant-only random markers:

1. LongCat to GPT OAuth: both gateway requests return 200 and GPT exactly recalls the LongCat marker.
2. ikuncode to GPT OAuth: direct continuation remains 200 and exactly recalls its marker; no handoff log is recorded.
3. Xunfei `cc2cx` to GPT OAuth: direct continuation remains 200 and exactly recalls its marker; no handoff log is recorded.

For the LongCat case, capture the normalized outbound shape in a local test fixture or assertion and verify that it contains no non-empty plaintext reasoning, preserves the assistant marker, and results in exactly one target upstream attempt. Verify `foreign_history_handoff` in request logs and verify that no `array_above_max_length`, cooldown, or circuit-open event occurs.

## Rollout And Recovery

- Build and run the new bundle from the build output without overwriting the installed Dev application.
- Temporarily enable disabled providers only for their forced-route tests.
- Restore provider enabled flags, route membership, and test-created circuit rows afterward.
- Relaunch the installed Dev application after testing.
- Do not commit or modify unrelated existing worktree changes.
