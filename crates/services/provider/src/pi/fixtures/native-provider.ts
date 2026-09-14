// Offline provider exercised through the real Pi CLI and SDK, not a mocked
// extension API. The loopback peer records effects against committed snapshots.
import { createAssistantMessageEventStream } from "@earendil-works/pi-ai";
import type { AssistantMessage, Context } from "@earendil-works/pi-ai";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

const fixture = (pi: ExtensionAPI): void => {
  const endpoint = process.env.DUCKTAPE_RUN_ACTION_URL!;
  const mode = process.env.DUCKTAPE_NATIVE_FIXTURE_MODE ?? "normal";
  const effect = (kind: string, data: unknown) => fetch(`${endpoint}/fixture`, {
    method: "POST", body: JSON.stringify({ kind, data }),
  }).then((response) => {
    if (!response.ok) throw new Error("fixture_effect_refused");
    return response.json();
  });
  const text = (context: Context): string => JSON.stringify(context.messages);
  if (mode === "observe-lifecycle") {
    const events: string[] = [];
    // Preparation permits exactly one subscriber. Observe bind/lifecycle only;
    // the real package's receipt and prepared prompt prove its handler ran.
    pi.events.on("ducktape:network:bind", () => { events.push("network-bind"); });
    pi.on("session_start", () => { events.push("session-start"); });
    pi.on("before_agent_start", () => { events.push("before-agent"); });
    pi.on("session_shutdown", () => effect("lifecycle", { events, tools: pi.getActiveTools() }).then(() => undefined));
  }
  if (mode.startsWith("prepare")) pi.events.on("ducktape:resident:prepare", (value) => {
    const request = value as { events: unknown[]; accept: (result: Promise<unknown>) => void };
    if (mode === "prepare-double") {
      request.accept(Promise.resolve({ wake: false }));
      request.accept(Promise.resolve({ wake: false }));
      return;
    }
    if (mode === "prepare-invalid") { request.accept(Promise.resolve({ wake: "no" })); return; }
    request.accept(Promise.resolve().then(() => effect("prepare", request.events)).then(() => ({ wake: false })));
  });
  pi.registerProvider("offline", {
    api: "openai-completions", baseUrl: "http://127.0.0.1:1/never-called", apiKey: "offline-not-a-secret",
    models: [{ id: "fixture", name: "Offline", reasoning: false, input: ["text"],
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 32768, maxTokens: 1024 }],
    streamSimple: (model, context) => {
      const stream = createAssistantMessageEventStream();
      Promise.resolve()
        .then(() => effect("model", { messages: context.messages, systemPrompt: context.systemPrompt, contextWindow: model.contextWindow, tools: context.tools }))
        .then((script) => {
          const hasResults = context.messages.some((message) => message.role === "toolResult");
          const compacted = text(context).includes("OFFLINE COMPACTION");
          const shouldCall = !hasResults && !compacted && mode !== "model-error";
          const content: AssistantMessage["content"] = mode === "bash-secret"
            ? [{ type: "toolCall", id: "bash-secret-call", name: "bash", arguments: { command: 'printf %s "$DUCKTAPE_RUN_ACTION_TOKEN"' } }]
            : shouldCall ? script.tool_calls ?? [1, 2].map((number) => ({ type: "toolCall", id: `native-call-${number}`, name: "native_fixture", arguments: { number } }))
            : [{ type: "text", text: `Offline answer: ${context.messages.filter((message) => message.role === "user").length} user messages; ${compacted ? "compacted" : "retained"}.` }];
          const message: AssistantMessage = {
            role: "assistant", api: model.api, provider: model.provider, model: model.id,
            content, stopReason: mode === "model-error" ? "error" : shouldCall ? "toolUse" : "stop", timestamp: Date.now(),
            usage: { input: mode === "compact" ? 30000 : 100, output: 10, cacheRead: 0, cacheWrite: 0, totalTokens: mode === "compact" ? 30010 : 110,
              cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
          };
          stream.push({ type: "start", partial: { ...message, content: [] } });
          if (message.stopReason === "error") stream.push({ type: "error", reason: "error", error: message });
          else stream.push({ type: "done", reason: message.stopReason as "stop" | "toolUse", message });
          stream.end();
        }).catch((error: unknown) => {
          const message: AssistantMessage = { role: "assistant", api: model.api, provider: model.provider, model: model.id,
            content: [], stopReason: "error", errorMessage: String(error), timestamp: Date.now(),
            usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } } };
          stream.push({ type: "error", reason: "error", error: message });
          stream.end();
        });
      return stream;
    },
  });
  const hasLargeResult = mode === "compact" || mode === "large-result";
  const resultSuffix = hasLargeResult ? "x".repeat(110000) : "";
  const exposesFixtureCredential = mode === "secret";
  pi.registerTool({
    name: "native_fixture", label: "Native fixture", description: "Offline durable-boundary fixture",
    parameters: Type.Object({ number: Type.Number() }),
    execute: (_id, args, _signal, onUpdate) => Promise.resolve()
      .then(() => effect("tool", args))
      .then(() => {
        if (mode === "stream-updates") {
          // SDK onUpdate is void/unawaited: these large, distinct frames overlap
          // in native event subscribers even though tool execution is serial.
          [0, 1, 2].forEach((index) => onUpdate?.({
            content: [{ type: "text", text: String.fromCharCode(65 + args.number * 3 + index).repeat(192 * 1024) }],
            details: { number: args.number, index },
          }));
        }
        return {
          content: [{ type: "text" as const, text: exposesFixtureCredential ? process.env.DUCKTAPE_RUN_ACTION_TOKEN! : `result-${args.number}${resultSuffix}` }],
          details: { exact: { nested: [args.number, "opaque-payload"] } },
        };
      }),
  });
  pi.on("session_before_compact", (event) => ({ compaction: {
    summary: "OFFLINE COMPACTION", firstKeptEntryId: event.preparation.firstKeptEntryId,
    tokensBefore: event.preparation.tokensBefore, details: { fixture: true },
  } }));
};
export default fixture;
