// Real Pi offline integration tests. Run with node --experimental-strip-types
// --test conversation.test.ts; PI_TEST_BINARY selects the installed Pi CLI.
// Test bodies are imperative to interleave process and checkpoint events.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { copyFile, cp, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import type { IncomingMessage, ServerResponse } from "node:http";
import { tmpdir } from "node:os";
import { basename, dirname, join } from "node:path";
import { test } from "node:test";
import type { TestContext } from "node:test";
import { fileURLToPath } from "node:url";
import { zstdDecompressSync } from "node:zlib";

import { vendorSse } from "./fixtures/native-wire.ts";
import type { Backend, WireTool } from "./fixtures/native-wire.ts";

const source = dirname(fileURLToPath(import.meta.url));
const binary = process.env.PI_TEST_BINARY ?? "pi";
const token = createHash("sha256").update("native-test-capability-must-never-be-archived").digest("hex");
const records = (jsonl: string): any[] => jsonl.trim().split("\n").map((line) => JSON.parse(line));
const messages = (jsonl: string): any[] => records(jsonl).filter((entry) => entry.type === "message").map((entry) => entry.message);
interface Checkpoint { kind: "checkpoint"; jsonl: string; delivery: boolean; complete: boolean; delivered_control_ids: string[] }
interface Effect { kind: string; data: any; checkpoint?: Checkpoint }
interface ControlMessage { id: string; text: string; kind: "steer" | "follow_up" }
interface ReportRequest { kind: "report"; operation_id: string; report_kind: "checkpoint" | "report"; payload: string }
interface WorkerReport { operation_id: string; worker: { module: string }; attempt: number; height: number; kind: "checkpoint" | "report"; payload: string }
interface ScriptedCall { type: "toolCall"; id: string; name: string; arguments: Record<string, unknown> }
const reportCall = (id: string, args: Record<string, unknown>): ScriptedCall => ({ type: "toolCall", id, name: "ducktape_report_job", arguments: args });

