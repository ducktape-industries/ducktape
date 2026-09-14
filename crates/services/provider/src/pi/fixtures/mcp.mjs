// A stdio peer for Pi bridge tests. A held response plus an explicit barrier
// makes cancellation and out-of-order response tests independent of timing.

import { createInterface } from "node:readline";

// --- Fixture protocol -----------------------------------------------------

const mode = process.argv[2];
const frames = [];
const held = [];
const tools = [
  {
    name: "ducktape_fixture",
    title: "ducktape::fixture",
    description: "Fixture operation",
    inputSchema: {
      type: "object",
      required: ["operation"],
      properties: {
        operation: { type: "string" },
        nested: { type: "object", properties: { values: { type: "array", items: { type: "integer" } } } },
      },
    },
  },
];
const send = (id, result) => process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id, result })}\n`);
const text = (id, value, isError = false) => send(id, {
  content: [{ type: "text", text: typeof value === "string" ? value : JSON.stringify(value) }], isError,
});
const call = (frame) => {
  switch (frame.params.arguments.operation) {
    case "env": return text(frame.id, { env: process.env, frames, pid: process.pid });
    case "error": return text(frame.id, "operation refused fixture-action-token", true);
    case "protocol_error": return process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id: frame.id, error: { code: -32601, message: "fixture refusal" } })}\n`);
    case "invalid_error": return process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id: frame.id, error: null })}\n`);
    case "malformed": return process.stdout.write("not json\n");
    case "unsupported": return send(frame.id, { content: [{ type: "resource", resource: {} }], isError: false });
    case "crash": return process.exit(7);
    case "block":
      text(frame.id, "blocked");
      // Model a synchronous MCP handler stuck in I/O: only process teardown
      // can release it. The test waits for the reply and child close, not time.
      return Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0);
    case "hold": return held.push(frame.id);
    case "release":
      held.splice(0).forEach((id) => text(id, "released"));
      return text(frame.id, "release acknowledged");
    case "large": return text(frame.id, "fixture-action-token\n" + "αβγ😀".repeat(30_000));
    case "lines": return text(frame.id, "line\n".repeat(2100));
    default: return text(frame.id, frame.params.arguments);
  }
};
const receive = (frame) => {
  frames.push(frame);
  switch (frame.method) {
    case "initialize": return send(frame.id, {
      protocolVersion: "2025-06-18", serverInfo: { name: "ducktape", version: "1" },
      capabilities: { tools: {} }, instructions: "Use ducktape_fixture to work on the network.",
    });
    case "notifications/initialized": return;
    case "tools/list": return send(frame.id, { tools: mode === "bad-list" ? [] : tools });
    case "tools/call": return call(frame);
    default: throw new Error("unexpected fixture method");
  }
};
const input = createInterface({ input: process.stdin });
input.on("line", (line) => receive(JSON.parse(line)));
input.on("close", () => process.exit(0));
