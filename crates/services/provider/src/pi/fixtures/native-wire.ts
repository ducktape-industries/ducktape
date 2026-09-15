// Minimal vendor-shaped SSE replies for the installed built-in Pi providers.
// No SDK mocks: tests capture their actual HTTP payloads across cold restarts.
export type Backend = "anthropic" | "openai-codex";
const encode = (events: Record<string, unknown>[]): string => events.map((event) =>
  `event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`).join("");

export interface WireTool { name: string; arguments: Record<string, unknown> }
export const vendorSse = (backend: Backend, request: number, call: WireTool = { name: "read", arguments: { path: "input.txt" } }): string => {
  const tool = request === 1;
  const argumentsJson = JSON.stringify(call.arguments);
  switch (backend) {
    case "anthropic": return encode([
      { type: "message_start", message: { id: `msg_fixture_${request}`, type: "message", role: "assistant", model: "fixture", content: [], stop_reason: null, stop_sequence: null, usage: { input_tokens: 10, output_tokens: 0 } } },
      { type: "content_block_start", index: 0, content_block: tool ? { type: "tool_use", id: "toolu_fixture", name: call.name, input: {} } : { type: "text", text: "" } },
      { type: "content_block_delta", index: 0, delta: tool ? { type: "input_json_delta", partial_json: argumentsJson } : { type: "text_delta", text: `Vendor answer ${request}.` } },
      { type: "content_block_stop", index: 0 },
      { type: "message_delta", delta: { stop_reason: tool ? "tool_use" : "end_turn", stop_sequence: null }, usage: { output_tokens: 5 } },
      { type: "message_stop" },
    ]);
    case "openai-codex": {
      const item = tool
        ? { type: "function_call", id: "fc_fixture", call_id: "call_fixture", name: call.name, arguments: argumentsJson, status: "completed" }
        : { type: "message", id: `msg_fixture_${request}`, role: "assistant", status: "completed", content: [{ type: "output_text", text: `Vendor answer ${request}.`, annotations: [] }] };
      return encode([
        { type: "response.created", response: { id: `resp_fixture_${request}`, status: "in_progress", output: [] } },
        { type: "response.output_item.added", output_index: 0, item: tool ? { ...item, arguments: "", status: "in_progress" } : { ...item, content: [], status: "in_progress" } },
        tool ? { type: "response.function_call_arguments.delta", output_index: 0, item_id: item.id, delta: argumentsJson }
          : { type: "response.output_text.delta", output_index: 0, item_id: item.id, content_index: 0, delta: `Vendor answer ${request}.` },
        { type: "response.output_item.done", output_index: 0, item },
        { type: "response.completed", response: { id: `resp_fixture_${request}`, status: "completed", output: [item], usage: { input_tokens: 10, output_tokens: 5, total_tokens: 15, input_tokens_details: { cached_tokens: 0 }, output_tokens_details: { reasoning_tokens: 0 } } } },
      ]);
    }
  }
};