const harness = async (t: TestContext) => {
  const cwd = await mkdtemp(join(tmpdir(), "native-pi-test-"));
  const config = join(cwd, "config");
  const history = join(cwd, "history");
  await Promise.all([mkdir(config), mkdir(history)]);
  await Promise.all([
    copyFile(join(source, "conversation.ts"), join(config, "conversation.ts")),
    copyFile(join(source, "ducktape.ts"), join(config, "ducktape.ts")),
    copyFile(join(source, "fixtures/native-provider.ts"), join(cwd, "fixture.ts")),
    writeFile(join(config, "auth.json"), "{}"),
    writeFile(join(config, "models.json"), JSON.stringify({ providers: { offline: {
      api: "openai-completions", baseUrl: "http://127.0.0.1:1/never-called", apiKey: "offline-not-a-secret",
      models: [{ id: "fixture", name: "Offline", reasoning: false, input: ["text"], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 32768, maxTokens: 1024 }],
    } } })),
    writeFile(join(config, "settings.json"), "{}"),
    writeFile(join(cwd, "ducktape"), `#!/bin/sh\nexec ${JSON.stringify(process.execPath)} ${JSON.stringify(join(source, "fixtures/mcp.mjs"))}\n`, { mode: 0o700 }),
  ]);
  const state: {
    revision: number; checkpoints: Checkpoint[]; effects: Effect[]; requests: any[];
    reject?: (checkpoint: Checkpoint) => boolean;
    hold?: (checkpoint: Checkpoint) => Promise<void>;
    control: "continue" | "pause" | "abort";
    controls: ControlMessage[];
    cancel?: { id: string };
    currentTurn: string;
    currentRun: string;
    attempt: number;
    checkpointIdentity?: string;
    idempotentReplies: number;
    replyRevision?: number;
    requestLimit?: number;
    rejectedSizes: number[];
    settlements: string[];
    rejectSettlement?: boolean;
    holdSettlement?: (id: string) => Promise<void>;
    protocol: any[];
    jobReporting: boolean;
    packages: Array<{name: string; path: string}>;
    events: Array<{sequence: number; operation_id: string; actor: unknown; input: unknown; admitted_at: number}>;
    backend?: Backend;
    modelTool?: WireTool;
    env?: Record<string, string>;
    node?: (path: string, body: any) => Promise<unknown>;
    payloads: Array<{body: any; checkpoint?: Checkpoint}>;
    onEffect?: (effect: Effect) => void;
    captureStdout?: string;
    toolCalls?: ScriptedCall[];
    reportRequests: ReportRequest[];
    reports: Map<string, WorkerReport>;
    reportWrites: number;
    holdReport?: (request: ReportRequest) => Promise<void>;
    rejectReport?: boolean;
    reportReply?: (report: WorkerReport) => unknown;
  } = { revision: 0, checkpoints: [], effects: [], requests: [], control: "continue", controls: [], jobReporting: false, packages: [{ name: "offline", path: "fixture.ts" }], events: [], payloads: [], currentTurn: "", currentRun: "", attempt: 1, idempotentReplies: 0, rejectedSizes: [], settlements: [], protocol: [], reportRequests: [], reports: new Map(), reportWrites: 0 };
  const serve = async (request: IncomingMessage, response: ServerResponse) => {
    const chunks: Buffer[] = [];
    for await (const chunk of request) chunks.push(Buffer.from(chunk));
    const raw = Buffer.concat(chunks);
    const bytes = request.headers["content-encoding"] === "zstd" ? zstdDecompressSync(raw) : raw;
    const nativeRequest = request.url === "/v1/native-conversation";
    const exceedsRequestLimit = nativeRequest && state.requestLimit !== undefined && bytes.length > state.requestLimit;
    if (exceedsRequestLimit) {
      state.rejectedSizes.push(bytes.length);
      response.statusCode = 413;
      response.end("request body limit exceeded");
      return;
    }
    const body = JSON.parse(bytes.toString("utf8"));
    if (state.backend && request.url?.startsWith("/vendor/")) {
      state.payloads.push({ body, checkpoint: state.checkpoints.at(-1) });
      response.setHeader("content-type", "text/event-stream");
      response.end(vendorSse(state.backend, state.payloads.length, state.modelTool));
      return;
    }
    response.setHeader("content-type", "application/json");
    if (request.url === "/v1/run-action/fixture") {
      const effect = { ...body, checkpoint: state.checkpoints.at(-1) } as Effect;
      state.effects.push(effect);
      state.onEffect?.(effect);
      response.end(JSON.stringify(body.kind === "model" ? { tool_calls: state.toolCalls } : {}));
      return;
    }
    const moduleRequest = request.url === "/v1/query" || request.url === "/v1/run-action";
    if (state.node && moduleRequest) {
      if (request.url === "/v1/run-action") assert.equal(request.headers["x-ducktape-run-action"], token);
      response.end(JSON.stringify(await state.node(request.url!, body)));
      return;
    }
    assert.equal(request.url, "/v1/native-conversation");
    assert.equal(request.headers["x-ducktape-run-action"], token);
    state.requests.push(body);
    switch (body.kind) {
      case "checkpoint": {
        assert.ok(!body.jsonl.includes(token));
        await state.hold?.(body);
        if (state.reject?.(body)) { response.statusCode = 409; response.end("{}"); return; }
        // Mirror the real host's operation identity: an exact retry of the
        // current checkpoint in the same run+attempt returns its SAME revision.
        const identity = JSON.stringify([state.currentRun, state.attempt, body.jsonl, body.delivery, body.complete]);
        const repeated = identity === state.checkpointIdentity;
        if (repeated) state.idempotentReplies += 1;
        else {
          state.checkpoints.push(body);
          state.revision += 1;
          state.checkpointIdentity = identity;
        }
        state.controls = state.controls.filter((message) => !body.delivered_control_ids.includes(message.id));
        const cancellation = records(body.jsonl).find((entry) => entry.customType === "ducktape.cancel_delivery"
          && entry.data.turn_id === state.currentTurn && body.delivered_control_ids.includes(entry.data.id));
        if (cancellation) {
          assert.equal(body.complete, true);
          if (state.rejectSettlement) { response.statusCode = 503; response.end("{}"); return; }
          await state.holdSettlement?.(cancellation.data.id);
          if (!state.settlements.includes(cancellation.data.id)) state.settlements.push(cancellation.data.id);
          state.cancel = undefined;
        }
        response.end(JSON.stringify({ revision: state.replyRevision ?? state.revision }));
        return;
      }
      case "report": {
        assert.equal(state.jobReporting, true);
        assert.deepEqual(Object.keys(body).sort(), ["kind", "operation_id", "payload", "report_kind"]);
        state.reportRequests.push(body);
        await state.holdReport?.(body);
        if (state.rejectReport) { response.statusCode = 409; response.end("{}"); return; }
        const prior = state.reports.get(body.operation_id);
        const conflict = prior && (prior.kind !== body.report_kind || prior.payload !== body.payload);
        if (conflict) { response.statusCode = 409; response.end("{}"); return; }
        const report: WorkerReport = prior ?? { operation_id: body.operation_id, worker: { module: "runs" },
          attempt: 3, height: 100 + state.reportWrites, kind: body.report_kind, payload: body.payload };
        if (!prior) { state.reports.set(body.operation_id, report); state.reportWrites++; }
        response.end(JSON.stringify(state.reportReply ? state.reportReply(report) : { report }));
        return;
      }
      case "control": response.end(JSON.stringify({ control: state.control, messages: state.controls, cancel: state.cancel })); return;
      default: throw new Error("unexpected fixture request");
    }
  };
  const server = createServer((req, res) => { serve(req, res).catch((error) => { res.statusCode = 500; res.end("{}"); t.diagnostic(String(error)); }); });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  assert.ok(address && typeof address !== "string");
  t.after(async () => { server.closeAllConnections(); await new Promise<void>((resolve) => server.close(() => resolve())); await rm(cwd, { recursive: true, force: true }); });
  const run = async (turn: string, prompt = "first input", mode = "normal", runId = turn) => {
    state.currentTurn = turn;
    state.currentRun = runId;
    // Re-materialize the host's last committed snapshot, discarding any local
    // writes from a process whose checkpoint reply was lost or refused.
    const committed = state.checkpoints.at(-1)?.jsonl;
    if (committed) await writeFile(join(history, "session.jsonl"), committed);
    else await rm(join(history, "session.jsonl"), { force: true });
    await writeFile(join(config, "conversation.json"), JSON.stringify({
      conversation_id: "fixture-conversation", turn_id: turn, revision: state.revision,
      session_path: "history/session.jsonl", packages: state.packages, events: state.events,
      system_prompt: "Immutable native fixture instructions.", job_reporting: state.jobReporting,
    }));
    const providerToken = `header.${Buffer.from(JSON.stringify({ "https://api.openai.com/auth": { chatgpt_account_id: "offline-account" } })).toString("base64url")}.signature`;
    if (state.backend) {
      await writeFile(join(config, "models.json"), JSON.stringify({ providers: { [state.backend]: {
        api: state.backend === "anthropic" ? "anthropic-messages" : "openai-codex-responses",
        baseUrl: `http://127.0.0.1:${address.port}/vendor`, apiKey: "$DUCKTAPE_BROKER_TOKEN",
        models: [{ id: "fixture", name: "Offline built-in", reasoning: false, input: ["text"], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 128000, maxTokens: 1024 }],
      } } }));
      await writeFile(join(cwd, "input.txt"), "Native builtin read result.");
    }
    const child = spawn(binary, ["--print", "--mode", "json", "--no-session", "--no-extensions", "--no-skills", "--no-prompt-templates", "--no-themes", "--no-context-files", "--no-approve", "--provider", state.backend ?? "offline", "--model", "fixture", "-e", join(config, "conversation.ts")], {
      cwd, env: { PATH: `${cwd}:${process.env.PATH}`, HOME: cwd, PI_CODING_AGENT_DIR: config,
        DUCKTAPE_RUN_ACTION_URL: `http://127.0.0.1:${address.port}/v1/run-action`, DUCKTAPE_RUN_ACTION_TOKEN: token, DUCKTAPE_BROKER_TOKEN: providerToken,
        DUCKTAPE_NODE: `http://127.0.0.1:${address.port}`, DUCKTAPE_RUN_ID: runId,
        DUCKTAPE_NATIVE_FIXTURE_MODE: mode, PI_SKIP_VERSION_CHECK: "1", ...state.env },
      stdio: ["pipe", "pipe", "pipe"],
    });
    const stdout: Buffer[] = [];
    const stderr: Buffer[] = [];
    const frames = { partial: "", errors: [] as string[] };
    child.stdout.on("data", (chunk) => {
      stdout.push(chunk);
      const lines = (frames.partial + chunk.toString("utf8")).split("\n");
      frames.partial = lines.pop()!;
      lines.filter(Boolean).forEach((line) => {
        try { state.protocol.push(JSON.parse(line)); }
        catch (error) { frames.errors.push(String(error)); }
      });
    });
    child.stderr.on("data", (chunk) => stderr.push(chunk));
    const result = once(child, "close");
    child.stdin.end(`/ducktape-conversation ${Buffer.from(prompt).toString("base64")}\n`);
    const [code, signal] = await result;
    const output = Buffer.concat(stdout).toString("utf8");
    if (state.captureStdout) {
      await mkdir(dirname(state.captureStdout), { recursive: true });
      await writeFile(state.captureStdout, output);
      await writeFile(`${state.captureStdout}.stderr.log`, Buffer.concat(stderr));
    }
    assert.deepEqual(frames.errors, [], `malformed protocol frames; raw output: ${state.captureStdout ?? "not captured"}`);
    assert.equal(frames.partial.length, 0, `native output must end at a complete JSONL frame (exit=${code}, signal=${signal}); raw output: ${state.captureStdout ?? "not captured"}`);
    return { code, signal, stdout: output, stderr: Buffer.concat(stderr).toString("utf8") };
  };
  return { state, run, cwd };
};

const success = (result: Awaited<ReturnType<Awaited<ReturnType<typeof harness>>["run"]>>) =>
  assert.equal(result.code, 0, `${result.stderr}\n${result.stdout}`);

