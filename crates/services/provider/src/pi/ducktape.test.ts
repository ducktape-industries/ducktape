// Dependency-free bridge regression tests: node --test ducktape.test.ts.
// Fake peers exercise transport faults; DUCKTAPE_TEST_BINARY optionally checks
// the actual compiled MCP catalog without contacting a node or loading secrets.
// Test bodies are imperative to interleave protocol events and assertions.

import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import type { TestContext } from "node:test";
import { fileURLToPath } from "node:url";
import type { ExtensionAPI, ToolDefinition } from "@earendil-works/pi-coding-agent";

import ducktapeExtension from "./ducktape.ts";
import type { DucktapeNetworkService } from "./ducktape.ts";

// --- Test setup -----------------------------------------------------------

const fixture = fileURLToPath(new URL("fixtures/mcp.mjs", import.meta.url));
const realBinary = process.env.DUCKTAPE_TEST_BINARY;
const sampleEnv = {
  DUCKTAPE_NODE: "http://127.0.0.1:1",
  DUCKTAPE_RUN_AGENT: "fixture-agent",
  DUCKTAPE_RUN_WORKSPACE: "fixture-workspace",
  DUCKTAPE_RUN_SKILLS: "fixture-skills",
  DUCKTAPE_RUN_ACTION_URL: "http://127.0.0.1:1/action",
  DUCKTAPE_RUN_ACTION_TOKEN: "fixture-action-token",
  DUCKTAPE_RUN_ID: "fixture-run",
  DUCKTAPE_PROVIDER_CONTROL_URL: "http://127.0.0.1:1/control",
  DUCKTAPE_PROVIDER_CONTROL_TOKEN: "fixture-control-token",
};

type Hook = (event: Record<string, unknown>, context: Record<string, unknown>) => unknown;

const harness = (t: TestContext, mode = "normal", binary?: string) => Promise.resolve()
  .then(() => mkdtemp(join(tmpdir(), "ducktape-pi-test-")))
  .then((cwd) => {
    t.after(() => rm(cwd, { recursive: true, force: true }));
    const command = binary === undefined
      ? `exec ${JSON.stringify(process.execPath)} ${JSON.stringify(fixture)} ${JSON.stringify(mode)}`
      : `exec ${JSON.stringify(binary)} "$@"`;
    return writeFile(join(cwd, "ducktape"), `#!/bin/sh\n${command}\n`, { mode: 0o700 }).then(() => cwd);
  })
  .then((cwd) => {
    const hooks = new Map<string, Hook>();
    const tools = new Map<string, ToolDefinition>();
    const active = new Set(["read", "bash"]);
    const events = new Map<string, (value: unknown) => void>();
    const pi = {
      events: { on: (name: string, handler: (value: unknown) => void) => {
        events.set(name, handler);
        return () => { events.delete(name); };
      } },
      on: (event: string, hook: Hook) => hooks.set(event, hook),
      registerTool: (tool: ToolDefinition) => tools.set(tool.name, tool),
      getActiveTools: () => [...active],
      setActiveTools: (names: string[]) => { active.clear(); names.forEach((name) => active.add(name)); },
    };
    const saved = { ...process.env };
    // Each extension instance snapshots its own run environment; restore the
    // test runner immediately so tests cannot accidentally depend on hot env.
    process.env = {
      ...sampleEnv, PATH: mode === "missing" ? join(cwd, "missing") : cwd,
      HOME: cwd, OPENAI_API_KEY: "real-credential-must-not-pass",
      ANTHROPIC_AUTH_TOKEN: "real-credential-must-not-pass", AWS_SECRET_ACCESS_KEY: "no-pass",
    };
    ducktapeExtension(pi as unknown as ExtensionAPI);
    process.env = saved;
    const invoke = (name: string, event = {}, ctx = {}) => Promise.resolve()
      .then(() => hooks.get(name)?.(event, { cwd, mode: "json", ...ctx }));
    const call = (operation: string, signal?: AbortSignal) => {
      const tool = tools.get("ducktape_fixture");
      assert.ok(tool);
      return tool.execute("call", { operation }, signal, undefined, {} as never);
    };
    t.after(() => invoke("session_shutdown"));
    const bind = (): DucktapeNetworkService => {
      const binding: { service?: DucktapeNetworkService } = {};
      events.get("ducktape:network:bind")?.({ accept: (service: DucktapeNetworkService) => { binding.service = service; } });
      assert.ok(binding.service);
      return binding.service;
    };
    return { cwd, tools, active, invoke, call, bind, events };
  });

