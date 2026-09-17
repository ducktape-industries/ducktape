// Ducktape's Pi tool plane: discover the same tools and guide as the other
// runners from the run's own MCP endpoint, rather than maintaining another tool
// catalog. Stage this file outside Pi's auto-discovery directories and pass it
// with -e only for tools-enabled headless runs. Pi supplies the TypeScript
// loader; runtime imports are Node built-ins, including in the standalone Pi
// binary.
//
// The endpoint is the node lane this run already dials (DUCKTAPE_NODE) plus
// MCP_PATH, and it is the ONLY thing this extension needs: the tools run on the
// host, under the identity that lane belongs to, so nothing here holds a
// credential, spawns a process, or knows which agent it is acting for.

import { randomUUID } from "node:crypto";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { ExtensionAPI, ToolDefinition } from "@earendil-works/pi-coding-agent";

// --- The run's tool plane ---------------------------------------------------

// Must match `provider_host::MCP_PATH` — the one route the run's node lane
// serves itself instead of forwarding.
const MCP_PATH = "/mcp";

const endpointFrom = (env: NodeJS.ProcessEnv): string | undefined => {
  const node = env.DUCKTAPE_NODE;
  if (typeof node !== "string" || node === "") return undefined;
  return `${node.replace(/\/+$/, "")}${MCP_PATH}`;
};

// --- Wire validation -------------------------------------------------------

interface McpTool {
  name: string;
  title: string;
  description: string;
  inputSchema: ToolDefinition["parameters"];
}

const object = (value: unknown): Record<string, unknown> => {
  const isObject = value !== null && typeof value === "object" && !Array.isArray(value);
  if (!isObject) throw new Error("Ducktape MCP returned a non-object");
  return value as Record<string, unknown>;
};

const toolList = (value: unknown): McpTool[] => {
  const { tools, nextCursor } = object(value);
  // Ducktape's registry is one complete list, not a paginated MCP catalog.
  const completeList = Array.isArray(tools) && tools.length > 0 && nextCursor === undefined;
  if (!completeList) throw new Error("Ducktape MCP returned an incomplete tool list");
  const names = new Set<string>();
  return tools.map((value) => {
    const tool = object(value);
    const validName = typeof tool.name === "string" && /^ducktape_[a-z0-9_]+$/.test(tool.name);
    if (!validName) throw new Error("Ducktape MCP returned an invalid tool name");
    const name = tool.name as string;
    if (names.has(name)) throw new Error("Ducktape MCP returned a duplicate tool name");
    names.add(name);
    const schema = object(tool.inputSchema);
    const validDeclaration = typeof tool.description === "string" && schema.type === "object";
    if (!validDeclaration) throw new Error("Ducktape MCP returned an invalid tool declaration");
    return {
      name,
      title: typeof tool.title === "string" ? tool.title : name,
      description: tool.description as string,
      // Pi validates JSON Schema directly. Do not flatten nested operation args.
      inputSchema: schema as ToolDefinition["parameters"],
    };
  });
};

// --- Streamable-HTTP transport ---------------------------------------------