test("real Pi persists input before model, assistant before tools, each result before sibling, and restores two turns", async (t) => {
  const h = await harness(t);
  success(await h.run("turn-1"));
  assert.equal(h.state.effects.filter((effect) => effect.kind === "model").length, 2);
  const firstModel = h.state.effects.find((effect) => effect.kind === "model")!;
  assert.equal(firstModel.checkpoint?.delivery, true);
  assert.equal(messages(firstModel.checkpoint!.jsonl).at(-1).role, "user");
  const tools = h.state.effects.filter((effect) => effect.kind === "tool");
  assert.equal(tools.length, 2);
  assert.equal(messages(tools[0].checkpoint!.jsonl).at(-1).role, "assistant");
  assert.equal(messages(tools[1].checkpoint!.jsonl).at(-1).role, "toolResult");
  const first = records(h.state.checkpoints.at(-1)!.jsonl);
  assert.equal(h.state.checkpoints.at(-1)?.complete, true);
  success(await h.run("turn-2", "second input"));
  const second = records(h.state.checkpoints.at(-1)!.jsonl);
  assert.deepEqual(second.slice(0, first.length), first);
  const users = messages(h.state.checkpoints.at(-1)!.jsonl).filter((message) => message.role === "user");
  assert.equal(users.length, 2);
  assert.equal(users[1].content[0].text, "second input");
  assert.deepEqual(messages(h.state.checkpoints.at(-1)!.jsonl).find((message) => message.role === "toolResult").details.exact, { nested: [1, "opaque-payload"] });
  const requestsBefore = h.state.effects.length;
  const redelivery = await h.run("turn-2", "second input");
  success(redelivery);
  assert.equal(h.state.effects.length, requestsBefore, "redelivery must not repeat a model or tool effect");
  const restored = records(redelivery.stdout).find((event) => event.type === "message_end" && event.restored);
  assert.ok(restored, redelivery.stdout + redelivery.stderr);
  assert.deepEqual(restored.message, messages(h.state.checkpoints.at(-1)!.jsonl).at(-1));
});

test("rapid unawaited large tool updates stay exact complete JSONL frames with intact native history", async (t) => {
  const h = await harness(t);
  h.state.captureStdout = join(source, "../../../../../target/chief-gates/native/provider-overlapping-frames.raw.jsonl");
  const result = await h.run("turn-1", "stream large partial updates", "stream-updates");
  success(result);
  const updates = records(result.stdout).filter((event) => event.type === "tool_execution_update");
  assert.equal(updates.length, 6);
  for (const number of [1, 2]) for (const index of [0, 1, 2]) {
    const update = updates.find((event) => event.partialResult.details.number === number && event.partialResult.details.index === index);
    assert.ok(update);
    assert.equal(update.partialResult.content[0].text, String.fromCharCode(65 + number * 3 + index).repeat(192 * 1024));
  }
  const history = h.state.checkpoints.at(-1)!.jsonl;
  assert.equal(h.state.checkpoints.at(-1)!.complete, true);
  assert.equal(messages(history).filter((message) => message.role === "user").length, 1);
  assert.deepEqual(messages(history).filter((message) => message.role === "toolResult").map((message) => message.content[0].text), ["result-1", "result-2"]);
  assert.equal(messages(history).at(-1).stopReason, "stop");
  assert.ok(!history.includes("D".repeat(192 * 1024)), "partial tool updates are protocol events, not invented native messages");
  const effects = h.state.effects.length;
  h.state.captureStdout = undefined;
  success(await h.run("turn-1", "stream large partial updates", "stream-updates"));
  assert.equal(h.state.effects.length, effects, "restoring the committed native answer must not replay updates or tools");
});

test("worker report waits for committed host response, retries idempotently, and native replay never reports again", async (t) => {
  const h = await harness(t);
  h.state.jobReporting = true;
  const input = { operation_id: "opaque-report-operation", kind: "checkpoint", payload: '  {"not_a_policy_schema": [3, "opaque é"]}\n\n' };
  h.state.toolCalls = [reportCall("report-first", input), reportCall("report-retry", input),
    { type: "toolCall", id: "after-report", name: "native_fixture", arguments: { number: 1 } }];
  const reached = Promise.withResolvers<void>();
  const release = Promise.withResolvers<void>();
  t.after(() => release.resolve());
  h.state.holdReport = () => { reached.resolve(); return release.promise; };
  const running = h.run("worker-turn", "publish progress", "normal", "worker-execution");
  assert.equal(await Promise.race([reached.promise.then(() => "report"), running.then(() => "exited")]), "report");
  assert.equal(h.state.reportWrites, 0);
  assert.equal(h.state.reportRequests.length, 1);
  assert.equal(h.state.effects.filter((effect) => effect.kind === "model").length, 1);
  assert.equal(h.state.effects.filter((effect) => effect.kind === "tool").length, 0);
  assert.equal(messages(h.state.checkpoints.at(-1)!.jsonl).filter((message) => message.role === "toolResult").length, 0);
  release.resolve();
  success(await running);
  assert.deepEqual(h.state.reportRequests, [0, 1].map(() => ({ kind: "report", operation_id: input.operation_id, report_kind: "checkpoint", payload: input.payload })));
  assert.equal(h.state.reportWrites, 1, "same operation ID and bytes reuse the committed immutable report");
  const expected = { report: h.state.reports.get(input.operation_id)! };
  assert.equal(expected.report.attempt, 3, "the host's job claim attempt is not the native run attempt");
  assert.equal(expected.report.payload, input.payload);
  const native = messages(h.state.checkpoints.at(-1)!.jsonl);
  const reports = native.filter((message) => message.role === "toolResult" && message.toolName === "ducktape_report_job");
  assert.equal(reports.length, 2);
  for (const report of reports) {
    assert.equal(report.isError, false);
    assert.deepEqual(JSON.parse(report.content[0].text), expected);
    assert.deepEqual(report.details, expected);
  }
  const sibling = h.state.effects.find((effect) => effect.kind === "tool")!;
  assert.equal(messages(sibling.checkpoint!.jsonl).filter((message) => message.role === "toolResult" && message.toolName === "ducktape_report_job").length, 2);
  assert.equal(native.find((message) => message.role === "toolResult" && message.toolName === "native_fixture").isError, false);
  const models = h.state.effects.filter((effect) => effect.kind === "model");
  assert.equal(models.length, 2);
  assert.ok(models[0].data.tools.some((tool: any) => tool.name === "ducktape_report_job"));
  const effects = h.state.effects.length;
  success(await h.run("worker-turn", "do not duplicate input", "normal", "worker-execution-retry"));
  assert.equal(h.state.effects.length, effects);
  assert.equal(h.state.reportRequests.length, 2);
  assert.equal(messages(h.state.checkpoints.at(-1)!.jsonl).filter((message) => message.role === "user").length, 1);
});

test("interrupted native persistence after a committed report repairs the unknown tool without reposting", async (t) => {
  const h = await harness(t);
  h.state.jobReporting = true;
  h.state.toolCalls = [reportCall("report-before-crash", { operation_id: "retained-report", kind: "checkpoint", payload: "already committed" })];
  h.state.reject = (checkpoint) => messages(checkpoint.jsonl).some((message) => message.role === "toolResult");
  assert.notEqual((await h.run("worker-turn")).code, 0);
  assert.equal(h.state.reportWrites, 1);
  assert.equal(h.state.reportRequests.length, 1);
  const original = h.state.checkpoints.at(-1)!.jsonl;
  assert.deepEqual(messages(original).map((message) => message.role), ["user", "assistant"]);
  h.state.reject = undefined;
  success(await h.run("worker-turn"));
  assert.equal(h.state.reportWrites, 1);
  assert.equal(h.state.reportRequests.length, 1, "unknown-outcome tools are never automatically reposted");
  const history = h.state.checkpoints.at(-1)!.jsonl;
  assert.ok(history.startsWith(original));
  const result = messages(history).find((message) => message.role === "toolResult");
  assert.equal(result.isError, true);
  assert.equal(result.details.reason, "interrupted_unknown_outcome");
  assert.equal(messages(history).filter((message) => message.role === "user").length, 1);
});