const contentText = (result: { content: Array<{ type: string; text?: string }> }): string => {
  assert.equal(result.content[0].type, "text");
  assert.equal(typeof result.content[0].text, "string");
  return result.content[0].text!;
};

// --- Discovery and environment --------------------------------------------

test("factory and interactive sessions do not start a tool plane", async (t) => {
  const h = await harness(t);
  assert.equal(h.tools.size, 0);
  await h.invoke("session_start", {}, { mode: "tui" });
  await h.invoke("session_start", {}, { mode: "rpc" });
  assert.equal(h.tools.size, 0);
  assert.equal(await h.invoke("before_agent_start", { systemPrompt: "base" }), undefined);
});

test("headless discovery preserves schemas, guide, tool names and run env but not credentials", async (t) => {
  const h = await harness(t);
  await h.invoke("session_start");
  assert.deepEqual([...h.active], ["read", "bash", "ducktape_fixture"]);
  const schema = h.tools.get("ducktape_fixture")?.parameters as { properties: Record<string, unknown> };
  assert.deepEqual(schema.properties.nested, {
    type: "object", properties: { values: { type: "array", items: { type: "integer" } } },
  });
  const result = await h.call("env");
  const { env, frames, pid } = JSON.parse(contentText(result));
  Object.entries(sampleEnv).forEach(([key, value]) => {
    assert.equal(env[key], key.endsWith("TOKEN") ? "[redacted]" : value);
  });
  assert.equal(env.OPENAI_API_KEY, undefined);
  assert.equal(env.ANTHROPIC_AUTH_TOKEN, undefined);
  assert.equal(env.AWS_SECRET_ACCESS_KEY, undefined);
  assert.equal(env.HOME, h.cwd);
  assert.deepEqual(frames.map((frame: { method: string }) => frame.method), [
    "initialize", "notifications/initialized", "tools/list", "tools/call",
  ]);
  assert.equal(frames[1].id, undefined);
  assert.deepEqual(await h.invoke("before_agent_start", { systemPrompt: "base" }), {
    systemPrompt: "base\n\nUse ducktape_fixture to work on the network.",
  });
  await h.invoke("session_shutdown");
  await h.invoke("session_shutdown");
  assert.throws(() => process.kill(pid, 0), { code: "ESRCH" });
});

// --- Honest failures and cancellation -------------------------------------

test("tool and protocol refusals reject, unsupported content is not a success", async (t) => {
  const h = await harness(t);
  await h.invoke("session_start", {}, { mode: "print" });
  await assert.rejects(h.call("error"), /operation refused \[redacted\]/);
  await assert.rejects(h.call("protocol_error"), /protocol error -32601: fixture refusal/);
  await assert.rejects(h.call("unsupported"), /unsupported tool content/);
  assert.equal(JSON.parse(contentText(await h.call("echo"))).operation, "echo");
});

for (const fault of ["malformed", "invalid_error", "crash"]) {
  test(`${fault} rejects pending and future requests without hanging`, async (t) => {
    const h = await harness(t);
    await h.invoke("session_start");
    const pending = assert.rejects(h.call("hold"), /Ducktape MCP/);
    await assert.rejects(h.call(fault), /Ducktape MCP/);
    await pending;
    await assert.rejects(h.call("echo"), /Ducktape MCP/);
  });
}

for (const fault of ["missing", "bad-list"]) {
  test(`${fault} startup makes tool unavailability explicit in the model prompt`, async (t) => {
    const h = await harness(t, fault);
    await h.invoke("session_start");
    assert.equal(h.tools.size, 0);
    const guidance = await h.invoke("before_agent_start", { systemPrompt: "base" });
    assert.match(JSON.stringify(guidance), /Ducktape tools are unavailable/);
  });
}

test("abort before dispatch sends nothing; in-flight abort preserves correlation for later calls", async (t) => {
  const h = await harness(t);
  await h.invoke("session_start");
  await assert.rejects(h.call("echo", AbortSignal.abort()));
  const controller = new AbortController();
  const cancelled = assert.rejects(h.call("hold", controller.signal), /submitted action may still complete/);
  // A reply to this later frame proves the peer received the held request.
  await h.call("barrier");
  controller.abort();
  await cancelled;
  assert.equal(contentText(await h.call("release")), "release acknowledged");
  assert.equal(JSON.parse(contentText(await h.call("echo"))).operation, "echo");
});

test("shutdown rejects outstanding calls and reaps the child", async (t) => {
  const h = await harness(t);
  await h.invoke("session_start");
  const { pid } = JSON.parse(contentText(await h.call("env")));
  const pending = assert.rejects(h.call("hold"), /session closed/);
  await h.call("barrier");
  await h.invoke("session_shutdown");
  await pending;
  assert.throws(() => process.kill(pid, 0), { code: "ESRCH" });
});

