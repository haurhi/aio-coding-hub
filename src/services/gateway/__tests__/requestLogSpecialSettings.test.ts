import { describe, expect, it } from "vitest";
import {
  hasCodexSystemRequestSpecialSetting,
  resolveClaudeModelMappingFromSpecialSettings,
  resolveModelRedirectFromSpecialSettings,
} from "../requestLogSpecialSettings";

describe("services/gateway/requestLogSpecialSettings", () => {
  it("resolves Claude model mapping with final provider preference", () => {
    const settings = JSON.stringify([
      { type: "noop" },
      {
        type: "claude_model_mapping",
        requestedModel: " claude-sonnet ",
        effectiveModel: " gpt-4.1 ",
        mappingKind: " sonnet ",
        providerId: 1,
        providerName: " Provider A ",
        applied: true,
      },
      {
        type: "claude_model_mapping",
        requestedModel: " claude-sonnet ",
        effectiveModel: " gpt-5.4 ",
        mappingKind: " sonnet ",
        providerId: 2,
        providerName: " Provider B ",
        applied: true,
      },
    ]);

    expect(resolveClaudeModelMappingFromSpecialSettings(settings, 2)).toEqual({
      requestedModel: "claude-sonnet",
      effectiveModel: "gpt-5.4",
      mappingKind: "sonnet",
      providerId: 2,
      providerName: "Provider B",
      applied: true,
    });
    expect(resolveClaudeModelMappingFromSpecialSettings(settings, 99)?.providerId).toBe(2);
    expect(resolveModelRedirectFromSpecialSettings(settings, 2)).toEqual({
      stage: "legacy",
      providerId: 2,
      providerName: "Provider B",
      sourceModel: "claude-sonnet",
      targetModel: "gpt-5.4",
    });
  });

  it("resolves generic model redirect with final provider preference", () => {
    const settings = JSON.stringify([
      {
        type: "model_redirect",
        stage: "provider",
        providerId: 1,
        providerName: "Provider A",
        sourceModel: "gpt-original",
        targetModel: "model-a",
      },
      {
        type: "model_redirect",
        stage: "provider",
        providerId: 2,
        providerName: "Provider B",
        sourceModel: "gpt-original",
        targetModel: "model-b",
      },
    ]);

    expect(resolveModelRedirectFromSpecialSettings(settings, 2)).toEqual({
      stage: "provider",
      providerId: 2,
      providerName: "Provider B",
      sourceModel: "gpt-original",
      targetModel: "model-b",
    });
  });

  it("ignores invalid, unapplied, and identity mappings", () => {
    expect(resolveClaudeModelMappingFromSpecialSettings(null)).toBeNull();
    expect(resolveClaudeModelMappingFromSpecialSettings("bad-json")).toBeNull();
    expect(
      resolveClaudeModelMappingFromSpecialSettings(
        JSON.stringify([
          {
            type: "claude_model_mapping",
            requestedModel: "same",
            effectiveModel: "same",
            mappingKind: "sonnet",
            providerId: 1,
            providerName: "Provider A",
            applied: true,
          },
          {
            type: "claude_model_mapping",
            requestedModel: "claude-sonnet",
            effectiveModel: "gpt-5.4",
            mappingKind: "sonnet",
            providerId: 2,
            providerName: "Provider B",
            applied: false,
          },
        ])
      )
    ).toBeNull();
  });

  it("identifies only the structured Codex system request marker", () => {
    expect(
      hasCodexSystemRequestSpecialSetting(
        JSON.stringify([{ type: "noop" }, { type: "codex_system_request", threadSource: "system" }])
      )
    ).toBe(true);
    expect(
      hasCodexSystemRequestSpecialSetting(
        JSON.stringify({ type: "codex_system_request", threadSource: "system" })
      )
    ).toBe(true);
  });

  it("rejects incomplete or mismatched Codex system request markers", () => {
    for (const settings of [
      [{ type: "codex_system_request" }],
      [{ type: "codex_system_request", threadSource: "user" }],
      [{ type: "other", threadSource: "system" }],
      [{ type: "codex_system_request", threadSource: true }],
    ]) {
      expect(hasCodexSystemRequestSpecialSetting(JSON.stringify(settings))).toBe(false);
    }
  });

  it("fails closed for missing or malformed special settings", () => {
    expect(hasCodexSystemRequestSpecialSetting(null)).toBe(false);
    expect(hasCodexSystemRequestSpecialSetting("bad-json")).toBe(false);
    expect(hasCodexSystemRequestSpecialSetting(JSON.stringify([null, false, "marker"]))).toBe(
      false
    );
  });
});