test("nonworker manifest exposes no report tool and cannot reach the report endpoint", async (t) => {
  const h = await harness(t);
  h.state.toolCalls = [reportCall("forbidden-report", { operation_id: "no-worker", kind: "report", payload: "opaque report" })];
  success(await h.run("ordinary-turn"));
  const models = h.state.effects.filter((effect) => effect.kind === "model");
  assert.ok(models.every((effect) => !effect.data.tools.some((tool: any) => tool.name === "ducktape_report_job")));
  assert.equal(h.state.reportRequests.length, 0);
  const result = messages(h.state.checkpoints.at(-1)!.jsonl).find((message) => message.role === "toolResult");
  assert.equal(result.isError, true);
  assert.equal(result.details?.report, undefined);
});

test("host report refusal produces a native failed tool result, never a committed ACK", async (t) => {
  const h = await harness(t);
  h.state.jobReporting = true;
  h.state.rejectReport = true;
  h.state.toolCalls = [reportCall("refused-report", { operation_id: "refused", kind: "report", payload: "uncommitted report" })];
  success(await h.run("worker-turn"));
  assert.equal(h.state.reportRequests.length, 1);
  assert.equal(h.state.reportWrites, 0);
  assert.equal(h.state.reports.size, 0);
  const result = messages(h.state.checkpoints.at(-1)!.jsonl).find((message) => message.role === "toolResult");
  assert.equal(result.isError, true);
  assert.equal(result.details?.report, undefined);
  assert.match(result.content[0].text, /native_host_refused/);
});

test("report tool enforces UTF-8 byte bounds and excludes model-supplied execution identity", async (t) => {
  const h = await harness(t);
  h.state.jobReporting = true;
  const boundary = { operation_id: "é".repeat(128), kind: "report", payload: "😀".repeat(1024) };
  h.state.toolCalls = [
    reportCall("exact-boundary", boundary),
    reportCall("long-id", { ...boundary, operation_id: "é".repeat(129) }),
    reportCall("long-payload", { ...boundary, payload: "😀".repeat(1025) }),
    reportCall("empty-id", { ...boundary, operation_id: "" }),
    reportCall("blank-payload", { ...boundary, payload: " \n " }),
    reportCall("reserved-id", { ...boundary, operation_id: "bad\u001fidentity" }),
    reportCall("invalid-kind", { ...boundary, kind: "accepted" }),
    reportCall("injected-identity", { ...boundary, run_id: "other-run", job_id: "other-job", attempt: 99 }),
  ];
  success(await h.run("worker-turn"));
  assert.equal(h.state.reportRequests.length, 1);
  assert.equal(h.state.reportRequests[0].operation_id, boundary.operation_id);
  assert.equal(h.state.reportRequests[0].payload, boundary.payload);
  const results = messages(h.state.checkpoints.at(-1)!.jsonl).filter((message) => message.role === "toolResult");
  assert.deepEqual(results.map((result) => result.isError), [false, true, true, true, true, true, true, true]);
});

for (const corruption of ["different-operation", "different-payload", "invalid-worker", "invalid-height", "history-head"] as const) test(`report tool rejects ${corruption} instead of acknowledging a report`, async (t) => {
  const h = await harness(t);
  h.state.jobReporting = true;
  h.state.toolCalls = [reportCall("bad-response", { operation_id: "report-response", kind: "checkpoint", payload: "exact original text" })];
  h.state.reportReply = (report) => {
    switch (corruption) {
      case "different-operation": return { report: { ...report, operation_id: "other-operation" } };
      case "different-payload": return { report: { ...report, payload: "other text" } };
      case "invalid-worker": return { report: { ...report, worker: { module: "runs", account: 7 } } };
      case "invalid-height": return { report: { ...report, height: "not-a-height" } };
      case "history-head": return { revision: h.state.revision };
    }
  };
  success(await h.run("worker-turn"));
  const result = messages(h.state.checkpoints.at(-1)!.jsonl).find((message) => message.role === "toolResult");
  assert.equal(result.isError, true);
  assert.equal(result.details?.report, undefined);
});

test("input checkpoint refusal fails closed before any model/tool effect", async (t) => {
  const h = await harness(t);
  h.state.reject = (checkpoint) => checkpoint.delivery;
  const result = await h.run("turn-1");
  assert.notEqual(result.code, 0);
  assert.equal(h.state.effects.length, 0);
  assert.ok(!h.state.checkpoints.some((checkpoint) => checkpoint.delivery || checkpoint.complete));
});

test("413 preserves the accepted head and unknown-tool recovery can fit the unchanged request limit", async (t) => {
  const h = await harness(t);
  h.state.requestLimit = 16 * 1024; // Scaled peer limit; the real host boundary is tested at 64 MiB.
  const failed = await h.run("events/0/1", "original input", "large-result");
  assert.notEqual(failed.code, 0);
  assert.ok(h.state.rejectedSizes.every((size) => size > h.state.requestLimit!));
  assert.ok(h.state.rejectedSizes.length > 0);
  assert.ok(h.state.protocol.some((event) => event.type === "native_conversation_error" && event.reason === "native_history_request_too_large"));
  const original = h.state.checkpoints.at(-1)!.jsonl;
  assert.ok(!messages(original).some((message) => message.role === "toolResult"));
  assert.equal(h.state.effects.filter((effect) => effect.kind === "tool").length, 1);
  success(await h.run("events/0/1", "original input"));
  const restored = h.state.checkpoints.at(-1)!.jsonl;
  assert.ok(restored.startsWith(original));
  assert.equal(h.state.requestLimit, 16 * 1024);
  assert.equal(h.state.effects.filter((effect) => effect.kind === "tool").length, 1);
  assert.equal(messages(restored).filter((message) => message.role === "user").length, 1);
  assert.ok(messages(restored).filter((message) => message.role === "toolResult").every((message) => message.details.reason === "interrupted_unknown_outcome"));
});

test("a full-history request limit blocks new input and retries without discarding accepted history", async (t) => {
  const h = await harness(t);
  success(await h.run("events/0/1"));
  // Every accepted seed request fits this fixed limit; the next native entry cannot.
  h.state.requestLimit = Math.max(...h.state.checkpoints.map((checkpoint) => Buffer.byteLength(JSON.stringify(checkpoint))));
  const original = h.state.checkpoints.at(-1)!.jsonl;
  const revision = h.state.revision;
  const effectCount = h.state.effects.length;
  const protocolCount = h.state.protocol.length;
  assert.notEqual((await h.run("events/1/2", "next input")).code, 0);
  assert.notEqual((await h.run("events/1/2", "next input", "normal", "retry/new-run")).code, 0);
  assert.equal(h.state.revision, revision);
  assert.equal(h.state.checkpoints.at(-1)!.jsonl, original);
  assert.equal(h.state.effects.length, effectCount);
  const rejectedEvents = h.state.protocol.slice(protocolCount);
  assert.equal(rejectedEvents.filter((event) => event.type === "native_conversation_error" && event.reason === "native_history_request_too_large").length, 2);
  assert.ok(!rejectedEvents.some((event) => ["native_conversation_handled", "native_conversation_cancelled"].includes(event.type)));
  assert.ok(!rejectedEvents.some((event) => event.type === "message_end" && event.message?.role === "assistant"));
});

