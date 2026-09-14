// Ducktape's Pi tool plane: discover the same tools and guide as the other
// runners from `ducktape mcp`, rather than maintaining another tool catalog.
// Stage this file outside Pi's auto-discovery directories and pass it with -e
// only for tools-enabled headless runs. Pi supplies the TypeScript loader;
// runtime imports are Node built-ins, including in the standalone Pi binary.
// The child inherits run-scoped capabilities, never model-provider credentials.

import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import type { ExtensionAPI, ToolDefinition } from "@earendil-works/pi-coding-agent";

// --- Process environment ---------------------------------------------------

const CHILD_ENV = [
  "PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "SSL_CERT_FILE", "SSL_CERT_DIR",
  "DUCKTAPE_NODE", "DUCKTAPE_RUN_AGENT", "DUCKTAPE_RUN_WORKSPACE",
  "DUCKTAPE_RUN_SKILLS", "DUCKTAPE_RUN_ACTION_URL", "DUCKTAPE_RUN_ACTION_TOKEN",
  "DUCKTAPE_RUN_ID", "DUCKTAPE_PROVIDER_CONTROL_URL", "DUCKTAPE_PROVIDER_CONTROL_TOKEN",
] as const;

const childEnvironment = (env: NodeJS.ProcessEnv): NodeJS.ProcessEnv =>
  Object.fromEntries(CHILD_ENV.flatMap((name) => {
    const value = env[name];
    return value === undefined ? [] : [[name, value]];
  }));

// --- Wire validation -------------------------------------------------------

interface McpTool {
  name: string;
  title: string;
  description: string;
  inputSchema: ToolDefinition["parameters"];
}