// One message per POST, one JSON response back — or 202 and no body for a
// notification, which is the transport's rule and why `send` never reads one.
// No SSE stream: the server never initiates a message.
const connect = (endpoint: string) => {
  // Mutable state is confined to this closure and only ever moves to "closed".
  const state: { failure?: Error } = {};
  const post = (frame: Record<string, unknown>, signal?: AbortSignal): Promise<Response> =>
    fetch(endpoint, {
      method: "POST",
      headers: { "content-type": "application/json", accept: "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", ...frame }),
      signal,
    });
  const send = (frame: Record<string, unknown>): void => {
    if (state.failure) throw state.failure;
    // A notification has no answer to wait for; a failed POST cannot be
    // reported to anyone, so it must not become an unhandled rejection.
    void post(frame).catch(() => {});
  };
  const request = (method: string, params: unknown, signal?: AbortSignal): Promise<unknown> =>
    Promise.resolve().then(() => {
      if (state.failure) throw state.failure;
      signal?.throwIfAborted();
      const id = randomUUID();
      return post({ id, method, params }, signal)
        .catch(() => {
          throw new Error("Ducktape MCP call cancelled or unreachable; a submitted action may still complete");
        })
        .then((response) => {
          if (!response.ok) throw new Error(`Ducktape MCP answered http ${response.status}`);
          return response.json();
        })
        .then((value: unknown) => {
          const frame = object(value);
          if (frame.jsonrpc !== "2.0") throw new Error("Ducktape MCP returned an invalid JSON-RPC frame");
          if (frame.id !== id) throw new Error("Ducktape MCP returned an invalid response id");
          if (frame.error !== undefined) {
            const error = object(frame.error);
            throw new Error(`Ducktape MCP protocol error ${error.code}: ${error.message}`);
          }
          if (!("result" in frame)) throw new Error("Ducktape MCP response has no result");
          return frame.result;
        });
    });
  const close = (): Promise<void> => {
    state.failure ??= new Error("Ducktape MCP session closed");
    return Promise.resolve();
  };
  return { request, send, close };
};

// --- Model-visible results -------------------------------------------------

const toolResult = (value: unknown) => {
  const result = object(value);
  const validResult = Array.isArray(result.content) && typeof result.isError === "boolean";
  if (!validResult) throw new Error("Ducktape MCP returned an invalid tool result");
  const text = (result.content as unknown[]).map((value) => {
    const block = object(value);
    // The product MCP server emits text only. Reject new content shapes rather
    // than silently dropping them or claiming an unsupported result succeeded.
    const textBlock = block.type === "text" && typeof block.text === "string";
    if (!textBlock) throw new Error("Ducktape MCP returned unsupported tool content");
    return block.text as string;
  }).join("\n");
  return { text, isError: result.isError as boolean };
};

const boundedText = (text: string): Promise<string> => {
  const head = text.split("\n").slice(0, 2000).join("\n");
  const bytes = Buffer.from(head);
  // Streaming decode omits an incomplete trailing UTF-8 character at the cap.
  const prefix = new TextDecoder().decode(bytes.subarray(0, 48 * 1024), { stream: true });
  if (prefix === text) return Promise.resolve(text);
  return Promise.resolve()
    .then(() => mkdtemp(join(tmpdir(), "ducktape-pi-")))
    .then((dir) => {
      const path = join(dir, "result.txt");
      return writeFile(path, text, { mode: 0o600 }).then(() =>
        `${prefix}\n\n[Output truncated to 2000 lines / 48 KiB. Full output: ${path}]`);
    });
};

// --- Package service -------------------------------------------------------

export interface MCPToolsCallResult {
  content: Array<{ type: string; text: string }>;
  isError: boolean;
  [key: string]: unknown;
}

export interface DucktapeNetworkService {
  callTool: (name: string, args: Record<string, unknown>, signal?: AbortSignal) => Promise<MCPToolsCallResult>;
}

const awaitStartup = (ready: Promise<void>, signal?: AbortSignal): Promise<void> => {
  if (!signal) return ready;
  const cancelled = Promise.withResolvers<void>();
  const abort = () => cancelled.reject(new Error("Ducktape MCP call cancelled before startup"));
  if (signal.aborted) abort();
  signal.addEventListener("abort", abort, { once: true });
  return Promise.race([ready, cancelled.promise]).finally(() => signal.removeEventListener("abort", abort));
};

// --- Pi lifecycle ----------------------------------------------------------