test("model waits for an outstanding input checkpoint acknowledgement", async (t) => {
  const h = await harness(t);
  const reached = Promise.withResolvers<void>();
  const release = Promise.withResolvers<void>();
  h.state.hold = (checkpoint) => {
    if (!checkpoint.delivery) return Promise.resolve();
    reached.resolve();
    return release.promise;
  };
  const running = h.run("turn-1");
  await reached.promise;
  assert.equal(h.state.effects.length, 0);
  release.resolve();
  success(await running);
  assert.equal(h.state.effects[0].checkpoint?.delivery, true);
});

test("restart after a partial tool batch retains committed result and repairs only the missing result", async (t) => {
  const h = await harness(t);
  h.state.reject = (checkpoint) => messages(checkpoint.jsonl).filter((message) => message.role === "toolResult").length === 2;
  assert.notEqual((await h.run("turn-1")).code, 0);
  const committed = messages(h.state.checkpoints.at(-1)!.jsonl).find((message) => message.role === "toolResult");
  assert.ok(committed && !committed.isError);
  h.state.reject = undefined;
  success(await h.run("turn-1"));
  assert.equal(h.state.effects.filter((effect) => effect.kind === "tool").length, 2);
  const results = messages(h.state.checkpoints.at(-1)!.jsonl).filter((message) => message.role === "toolResult");
  assert.deepEqual(results[0], committed);
  assert.equal(results[1].details.reason, "interrupted_unknown_outcome");
});

test("same-attempt tool-call checkpoint restart skips unchanged publication and repairs unknown outcomes", async (t) => {
  const h = await harness(t);
  h.state.reject = (checkpoint) => messages(checkpoint.jsonl).some((message) => message.role === "toolResult");
  assert.notEqual((await h.run("turn-1")).code, 0);
  assert.equal(h.state.effects.filter((effect) => effect.kind === "tool").length, 1);
  const original = h.state.checkpoints.at(-1)!;
  assert.equal(original.complete, false);
  assert.deepEqual(messages(original.jsonl).map((message) => message.role), ["user", "assistant"]);
  const before = h.state.requests.length;
  h.state.reject = undefined;
  success(await h.run("turn-1")); // Same run id AND attempt; the host's fence still runs.
  const resumed = h.state.requests.slice(before);
  assert.equal(resumed[0].kind, "control");
  assert.ok(!resumed.some((request) => request.kind === "checkpoint" && request.jsonl === original.jsonl && !request.complete));
  const repaired = resumed.find((request) => request.kind === "checkpoint");
  assert.ok(repaired.jsonl.startsWith(original.jsonl));
  assert.equal(messages(repaired.jsonl).at(-1).role, "toolResult");
  assert.equal(h.state.effects.filter((effect) => effect.kind === "tool").length, 1);
  const results = messages(h.state.checkpoints.at(-1)!.jsonl).filter((message) => message.role === "toolResult");
  assert.equal(results.length, 2);
  assert.ok(results.every((message) => message.isError && message.details.reason === "interrupted_unknown_outcome"));
  assert.equal(messages(h.state.checkpoints.at(-1)!.jsonl).filter((message) => message.role === "user").length, 1);
  const completedRevision = h.state.revision;
  success(await h.run("turn-1"));
  assert.equal(h.state.revision, completedRevision, "idempotent final retry must return the existing revision");
  assert.ok(h.state.idempotentReplies > 0);
});

test("explicit retry uses a new opaque execution id without duplicating a logical input range", async (t) => {
  const h = await harness(t);
  const turn = "events/0/1";
  const initialRun = "chat\u001fchannel\u001f1\u001fresident";
  const retryRun = "conversation/frozen-range/2";
  h.state.reject = (checkpoint) => messages(checkpoint.jsonl).some((message) => message.role === "toolResult");
  assert.notEqual((await h.run(turn, "original input", "normal", initialRun)).code, 0);
  const original = h.state.checkpoints.at(-1)!;
  const originalUser = messages(original.jsonl).find((message) => message.role === "user");
  h.state.reject = undefined;
  success(await h.run(turn, "must not become another user", "normal", retryRun));
  const restored = h.state.checkpoints.at(-1)!.jsonl;
  assert.ok(restored.startsWith(original.jsonl));
  assert.deepEqual(messages(restored).filter((message) => message.role === "user"), [originalUser]);
  assert.equal(h.state.effects.filter((effect) => effect.kind === "tool").length, 1);
  const deliveries = records(restored).filter((entry) => entry.customType === "ducktape.turn_delivery");
  assert.equal(deliveries.length, 1);
  assert.equal(deliveries[0].data.turn_id, turn);
  assert.equal(JSON.parse(h.state.checkpointIdentity!)[0], retryRun);
  assert.equal(h.state.attempt, 1, "a retry run is distinct even when attempt numbering restarts");
});

test("same-attempt input-only checkpoint restart retains the original user before retrying the model", async (t) => {
  const h = await harness(t);
  h.state.hold = (checkpoint) => {
    if (checkpoint.delivery) h.state.control = "pause";
    return Promise.resolve();
  };
  assert.notEqual((await h.run("turn-1")).code, 0);
  const original = h.state.checkpoints.at(-1)!;
  assert.equal(original.complete, false);
  assert.deepEqual(messages(original.jsonl).map((message) => message.role), ["user"]);
  assert.equal(h.state.effects.length, 0);
  const before = h.state.requests.length;
  h.state.control = "continue";
  h.state.hold = undefined;
  success(await h.run("turn-1"));
  const resumed = h.state.requests.slice(before);
  assert.equal(resumed[0].kind, "control");
  assert.ok(!resumed.some((request) => request.kind === "checkpoint" && request.jsonl === original.jsonl && !request.complete));
  const restored = h.state.checkpoints.at(-1)!.jsonl;
  assert.ok(restored.startsWith(original.jsonl));
  assert.equal(messages(restored).filter((message) => message.role === "user").length, 1);
});

test("a new native entry cannot reuse the restored revision", async (t) => {
  const h = await harness(t);
  success(await h.run("turn-1"));
  const before = h.state.effects.length;
  h.state.replyRevision = h.state.revision;
  assert.notEqual((await h.run("turn-2", "second input")).code, 0);
  assert.equal(h.state.effects.length, before, "the stale response must stop the new turn before its model request");
  assert.equal(h.state.checkpoints.at(-1)!.complete, false);
});

test("secret-bearing tool output cannot reach native snapshots or stdout", async (t) => {
  const h = await harness(t);
  const result = await h.run("turn-1", "first input", "secret");
  assert.notEqual(result.code, 0);
  assert.ok(!JSON.stringify(h.state.requests).includes(token));
  assert.ok(!result.stdout.includes(token));
  assert.ok(!result.stderr.includes(token));
  assert.equal(h.state.effects.filter((effect) => effect.kind === "tool").length, 1);
});

test("safe-boundary pause stops before model effects", async (t) => {
  const h = await harness(t);
  h.state.control = "pause";
  assert.notEqual((await h.run("turn-1")).code, 0);
  assert.equal(h.state.effects.length, 0);
});

