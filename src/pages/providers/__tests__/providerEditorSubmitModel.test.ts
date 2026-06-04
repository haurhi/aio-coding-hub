import { describe, expect, it } from "vitest";
import { DEFAULT_FORM_VALUES } from "../providerEditorUtils";
import { buildProviderEditorUpsertInput } from "../providerEditorSubmitModel";
import type { ProviderEditorPayloadContext } from "../providerEditorActionContext";

function makeContext(
  overrides: Partial<ProviderEditorPayloadContext> = {}
): ProviderEditorPayloadContext {
  return {
    mode: "create",
    cliKey: "claude",
    editingProviderId: null,
    authMode: "api_key",
    baseUrlMode: "order",
    baseUrlRows: [{ id: "1", url: "https://example.com/v1", ping: { status: "idle" } }],
    tags: [],
    claudeModels: {},
    modelMappingRows: [],
    streamIdleTimeoutSeconds: "",
    apiKeyConfigured: false,
    isCodexGatewaySource: false,
    sourceProviderId: null,
    selectedCx2ccSourceProvider: null,
    formValues: {
      ...DEFAULT_FORM_VALUES,
      name: "Provider A",
      api_key: "sk-test",
    },
    ...overrides,
  };
}

describe("pages/providers/providerEditorSubmitModel", () => {
  it("requires an api key when editing an api-key provider without a saved secret", () => {
    const result = buildProviderEditorUpsertInput(
      makeContext({
        mode: "edit",
        editingProviderId: 8,
        apiKeyConfigured: false,
        formValues: {
          ...DEFAULT_FORM_VALUES,
          name: "Provider A",
          api_key: "",
        },
      })
    );

    expect(result).toEqual({
      ok: false,
      error: {
        kind: "message",
        message: "请输入 API Key",
      },
    });
  });

  it("clears base urls and api key for oauth providers", () => {
    const result = buildProviderEditorUpsertInput(
      makeContext({
        authMode: "oauth",
        formValues: {
          ...DEFAULT_FORM_VALUES,
          name: "OAuth Provider",
          api_key: "",
          auth_mode: "oauth",
        },
      })
    );

    expect(result.ok).toBe(true);
    if (!result.ok) return;

    expect(result.value.payload.baseUrls).toEqual([]);
    expect(result.value.payload.apiKey).toBeNull();
    expect(result.value.payload.authMode).toBe("oauth");
  });

  it("forces cx2cc gateway sources to use zero cost and no source provider id", () => {
    const result = buildProviderEditorUpsertInput(
      makeContext({
        authMode: "cx2cc",
        isCodexGatewaySource: true,
        formValues: {
          ...DEFAULT_FORM_VALUES,
          name: "CX2CC Provider",
          api_key: "",
        },
      })
    );

    expect(result.ok).toBe(true);
    if (!result.ok) return;

    expect(result.value.payload.costMultiplier).toBe(0);
    expect(result.value.payload.bridgeType).toBe("cx2cc");
    expect(result.value.payload.sourceProviderId).toBeNull();
    expect(result.value.payload.authMode).toBe("api_key");
  });

  it("marks codex api-key providers as r2c bridge when selected", () => {
    const result = buildProviderEditorUpsertInput(
      makeContext({
        cliKey: "codex",
        authMode: "r2c",
        baseUrlRows: [
          {
            id: "1",
            url: "https://ark.cn-beijing.volces.com/api/coding/v3",
            ping: { status: "idle" },
          },
        ],
        formValues: {
          ...DEFAULT_FORM_VALUES,
          name: "Volcengine Coding Plan",
          api_key: "sk-volc",
        },
      })
    );

    expect(result.ok).toBe(true);
    if (!result.ok) return;

    expect(result.value.payload).toEqual(
      expect.objectContaining({
        cliKey: "codex",
        authMode: "api_key",
        apiKey: "sk-volc",
        baseUrls: ["https://ark.cn-beijing.volces.com/api/coding/v3"],
        sourceProviderId: null,
        bridgeType: "r2c",
      })
    );
  });

  it("marks claude api-key providers as chat-completions bridge when selected", () => {
    const result = buildProviderEditorUpsertInput(
      makeContext({
        cliKey: "claude",
        authMode: "claude_chat_completions",
        baseUrlRows: [
          {
            id: "1",
            url: "https://opencode.ai/zen/go/v1",
            ping: { status: "idle" },
          },
        ],
        claudeModels: {
          sonnet_model: "mimo-v2.5-pro",
          haiku_model: "mimo-v2.5",
        },
        formValues: {
          ...DEFAULT_FORM_VALUES,
          name: "OpenCode Go MiMo",
          api_key: "sk-opencode",
        },
      })
    );

    expect(result.ok).toBe(true);
    if (!result.ok) return;

    expect(result.value.payload).toEqual(
      expect.objectContaining({
        cliKey: "claude",
        authMode: "api_key",
        apiKey: "sk-opencode",
        baseUrls: ["https://opencode.ai/zen/go/v1"],
        sourceProviderId: null,
        bridgeType: "claude_chat_completions",
        claudeModels: {
          sonnet_model: "mimo-v2.5-pro",
          haiku_model: "mimo-v2.5",
        },
      })
    );
  });

  it("includes normalized codex model mapping for r2c providers", () => {
    const result = buildProviderEditorUpsertInput(
      makeContext({
        cliKey: "codex",
        authMode: "r2c",
        modelMappingRows: [
          { id: "1", source: " gpt-5.5 ", target: " DeepSeek-V4-Pro " },
          { id: "2", source: "gpt-5", target: "" },
        ],
        baseUrlRows: [
          {
            id: "1",
            url: "https://ark.cn-beijing.volces.com/api/coding/v3",
            ping: { status: "idle" },
          },
        ],
        formValues: {
          ...DEFAULT_FORM_VALUES,
          name: "Volcengine Coding Plan",
          api_key: "sk-volc",
        },
      })
    );

    expect(result.ok).toBe(true);
    if (!result.ok) return;

    expect(result.value.payload.modelMapping).toEqual({
      "gpt-5.5": "DeepSeek-V4-Pro",
    });
  });
});