test("shutdown forcibly reaps a server blocked inside a handler", async (t) => {
  const h = await harness(t);
  await h.invoke("session_start");
  const { pid } = JSON.parse(contentText(await h.call("env")));
  assert.equal(contentText(await h.call("block")), "blocked");
  await h.invoke("session_shutdown");
  assert.throws(() => process.kill(pid, 0), { code: "ESRCH" });
});

// --- Bounded, recoverable output ------------------------------------------

for (const operation of ["large", "lines"]) {
  test(`${operation} output is bounded with a complete private redacted artifact`, async (t) => {
    const h = await harness(t);
    await h.invoke("session_start");
    const text = contentText(await h.call(operation));
    assert.ok(Buffer.byteLength(text) < 50 * 1024);
    assert.ok(!text.includes("\uFFFD"), "UTF-8 preview must not end with a broken character");
    const path = text.match(/Full output: (.+)\]/)?.[1];
    assert.ok(path);
    t.after(() => rm(dirname(path), { recursive: true, force: true }));
    assert.equal((await stat(path)).mode & 0o777, 0o600);
    const full = await readFile(path, "utf8");
    assert.ok(full.length > text.length);
    assert.ok(!full.includes(sampleEnv.DUCKTAPE_RUN_ACTION_TOKEN));
  });
}

// --- Real binary parity (optional; no network or credential required) ------

test("compiled Ducktape MCP catalog and unbound refusal round-trip", { skip: !realBinary }, async (t) => {
  assert.ok(realBinary);
  const binary = resolve(realBinary);
  const h = await harness(t, "normal", binary);
  const catalog = await new Promise<string>((resolve, reject) => {
    const child = execFile(binary, ["mcp"], { env: {}, maxBuffer: 1024 * 1024 }, (error, stdout) => {
      if (error) return reject(error);
      resolve(stdout);
    });
    child.stdin!.end('{"jsonrpc":"2.0","id":1,"method":"tools/list"}\n');
  });
  const expected = JSON.parse(catalog).result.tools;
  await h.invoke("session_start");
  assert.deepEqual([...h.tools.keys()], expected.map((tool: { name: string }) => tool.name));
  expected.forEach((tool: { name: string; inputSchema: unknown }) => {
    assert.deepEqual(h.tools.get(tool.name)?.parameters, tool.inputSchema);
  });
  const action = h.tools.get("ducktape_action");
  assert.ok(action);
  await assert.rejects(action.execute("id", {}, undefined, undefined, {} as never));
});

// --- Generic package service ----------------------------------------------

test("package service binds before startup, waits for discovery, and retains full envelopes", async (t) => {
  const h = await harness(t);
  const service = h.bind();
  const pending = service.callTool("ducktape_fixture", { operation: "large" });
  await h.invoke("session_start");
  const result = await pending;
  assert.equal(result.isError, false);
  assert.equal(result.content[0].type, "text");
  assert.equal(result.content[0].text, "[redacted]\n" + "αβγ😀".repeat(30_000));
  const refused = await service.callTool("ducktape_fixture", { operation: "error" });
  assert.equal(refused.isError, true);
  assert.equal(refused.content[0].text, "operation refused [redacted]");
  await assert.rejects(service.callTool("not_discovered", {}), /not discovered/);
  await assert.rejects(service.callTool("ducktape_fixture", { operation: "protocol_error" }), /protocol error/);
  const controller = new AbortController();
  const cancelled = assert.rejects(service.callTool("ducktape_fixture", { operation: "hold" }, controller.signal), /cancelled/);
  await service.callTool("ducktape_fixture", { operation: "barrier" });
  controller.abort();
  await cancelled;
  await h.invoke("session_shutdown");
  await assert.rejects(service.callTool("ducktape_fixture", {}), /closed/);
  assert.equal(h.events.has("ducktape:network:bind"), false);
});

test("package service startup wait can be cancelled before a session starts", async (t) => {
  const h = await harness(t);
  const controller = new AbortController();
  const pending = assert.rejects(h.bind().callTool("ducktape_fixture", {}, controller.signal), /cancelled/);
  controller.abort();
  await pending;
  assert.equal(h.tools.size, 0);
});

test("package service startup failure rejects waiting and future calls", async (t) => {
  const h = await harness(t, "missing");
  const service = h.bind();
  const pending = assert.rejects(service.callTool("ducktape_fixture", {}), /Could not start|exited|closed/);
  await h.invoke("session_start");
  await pending;
  await assert.rejects(service.callTool("ducktape_fixture", {}));
});