test("explicit cancellation before input waits for committed host settlement without fabricated messages", async (t) => {
  const h = await harness(t);
  const id = JSON.stringify(["turn-1", "cancel-1"]);
  h.state.cancel = { id };
  const reached = Promise.withResolvers<void>();
  const release = Promise.withResolvers<void>();
  h.state.holdSettlement = async (received) => { assert.equal(received, id); reached.resolve(); await release.promise; };
  const running = h.run("turn-1");
  await reached.promise;
  assert.equal(h.state.effects.length, 0);
  assert.deepEqual(h.state.settlements, []);
  assert.ok(!h.state.protocol.some((event) => event.type === "native_conversation_cancelled"));
  release.resolve();
  const result = await running;
  success(result);
  assert.deepEqual(h.state.settlements, [id]);
  assert.equal(records(result.stdout).filter((event) => event.type === "native_conversation_cancelled").length, 1);
  const checkpoint = h.state.checkpoints.at(-1)!;
  assert.equal(checkpoint.delivery, false);
  assert.equal(checkpoint.complete, true);
  assert.deepEqual(checkpoint.delivered_control_ids, [id]);
  assert.deepEqual(messages(checkpoint.jsonl), []);
  assert.deepEqual(records(checkpoint.jsonl).find((entry) => entry.customType === "ducktape.cancel_delivery").data,
    { conversation_id: "fixture-conversation", turn_id: "turn-1", id });
});

test("cancellation between sibling tools preserves partial native history and stops further effects", async (t) => {
  const h = await harness(t);
  const id = JSON.stringify(["turn-1", "cancel-after-tool"]);
  h.state.onEffect = (effect) => { if (effect.kind === "tool") h.state.cancel = { id }; };
  const result = await h.run("turn-1");
  success(result);
  assert.equal(h.state.effects.filter((effect) => effect.kind === "model").length, 1);
  assert.equal(h.state.effects.filter((effect) => effect.kind === "tool").length, 1);
  const checkpoint = h.state.checkpoints.at(-1)!;
  assert.equal(checkpoint.delivery, true);
  assert.deepEqual(messages(checkpoint.jsonl).map((message) => message.role), ["user", "assistant", "toolResult"]);
  assert.equal(messages(checkpoint.jsonl)[1].stopReason, "toolUse");
  assert.deepEqual(h.state.settlements, [id]);
  assert.equal(records(result.stdout).filter((event) => event.type === "native_conversation_cancelled").length, 1);
});

test("failed final cancellation settlement is not success, and a restored receipt reconciles without replay", async (t) => {
  const h = await harness(t);
  const id = JSON.stringify(["turn-1", "cancel-reconcile"]);
  h.state.cancel = { id };
  h.state.rejectSettlement = true;
  const refused = await h.run("turn-1");
  assert.notEqual(refused.code, 0);
  assert.ok(!records(refused.stdout).some((event) => event.type === "native_conversation_cancelled"));
  const committed = h.state.checkpoints.at(-1)!.jsonl;
  assert.equal(records(committed).filter((entry) => entry.customType === "ducktape.cancel_delivery").length, 1);
  assert.deepEqual(h.state.settlements, []);
  // Model the receipt commit surviving a crash after ACK removed the pending
  // control. Resume must reconcile that receipt, not depend on another notice.
  h.state.cancel = undefined;
  h.state.control = "abort";
  h.state.rejectSettlement = false;
  const reconciled = await h.run("turn-1");
  success(reconciled);
  assert.equal(h.state.effects.length, 0);
  assert.equal(h.state.checkpoints.at(-1)!.jsonl, committed);
  assert.deepEqual(h.state.settlements, [id]);
  assert.equal(records(reconciled.stdout).filter((event) => event.type === "native_conversation_cancelled").length, 1);
  success(await h.run("turn-1"));
  assert.deepEqual(h.state.settlements, [id]);
});

test("plain abort remains a failure without a cancellation receipt or terminal marker", async (t) => {
  const h = await harness(t);
  h.state.control = "abort";
  const result = await h.run("turn-1");
  assert.notEqual(result.code, 0);
  assert.ok(!records(result.stdout).some((event) => event.type === "native_conversation_cancelled"));
  assert.ok(!h.state.checkpoints.some((checkpoint) => records(checkpoint.jsonl).some((entry) => entry.customType === "ducktape.cancel_delivery")));
  assert.equal(h.state.effects.length, 0);
});

test("control input ids are acknowledged only in committed native user history", async (t) => {
  const h = await harness(t);
  h.state.onEffect = (effect) => {
    if (effect.kind !== "tool" || effect.data.number !== 1) return;
    h.state.controls = [{ id: "control-1", text: "durable steering", kind: "steer" }];
  };
  success(await h.run("turn-1"));
  const acknowledged = h.state.checkpoints.find((checkpoint) => checkpoint.delivered_control_ids.includes("control-1"));
  assert.ok(acknowledged);
  assert.ok(messages(acknowledged.jsonl).some((message) => message.content?.[0]?.text === "durable steering"));
  assert.equal(h.state.controls.length, 0);
});

test("a persisted failed assistant retries without duplicating user or deleting native history", async (t) => {
  const h = await harness(t);
  assert.notEqual((await h.run("turn-1", "first input", "model-error")).code, 0);
  const failed = records(h.state.checkpoints.at(-1)!.jsonl);
  assert.equal(messages(h.state.checkpoints.at(-1)!.jsonl).at(-1).stopReason, "error");
  success(await h.run("turn-1"));
  assert.deepEqual(records(h.state.checkpoints.at(-1)!.jsonl).slice(0, failed.length), failed);
  assert.equal(messages(h.state.checkpoints.at(-1)!.jsonl).filter((message) => message.role === "user").length, 1);
});

test("native compaction entries and retained context survive a cold restart", async (t) => {
  const h = await harness(t);
  h.state.captureStdout = join(source, "../../../../../target/chief-gates/native/provider-compaction-frames.raw.jsonl");
  success(await h.run("turn-1", "first input", "compact"));
  const checkpoint = h.state.checkpoints.at(-1)!;
  assert.ok(records(checkpoint.jsonl).some((entry) => entry.type === "compaction"), JSON.stringify({ entries: records(checkpoint.jsonl).map((entry) => entry.type), effects: h.state.effects.map((effect) => ({kind: effect.kind, bytes: JSON.stringify(effect.data).length, contextWindow: effect.data.contextWindow})) }));
  success(await h.run("turn-2", "second input", "compact"));
  assert.ok(h.state.effects.filter((effect) => effect.kind === "model").at(-1)!.data.messages.some((message: any) => JSON.stringify(message).includes("OFFLINE COMPACTION")));
});

test("resident handled-only delivery checkpoints authenticated event ids, replays without a fake conversation or prepare effects", async (t) => {
  const h = await harness(t);
  h.state.events = [{ sequence: 4, operation_id: "operation-4", actor: { public_key: "frozen" }, input: { text: "not parsed as a command" }, admitted_at: 9 }];
  const result = await h.run("turn-1", "current input", "prepare-handled");
  success(result);
  assert.equal(h.state.effects.length, 1);
  assert.equal(h.state.effects[0].kind, "prepare");
  const receipt = records(h.state.checkpoints.at(-1)!.jsonl).find((entry) => entry.customType === "ducktape.input_handled");
  assert.deepEqual(receipt.data, { conversation_id: "fixture-conversation", turn_id: "turn-1", events: [{ sequence: 4, operation_id: "operation-4" }] });
  assert.deepEqual(messages(h.state.checkpoints.at(-1)!.jsonl), []);
  assert.ok(records(result.stdout).some((event) => event.type === "native_conversation_handled"));
  success(await h.run("turn-1", "current input", "prepare-handled"));
  assert.equal(h.state.effects.length, 1);
});