const ducktapeExtension = (pi: ExtensionAPI): void => {
  // Snapshot once per extension instance, after the provider has provisioned
  // this run. No host credential/config discovery and no re-reading hot env.
  // The run's write authority lives on the HOST, behind this endpoint. Nothing
  // this process holds is a secret any more, which is why no result or
  // diagnostic is redacted on the way out — there is nothing to redact.
  const endpoint = endpointFrom(process.env);
  const state: { client?: ReturnType<typeof connect>; guidance?: string; unavailable?: Error } = {};
  const names = new Set<string>();
  const ready = Promise.withResolvers<void>();
  // A failed startup may have no package consumer. Attach rejection handling
  // immediately, while retaining the rejecting promise for every actual caller.
  ready.promise.catch(() => {});
  const service: DucktapeNetworkService = {
    callTool: (name, args, signal) => Promise.resolve()
      .then(() => awaitStartup(ready.promise, signal))
      .then(() => {
        signal?.throwIfAborted();
        if (state.unavailable) throw state.unavailable;
        if (!names.has(name)) throw new Error("Ducktape MCP tool was not discovered");
        if (!state.client) throw new Error("Ducktape MCP session is unavailable");
        return state.client.request("tools/call", { name, arguments: args }, signal);
      })
      .then((value) => {
        // Validate the server's exact text-only envelope without flattening or
        // clipping it. Trusted packages need complete structured operation JSON.
        toolResult(value);
        return value as MCPToolsCallResult;
      }),
  };
  const unbind = pi.events.on("ducktape:network:bind", (value) => {
    const request = object(value);
    if (typeof request.accept !== "function") throw new Error("Ducktape network binding requires accept");
    request.accept(service);
  });

  pi.on("session_start", (_event, ctx) => {
    const headless = ctx.mode === "print" || ctx.mode === "json";
    if (!headless) {
      state.unavailable = new Error("Ducktape MCP is only available in headless sessions");
      ready.reject(state.unavailable);
      return;
    }
    if (!endpoint) {
      state.unavailable = new Error("This run has no Ducktape node lane, so it has no tool plane");
      ready.reject(state.unavailable);
      return;
    }
    const client = connect(endpoint);
    state.client = client;
    const startup = AbortSignal.timeout(10_000);
    return Promise.resolve()
      .then(() => client.request("initialize", {
        protocolVersion: "2025-06-18", capabilities: {},
        clientInfo: { name: "ducktape-pi", version: "1" },
      }, startup))
      .then((value) => {
        const initialized = object(value);
        const validServer = initialized.protocolVersion === "2025-06-18"
          && object(initialized.serverInfo).name === "ducktape"
          && typeof initialized.instructions === "string";
        if (!validServer) throw new Error("Ducktape MCP initialization did not match the server contract");
        state.guidance = initialized.instructions as string;
        client.send({ method: "notifications/initialized" });
        return client.request("tools/list", {}, startup);
      })
      .then(toolList)
      .then((tools) => {
        tools.forEach((tool) => names.add(tool.name));
        tools.forEach((tool) => pi.registerTool({
          name: tool.name, label: tool.title,
          description: `${tool.description}\nOutput is capped at 2000 lines / 48 KiB; larger results are saved to a local file.`,
          parameters: tool.inputSchema,
          execute: (_id, args, signal) => Promise.resolve()
            .then(() => client.request("tools/call", { name: tool.name, arguments: args }, signal))
            .then(toolResult)
            .then(({ text, isError }) => boundedText(text).then((text) => {
              // Returning isError from execute does NOT mark a Pi tool failed.
              if (isError) throw new Error(text);
              return { content: [{ type: "text" as const, text }], details: {} };
            })),
        }));
        pi.setActiveTools([...new Set([...pi.getActiveTools(), ...tools.map((tool) => tool.name)])]);
        ready.resolve();
      })
      .catch((error: unknown) => {
        const reason = error instanceof Error ? error.message : String(error);
        state.guidance = `Ducktape tools are unavailable: ${reason}. Do not claim to have read or changed the network.`;
        state.unavailable = new Error(reason);
        ready.reject(state.unavailable);
        return client.close();
      });
  });
  pi.on("before_agent_start", (event) => {
    if (!state.guidance) return;
    return { systemPrompt: `${event.systemPrompt}\n\n${state.guidance}` };
  });
  pi.on("session_shutdown", () => {
    state.unavailable = new Error("Ducktape MCP session closed");
    ready.reject(state.unavailable);
    unbind();
    return state.client?.close();
  });
};

export default ducktapeExtension;