interface Pending {
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
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

// --- Session-scoped stdio transport ----------------------------------------

const connect = (cwd: string, env: NodeJS.ProcessEnv) => {
  const child = spawn("ducktape", ["mcp"], {
    cwd, env, stdio: ["pipe", "pipe", "ignore"], shell: false,
  });
  const lines = createInterface({ input: child.stdout, crlfDelay: Infinity });
  const pending = new Map<string, Pending>();
  // Mutable resource state is confined to callbacks that own the child lifetime.
  const state: { failure?: Error; closing?: Promise<void> } = {};
  const exited = new Promise<void>((resolve) => child.once("close", () => resolve()));
  const fail = (error: Error) => {
    if (state.failure) return;
    state.failure = error;
    pending.forEach((request) => request.reject(error));
    pending.clear();
  };
  const close = (): Promise<void> => {
    if (state.closing) return state.closing;
    fail(new Error("Ducktape MCP session closed"));
    // EOF is the server's normal shutdown. Bound a blocked HTTP call without
    // detaching the process; the sandbox still owns Pi and its child together.
    child.stdin.end();
    const kill = setTimeout(() => child.kill("SIGKILL"), 1000);
    state.closing = exited.finally(() => {
      clearTimeout(kill);
      lines.close();
      process.off("exit", onExit);
    });
    return state.closing;
  };
  const onExit = () => child.kill("SIGKILL");
  process.once("exit", onExit);
  child.once("error", () => fail(new Error("Could not start ducktape mcp; check the run PATH")));
  child.once("close", (code, signal) => {
    fail(new Error(`Ducktape MCP exited (code ${code}, signal ${signal})`));
    process.off("exit", onExit);
  });
  child.stdin.on("error", () => fail(new Error("Ducktape MCP stdin failed")));
  child.stdout.on("error", () => fail(new Error("Ducktape MCP stdout failed")));
  lines.once("close", () => fail(new Error("Ducktape MCP stdout closed")));

  const receive = (line: string) => {
    if (line.trim() === "") return;
    const frame = object(JSON.parse(line));
    if (frame.jsonrpc !== "2.0") throw new Error("Ducktape MCP returned an invalid JSON-RPC frame");
    if (typeof frame.id !== "string") throw new Error("Ducktape MCP returned an invalid response id");
    const request = pending.get(frame.id);
    // A cancelled request can still finish on the synchronous Rust server.
    if (!request) return;
    if (frame.error !== undefined) {
      const error = object(frame.error);
      pending.delete(frame.id);
      request.reject(new Error(`Ducktape MCP protocol error ${error.code}: ${error.message}`));
      return;
    }
    pending.delete(frame.id);
    if (!("result" in frame)) {
      request.reject(new Error("Ducktape MCP response has no result"));
      return;
    }
    request.resolve(frame.result);
  };
  lines.on("line", (line) => {
    Promise.resolve().then(() => receive(line)).catch(() => {
      // Never include a malformed raw frame or child stderr in diagnostics:
      // those can contain run capabilities or arbitrarily large tool payloads.
      fail(new Error("Ducktape MCP returned malformed protocol output"));
      return close();
    });
  });

  const send = (frame: Record<string, unknown>) => {
    if (state.failure) throw state.failure;
    child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", ...frame })}\n`);
  };
  const request = (method: string, params: unknown, signal?: AbortSignal): Promise<unknown> =>
    Promise.resolve().then(() => {
      if (state.failure) throw state.failure;
      signal?.throwIfAborted();
      const id = randomUUID();
      const abort = () => {
        pending.get(id)?.reject(new Error("Ducktape MCP call cancelled; a submitted action may still complete"));
        pending.delete(id);
      };
      return new Promise<unknown>((resolve, reject) => {
        pending.set(id, { resolve, reject });
        signal?.addEventListener("abort", abort, { once: true });
        send({ id, method, params });
      }).finally(() => {
        signal?.removeEventListener("abort", abort);
        pending.delete(id);
      });
    });
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

// --- Pi lifecycle ----------------------------------------------------------

const ducktapeExtension = (pi: ExtensionAPI): void => {
  // Snapshot once per extension instance, after the provider has provisioned
  // this run. No host credential/config discovery and no re-reading hot env.
  const env = childEnvironment(process.env);
  const tokens = [env.DUCKTAPE_RUN_ACTION_TOKEN, env.DUCKTAPE_PROVIDER_CONTROL_TOKEN]
    .filter((token): token is string => typeof token === "string" && token.length > 0);
  const redact = (text: string) => tokens.reduce((value, token) => value.replaceAll(token, "[redacted]"), text);
  const state: { client?: ReturnType<typeof connect>; guidance?: string } = {};

  pi.on("session_start", (_event, ctx) => {
    const headless = ctx.mode === "print" || ctx.mode === "json";
    if (!headless) return;
    const client = connect(ctx.cwd, env);
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
        state.guidance = redact(initialized.instructions as string);
        client.send({ method: "notifications/initialized" });
        return client.request("tools/list", {}, startup);
      })
      .then(toolList)
      .then((tools) => {
        tools.forEach((tool) => pi.registerTool({
          name: tool.name, label: tool.title,
          description: `${tool.description}\nOutput is capped at 2000 lines / 48 KiB; larger results are saved to a local file.`,
          parameters: tool.inputSchema,
          execute: (_id, args, signal) => Promise.resolve()
            .then(() => client.request("tools/call", { name: tool.name, arguments: args }, signal))
            .then(toolResult)
            .then(({ text, isError }) => boundedText(redact(text)).then((text) => {
              // Returning isError from execute does NOT mark a Pi tool failed.
              if (isError) throw new Error(text);
              return { content: [{ type: "text" as const, text }], details: {} };
            }))
            .catch((error: unknown) => {
              throw new Error(redact(error instanceof Error ? error.message : String(error)));
            }),
        }));
        pi.setActiveTools([...new Set([...pi.getActiveTools(), ...tools.map((tool) => tool.name)])]);
      })
      .catch((error: unknown) => {
        const reason = redact(error instanceof Error ? error.message : String(error));
        state.guidance = `Ducktape tools are unavailable: ${reason}. Do not claim to have read or changed the network.`;
        return client.close();
      });
  });
  pi.on("before_agent_start", (event) => {
    if (!state.guidance) return;
    return { systemPrompt: `${event.systemPrompt}\n\n${state.guidance}` };
  });
  pi.on("session_shutdown", () => state.client?.close());
};

export default ducktapeExtension;