for (const mode of ["prepare-double", "prepare-invalid"]) test(`${mode} fails before input acknowledgement or model effect`, async (t) => {
  const h = await harness(t);
  assert.notEqual((await h.run("turn-1", "input", mode)).code, 0);
  assert.equal(h.state.effects.length, 0);
  assert.ok(!h.state.checkpoints.some((checkpoint) => checkpoint.delivery));
});

test("builtin bash cannot smuggle the run capability into native history or protocol output", async (t) => {
  const h = await harness(t);
  const result = await h.run("turn-1", "input", "bash-secret");
  assert.notEqual(result.code, 0);
  assert.ok(!JSON.stringify(h.state.requests).includes(token));
  assert.ok(!result.stdout.includes(token));
  assert.ok(!result.stderr.includes(token));
  assert.equal(h.state.effects.filter((effect) => effect.kind === "model").length, 1);
});

test("pinned Pi package manifest loads extensions, skills, prompts but no ambient resources", async (t) => {
  const h = await harness(t);
  await Promise.all([mkdir(join(h.cwd, "pinned/skills/fixture"), { recursive: true }), mkdir(join(h.cwd, "pinned/prompts"), { recursive: true }), mkdir(join(h.cwd, ".pi/extensions"), { recursive: true })]);
  await Promise.all([
    writeFile(join(h.cwd, "pinned/package.json"), JSON.stringify({ name: "pinned", pi: { extensions: ["./extension.ts"], skills: ["./skills"], prompts: ["./prompts"] } })),
    writeFile(join(h.cwd, "pinned/index.ts"), 'throw new Error("index.ts must not load");'),
    writeFile(join(h.cwd, "pinned/extension.ts"), `export default (pi) => { pi.on("before_agent_start", (event) => ({systemPrompt: event.systemPrompt + "\\nPINNED " + JSON.stringify({skills:event.systemPromptOptions.skills.map(s=>s.name), commands:pi.getCommands().map(c=>c.name)})})); };`),
    writeFile(join(h.cwd, "pinned/skills/fixture/SKILL.md"), "---\nname: fixture\ndescription: Pinned fixture skill\n---\nFixture skill content.\n"),
    writeFile(join(h.cwd, "pinned/prompts/pinned.md"), "---\ndescription: Pinned prompt\n---\nPinned prompt text.\n"),
    writeFile(join(h.cwd, ".pi/extensions/ambient.ts"), 'throw new Error("ambient extension must not load");'),
    writeFile(join(h.cwd, "AGENTS.md"), "AMBIENT_CONTEXT_MUST_NOT_LOAD"),
    writeFile(join(h.cwd, "SYSTEM.md"), "AMBIENT_SYSTEM_MUST_NOT_LOAD"),
  ]);
  h.state.packages.push({ name: "pinned", path: "pinned" });
  success(await h.run("turn-1"));
  const prefix = h.state.effects.find((effect) => effect.kind === "model")!.data.systemPrompt;
  assert.match(prefix, /PINNED.*fixture/);
  assert.match(prefix, /PINNED.*pinned/);
  assert.ok(!prefix.includes("AMBIENT_"));
});

test("real pinned Chief through native Pi and authenticated MCP handles progress without a model, then serves bounded human turns", async (t) => {
  const h = await harness(t);
  const chiefRoot = join(source, "../../../../../agents/chief");
  const chiefPath = join(h.cwd, "pinned-chief");
  await cp(chiefRoot, chiefPath, { recursive: true, filter: (path) => basename(path) !== "node_modules" && !basename(path).startsWith(".") });
  const { moduleFixture } = await import("./fixtures/native-modules.ts");
  const modules = moduleFixture(() => h.state.currentRun);
  await writeFile(join(chiefPath, "chief.config.json"), JSON.stringify(modules.config));
  const mcpBinary = process.env.DUCKTAPE_TEST_BINARY ?? join(source, "../../../../../target/debug/ducktape");
  await writeFile(join(h.cwd, "ducktape"), `#!/bin/sh\nexec ${JSON.stringify(mcpBinary)} "$@"\n`, { mode: 0o700 });
  const [{ CHIEF_TOOL_NAMES }, { CHIEF_PROMPT }] = await Promise.all([
    import("../../../../../agents/chief/tools.ts"), import("../../../../../agents/chief/prompt.ts"),
  ]);

  // Bootstrap one live task using real policy/record serialization, outside the
  // measured Pi runs. No fake prepare callback or scripted Chief service reply.
  h.state.currentRun = "bootstrap";
  const service = await modules.start();
  const boardSentinel = "FULL_PRIVATE_BOARD_BRIEF_MUST_NOT_ENTER_MODEL_CONTEXT";
  const seeded = await service.execute({ kind: "change", operationId: "seed-task", expectedRevision: 0,
    action: { kind: "task_put", task: { id: "task1", key: "task1", title: "Live worker task", brief: boardSentinel.repeat(100), scope: ["src"], access: "read", dependencies: [] } } });
  assert(seeded.success, JSON.stringify(seeded));
  const dispatched = await service.execute({ kind: "dispatch", operationId: "seed-dispatch", expectedRevision: modules.state.revision,
    taskId: "task1", fresh: true });
  assert(dispatched.success, JSON.stringify(dispatched));
  const job = [...modules.state.jobs.values()][0];
  job.status = "processing";
  const payload = JSON.stringify({ summary: "Verified the live native seam", next: "Review the result", artifacts: [] });
  const operationId = "routine-checkpoint";
  const actor = { account: 7 };
  h.state.events = [{ sequence: 10, operation_id: operationId, admitted_at: 1, actor: { Module: "tasks" }, input: { event: { kind: "attribution", content: {
    attribution: { actor, source: { module: "tasks", kind: "job_event", object: createHash("sha256").update(operationId).digest("hex") } },
    source: { job_id: job.job_id, conversation_id: job.conversation_id, job_kind: job.kind, created_at_revision: job.created_at_revision,
      job_attempt: job.attempt, submitter: job.submitter, actor, height: 1,
      operation: { checkpoint: { job_id: job.job_id, operation_id: operationId, attempt: job.attempt, kind: "checkpoint", payload } } },
  } } } }];
  h.state.backend = "anthropic";
  h.state.modelTool = { name: "chief_board", arguments: { section: "overview", offset: 0, limit: 1 } };
  h.state.packages = [{ name: "observer", path: "fixture.ts" }, { name: "chief", path: "pinned-chief" }];
  h.state.env = { DUCKTAPE_RUN_AGENT: modules.config.agentId };
  h.state.node = modules.node;
  const seededRevision = modules.state.revision;
  h.state.hold = async (checkpoint) => {
    if (!checkpoint.delivery) return;
    const board = await modules.board();
    assert.equal(board.runs[0].progress?.sequence, 10, "canonical progress must commit BEFORE native delivery acknowledgement");
    assert.ok(board.revision > seededRevision);
  };
  const routine = await h.run("events/10/11", "never substitute this text for the committed event", "observe-lifecycle", "progress-run");
  success(routine);
  assert.equal(h.state.payloads.length, 0, "routine preparation must not call any model");
  assert.equal(h.state.effects.filter((effect) => effect.kind === "model").length, 0);
  assert.deepEqual(messages(h.state.checkpoints.at(-1)!.jsonl), [], "no fabricated assistant, user or usage");
  assert.equal(h.state.checkpoints.at(-1)!.delivery, true);
  assert.equal(h.state.checkpoints.at(-1)!.complete, true);
  assert.ok(records(routine.stdout).some((event) => event.type === "native_conversation_handled"));
  const handled = records(h.state.checkpoints.at(-1)!.jsonl).find((entry) => entry.customType === "ducktape.input_handled");
  assert.deepEqual(handled.data.events, [{ sequence: 10, operation_id: operationId }]);
  const canonicalBoard = await modules.board();
  const progress = canonicalBoard.runs[0].progress!;
  assert.equal(progress.summary, "Verified the live native seam");
  const root = modules.state.snapshots.get(progress.source!.hash)!;
  assert.equal(root.size, 1);
  assert.equal(root.get(`/shared/agents/chief/reports/${progress.source!.fileId}.txt`)!.toString(), payload);
  const actions = modules.state.http.filter((request) => request.path === "/v1/run-action").map((request) => request.body.message.agent_action);
  assert.equal(actions.length, 5, "raw report + history each commit/project, followed by atomic Pages CAS");
  assert.ok(actions.every((action) => action.run_id === "progress-run"));
  assert.deepEqual(actions.map((action) => action.action.target.module), ["files", "files", "files", "files", "pages"]);
  const receipt = [...modules.state.receipts.values()].find((receipt) => receipt.revision === canonicalBoard.revision)!;
  assert.ok(receipt.artifacts.includes(progress.source!.hash));
  const lifecycle = h.state.effects.find((effect) => effect.kind === "lifecycle")!.data;
  // The real preparation (proved by canonical writes + native handled receipt)
  // follows the authenticated bind, with no intervening agent/model start.
  assert.deepEqual(lifecycle.events, ["session-start", "network-bind"]);
  assert.deepEqual([...lifecycle.tools].sort(), [...CHIEF_TOOL_NAMES].sort());

  h.state.hold = undefined;
  for (const sequence of [11, 12]) {
    const member = { External: Array(32).fill(7) };
    h.state.events = [{ sequence, operation_id: `human-${sequence}`, admitted_at: 2, actor: member,
      input: { chat: { message: { head: { message_id: `message-${sequence}`, deleted: false, rev: 0, edited_at: null,
        origin: member, content_origin: member, author: { account: 7 }, blocks: [{ paragraph: [{ text: `Please review the worker, question ${sequence}.` }] }] } } } } }];
    success(await h.run(`events/${sequence}/${sequence + 1}`, "not the authenticated member input", "observe-lifecycle", `human-run-${sequence}`));
  }
  assert.equal(h.state.payloads.length, 3, "one real tool exchange then a second cold human turn");
  const [first, afterTool, restarted] = h.state.payloads.map((payload) => payload.body);
  assert.ok(first.system.map((part: any) => part.text).join("\n").includes(CHIEF_PROMPT));
  assert.deepEqual(afterTool.system, first.system);
  assert.deepEqual(restarted.system, first.system);
  for (const request of [first, afterTool, restarted]) {
    assert.deepEqual(request.tools.map((tool: any) => tool.name).sort(), [...CHIEF_TOOL_NAMES].sort());
    // Tool results are legitimate historical context; only automatic system
    // and member-prompt injection must exclude the full board.
    const userText = request.messages.filter((message: any) => message.role === "user")
      .flatMap((message: any) => message.content.filter((part: any) => part.type === "text"));
    assert.ok(!JSON.stringify(request.system).includes(boardSentinel));
    assert.ok(!JSON.stringify(userText).includes(boardSentinel));
    assert.ok(!JSON.stringify(request).includes("session_key"));
    assert.ok(!JSON.stringify(request).includes("not the authenticated member input"));
  }
  assert.match(JSON.stringify(first.messages), /Please review the worker, question 11/);
  assert.match(JSON.stringify(restarted.messages), /Please review the worker, question 12/);
  const native = messages(h.state.checkpoints.at(-1)!.jsonl);
  const result = native.find((message) => message.role === "toolResult" && message.toolName === "chief_board");
  assert.equal(result.isError, false);
  const overview = JSON.parse(result.content[0].text);
  assert.equal(overview.success, true);
  assert.equal(overview.data.tasks[0].id, "task1");
  assert.equal(overview.data.revision, canonicalBoard.revision);
  assert.match(overview.data.provenance, /bounded overview/);
  assert.equal(native.filter((message) => message.role === "user").length, 2);
  assert.equal(modules.state.revision, canonicalBoard.revision, "read-only human smoke must not invent policy writes");
  assert.equal(modules.state.http.filter((request) => request.path === "/v1/run-action").length, 5);
  const observations = h.state.effects.filter((effect) => effect.kind === "lifecycle");
  assert.equal(observations.length, 3);
  for (const observation of observations.slice(1)) {
    assert.deepEqual(observation.data.events, ["session-start", "network-bind", "before-agent"]);
    assert.deepEqual([...observation.data.tools].sort(), [...CHIEF_TOOL_NAMES].sort());
  }
});

for (const backend of ["anthropic", "openai-codex"] as const) test(`${backend} built-in HTTP SSE payload retains native assistant/tool ids and stable prefix across cold turns`, async (t) => {
  const h = await harness(t);
  h.state.backend = backend;
  h.state.packages = [];
  const firstResult = await h.run("turn-1");
  success(firstResult);
  assert.equal(h.state.payloads.length, 2, firstResult.stdout + firstResult.stderr);
  assert.ok(records(firstResult.stdout).some((event) => event.type === "message_end" && event.message.role === "assistant"));
  const firstHistory = records(h.state.checkpoints.at(-1)!.jsonl);
  success(await h.run("turn-2", "second input"));
  assert.equal(h.state.payloads.length, 3);
  assert.deepEqual(records(h.state.checkpoints.at(-1)!.jsonl).slice(0, firstHistory.length), firstHistory);
  const [first, afterTool, restarted] = h.state.payloads.map((payload) => payload.body);
  assert.equal(h.state.payloads[0].checkpoint?.delivery, true);
  const nativeCall = messages(h.state.checkpoints.at(-1)!.jsonl).find((message) => message.role === "assistant" && message.content.some((part: any) => part.type === "toolCall")).content.find((part: any) => part.type === "toolCall");
  switch (backend) {
    case "anthropic": {
      assert.deepEqual(restarted.system, first.system);
      // Anthropic moves the ephemeral cache boundary to the new final message;
      // compare native content without that request-local cache annotation.
      const content = (value: unknown) => JSON.parse(JSON.stringify(value, (key, item) => key === "cache_control" ? undefined : item));
      assert.deepEqual(content(restarted.messages.slice(0, afterTool.messages.length)), content(afterTool.messages));
      assert.equal(nativeCall.id, "toolu_fixture");
      assert.ok(JSON.stringify(restarted.messages).includes('"tool_use_id":"toolu_fixture"'));
      assert.ok(JSON.stringify(restarted.messages).includes("Native builtin read result."));
      break;
    }
    case "openai-codex": {
      assert.equal(restarted.instructions, first.instructions);
      assert.equal(restarted.prompt_cache_key, first.prompt_cache_key);
      assert.deepEqual(restarted.input.slice(0, afterTool.input.length), afterTool.input);
      assert.match(nativeCall.id, /call_fixture/);
      const call = restarted.input.find((item: any) => item.type === "function_call");
      const output = restarted.input.find((item: any) => item.type === "function_call_output");
      assert.equal(call.id, "fc_fixture");
      assert.equal(call.call_id, "call_fixture");
      assert.equal(output.call_id, call.call_id);
      assert.match(JSON.stringify(output), /Native builtin read result/);
      break;
    }
  }
});
