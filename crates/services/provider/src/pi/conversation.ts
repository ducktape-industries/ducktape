// Durable native Pi transport, bootstrapped as an explicit CLI command so Pi's
// own loader resolves the installed SDK in both Node and standalone installs.
// The outer session never prompts a model. Only native SessionManager entries
// cross the checkpoint boundary; config, credentials and system instructions do
// not. Agent subscribers (unlike extension hooks) are awaited and fail closed.

import { randomUUID } from "node:crypto";
import { Writable } from "node:stream";
import { open, readFile, realpath, rename } from "node:fs/promises";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import {
  createAgentSession, createEventBus, CURRENT_SESSION_VERSION, DefaultResourceLoader,
  ModelRuntime, SessionManager, SettingsManager,
} from "@earendil-works/pi-coding-agent";
import type { AgentSession, EventBus, ExtensionAPI, ExtensionCommandContext, FileEntry, ToolDefinition } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
type AgentEvent = Parameters<Parameters<AgentSession["agent"]["subscribe"]>[0]>[0];
type AgentMessage = AgentSession["messages"][number];
type ExecutionResult =
  | { kind: "executed" }
  | { kind: "handled" }
  | { kind: "restored"; message: Extract<AgentMessage, { role: "assistant" }> };

// --- Transport contract ----------------------------------------------------

interface Manifest {
  conversation_id: string;
  turn_id: string;
  revision: number;
  session_path: string;
  system_prompt: string;
  job_reporting: boolean;
  packages: Array<{ name: string; path: string }>;
  events: Array<{ sequence: number; operation_id: string; actor: unknown; input: unknown; admitted_at: number }>;
}

type Preparation = { wake: false } | { wake: true; prompt?: string };

interface ControlMessage { id: string; text: string; kind: "steer" | "follow_up" }
interface Control { control: "continue" | "pause" | "abort"; messages: ControlMessage[]; cancel?: { id: string } }
interface Checkpoint {
  kind: "checkpoint";
  jsonl: string;
  delivery: boolean;
  complete: boolean;
  delivered_control_ids: string[];
}
type ReportKind = "checkpoint" | "report";
interface JobReportInput { operation_id: string; kind: ReportKind; payload: string }
type ReportWorker = "system" | { account: number } | { key: number[] } | { module: string };
interface JobReport extends JobReportInput { worker: ReportWorker; attempt: number; height: number }
interface JobReportResult { report: JobReport }
interface Transport {
  checkpoint: (request: Checkpoint) => Promise<{ revision: number }>;
  control: () => Promise<Control>;
  report: (input: JobReportInput, signal?: AbortSignal) => Promise<JobReportResult>;
}
interface TurnReceipt { conversation_id: string; turn_id: string; message_id: string }
interface RestoredHistory { manager: SessionManager; committedJsonl?: string }
type ControlledMessage = AgentMessage & { ducktape_control_id?: string };

const object = (value: unknown): Record<string, unknown> => {
  const valid = value !== null && typeof value === "object" && !Array.isArray(value);
  if (!valid) throw new Error("native_invalid_object");
  return value as Record<string, unknown>;
};
const string = (value: unknown): string => {
  if (typeof value !== "string" || value.length === 0) throw new Error("native_invalid_string");
  return value;
};
const revision = (value: unknown): number => {
  if (!Number.isSafeInteger(value) || (value as number) < 0) throw new Error("native_invalid_revision");
  return value as number;
};
const manifestFrom = (value: unknown): Manifest => {
  const data = object(value);
  if (!Array.isArray(data.packages)) throw new Error("native_invalid_packages");
  if (!Array.isArray(data.events)) throw new Error("native_invalid_events");
  if (typeof data.job_reporting !== "boolean") throw new Error("native_invalid_job_reporting");
  return {
    conversation_id: string(data.conversation_id), turn_id: string(data.turn_id),
    revision: revision(data.revision), session_path: string(data.session_path),
    system_prompt: string(data.system_prompt), job_reporting: data.job_reporting,
    events: data.events.map((value) => {
      const event = object(value);
      return { sequence: revision(event.sequence), operation_id: string(event.operation_id),
        actor: event.actor, input: event.input, admitted_at: revision(event.admitted_at) };
    }),
    packages: data.packages.map((item) => {
      const pin = object(item);
      return { name: string(pin.name), path: string(pin.path) };
    }),
  };
};
const controlFrom = (value: unknown): Control => {
  const data = object(value);
  const validControl = data.control === "continue" || data.control === "pause" || data.control === "abort";
  if (!validControl || !Array.isArray(data.messages)) throw new Error("native_invalid_control");
  return {
    control: data.control as Control["control"],
    ...(data.cancel === undefined ? {} : { cancel: { id: string(object(data.cancel).id) } }),
    messages: data.messages.map((item) => {
      const message = object(item);
      const validKind = message.kind === "steer" || message.kind === "follow_up";
      if (!validKind) throw new Error("native_invalid_control_kind");
      return { id: string(message.id), text: string(message.text), kind: message.kind as ControlMessage["kind"] };
    }),
  };
};

// --- Private files and credentials -----------------------------------------

const required = (env: NodeJS.ProcessEnv, name: string): string => string(env[name]);
const within = (root: string, path: string): boolean => {
  const suffix = relative(root, path);
  return suffix !== ".." && !suffix.startsWith(`..${sep}`) && !isAbsolute(suffix);
};
const workspacePath = (cwd: string, path: string): Promise<string> => Promise.resolve()
  .then(() => {
    const target = resolve(cwd, path);
    if (isAbsolute(path) || !within(cwd, target) || target === cwd) throw new Error("native_path_escape");
    return realpath(dirname(target)).then((parent) => {
      if (!within(cwd, parent)) throw new Error("native_path_escape");
      return realpath(target).catch((error: NodeJS.ErrnoException) => {
        if (error.code !== "ENOENT") throw error;
        return target;
      });
    });
  })
  .then((target) => {
    if (!within(cwd, target)) throw new Error("native_path_escape");
    return target;
  });

const writeSnapshot = (path: string, jsonl: string): Promise<void> => {
  const temporary = `${path}.${randomUUID()}.tmp`;
  return Promise.resolve()
    .then(() => open(temporary, "wx", 0o600))
    .then((file) => file.writeFile(jsonl, "utf8").then(() => file.sync()).finally(() => file.close()))
    .then(() => rename(temporary, path))
    .then(() => open(dirname(path), "r"))
    .then((directory) => directory.sync().finally(() => directory.close()));
};

// Reject rather than redact: opaque thinking signatures and tool payloads must
// survive byte-for-byte. This gate covers env/config reads through builtin tools.
const secretValues = (env: NodeJS.ProcessEnv, configs: unknown[]): string[] => {
  const collect = (value: unknown, key = ""): string[] => {
    if (typeof value === "string") {
      const secretKey = /(^key$)|token|secret|password|api.?key|authorization|cookie|access|refresh/i.test(key);
      return secretKey && value.length > 0 ? [value, value.replace(/^Bearer\s+/i, "")] : [];
    }
    if (value === null || typeof value !== "object") return [];
    return Object.entries(value).flatMap(([name, entry]) => collect(entry, name));
  };
  return [...new Set([collect(env), ...configs.map((value) => collect(value))].flat())];
};
const assertNoSecrets = (jsonl: string, secrets: string[]): void => {
  const exposed = secrets.some((secret) => jsonl.includes(secret) || jsonl.includes(JSON.stringify(secret).slice(1, -1)));
  if (exposed) throw new Error("native_history_contains_secret");
};
const readConfig = (path: string): Promise<unknown> => Promise.resolve()
  .then(() => readFile(path, "utf8"))
  .then((text) => JSON.parse(text) as unknown)
  .catch((error: NodeJS.ErrnoException) => {
    if (error.code === "ENOENT") return {};
    throw new Error("native_invalid_config");
  });

const hostTransport = (url: string, token: string): Transport => {
  const endpoint = new URL("/v1/native-conversation", url);
  const local = endpoint.protocol === "http:" && ["127.0.0.1", "localhost", "[::1]"].includes(endpoint.hostname);
  if (!local || endpoint.username || endpoint.password || endpoint.search || endpoint.hash) throw new Error("native_invalid_host_route");
  // Every checkpoint sends complete JSONL, not a delta. The host limits the
  // encoded JSON request body to 64 MiB, including escaping and receipt fields.
  // Native compaction retains old entries, so it does not reclaim this budget.
  // A refusal preserves the accepted head and fails closed; never truncate it.
  const post = (body: unknown, signal?: AbortSignal): Promise<unknown> => Promise.resolve()
    .then(() => {
      const timeout = AbortSignal.timeout(60_000);
      const cancellation = signal ? AbortSignal.any([signal, timeout]) : timeout;
      return fetch(endpoint, {
        method: "POST", redirect: "error", headers: { "content-type": "application/json", "x-ducktape-run-action": token },
        body: JSON.stringify(body), signal: cancellation,
      });
    })
    .then((response) => {
      const exceedsRequestLimit = response.status === 413;
      if (exceedsRequestLimit) throw new Error("native_history_request_too_large");
      if (!response.ok) throw new Error("native_host_refused");
      return response.json();
    });
  return {
    checkpoint: (body) => post(body).then((value) => ({ revision: revision(object(value).revision) })),
    control: () => post({ kind: "control" }).then(controlFrom),
    report: (input, signal) => post({ kind: "report", operation_id: input.operation_id,
      report_kind: input.kind, payload: input.payload }, signal).then((value) => jobReportFrom(value, input)),
  };
};

// --- Semantic worker reports ----------------------------------------------

const jobReportParameters = Type.Object({
  operation_id: Type.String({ minLength: 1, maxLength: 256, description: "Nonempty ID, at most 256 UTF-8 bytes. Reuse only for an exact retry of the same report." }),
  kind: Type.Union([Type.Literal("checkpoint"), Type.Literal("report")]),
  payload: Type.String({ minLength: 1, maxLength: 4096, description: "Opaque nonblank text or JSON text, at most 4096 UTF-8 bytes. Preserved exactly, never interpreted as a policy schema." }),
}, { additionalProperties: false });
const jobReportInput = (value: unknown): JobReportInput => {
  const input = object(value);
  const validId = typeof input.operation_id === "string" && input.operation_id.length > 0
    && Buffer.byteLength(input.operation_id, "utf8") <= 256 && !input.operation_id.includes("\u001f");
  const validKind = input.kind === "checkpoint" || input.kind === "report";
  const validPayload = typeof input.payload === "string" && input.payload.trim().length > 0 && Buffer.byteLength(input.payload, "utf8") <= 4096;
  if (Object.keys(input).length !== 3 || !validId || !validKind || !validPayload) throw new Error("native_invalid_job_report_input");
  return input as unknown as JobReportInput;
};
const validReportWorker = (value: unknown): value is ReportWorker => {
  if (value === "system") return true;
  const worker = object(value);
  const fields = Object.keys(worker);
  if (fields.length !== 1) return false;
  switch (fields[0]) {
    case "account": return Number.isSafeInteger(worker.account) && (worker.account as number) >= 0;
    case "key": return Array.isArray(worker.key) && worker.key.length > 0
      && worker.key.every((byte) => Number.isInteger(byte) && byte >= 0 && byte <= 255);
    case "module": return typeof worker.module === "string" && worker.module.length > 0;
    default: return false;
  }
};
const jobReportFrom = (value: unknown, input: JobReportInput): JobReportResult => {
  const result = object(value);
  const report = object(result.report);
  const exactFields = Object.keys(result).length === 1 && Object.keys(report).length === 6
    && ["operation_id", "worker", "attempt", "height", "kind", "payload"].every((field) => Object.hasOwn(report, field));
  const exactInput = report.operation_id === input.operation_id && report.kind === input.kind && report.payload === input.payload;
  const validAttempt = Number.isSafeInteger(report.attempt) && (report.attempt as number) >= 0;
  const validHeight = Number.isSafeInteger(report.height) && (report.height as number) >= 0;
  if (!exactFields || !exactInput || !validAttempt || !validHeight || !validReportWorker(report.worker)) throw new Error("native_invalid_job_report_response");
  return result as unknown as JobReportResult;
};
const jobReportTool = (host: Transport): ToolDefinition => ({
  name: "ducktape_report_job", label: "Report job progress",
  description: "Publish semantic progress or report text to the current worker Job. The host derives all execution identity and returns the immutable committed Tasks report. This is not a native conversation-history checkpoint. Reuse an operation ID only for an exact retry.",
  parameters: jobReportParameters,
  execute: (_id, input, signal) => Promise.resolve()
    .then(() => { signal?.throwIfAborted(); return jobReportInput(input); })
    .then((input) => host.report(input, signal))
    .then((result) => ({ content: [{ type: "text", text: JSON.stringify(result) }], details: result })),
});

// --- Native history and recovery -------------------------------------------

const serialize = (manager: SessionManager): string =>
  [manager.getHeader(), ...manager.getEntries()].map((entry) => JSON.stringify(entry)).join("\n") + "\n";
const restore = (cwd: string, path: string, expectedRevision: number): Promise<RestoredHistory> => Promise.resolve()
  .then(() => readFile(path, "utf8"))
  .catch((error: NodeJS.ErrnoException) => {
    if (error.code === "ENOENT" && expectedRevision === 0) return "";
    throw new Error("native_history_missing");
  })
  .then((jsonl) => {
    if (jsonl === "" && expectedRevision === 0) return { manager: SessionManager.inMemory(cwd) };
    if (!jsonl.endsWith("\n")) throw new Error("native_history_incomplete");
    const entries = jsonl.trimEnd().split("\n").map((line) => object(JSON.parse(line)));
    const header = entries[0];
    const validHeader = header?.type === "session" && header.version === CURRENT_SESSION_VERSION && typeof header.id === "string";
    if (!validHeader) throw new Error("native_history_invalid_header");
    const ids = new Set<string>();
    entries.slice(1).forEach((entry) => {
      const id = string(entry.id);
      const validParent = entry.parentId === null || (typeof entry.parentId === "string" && ids.has(entry.parentId));
      if (ids.has(id) || !validParent) throw new Error("native_history_invalid_tree");
      ids.add(id);
    });
    return {
      manager: SessionManager.inMemory(cwd, undefined, entries as unknown as FileEntry[]),
      // The checkpoint baseline is the host's bytes, captured before SDK
      // initialization can append metadata. A new entry must advance revision;
      // an unchanged same-attempt retry must not require another publication.
      committedJsonl: expectedRevision > 0 ? jsonl : undefined,
    };
  });
const receipts = (manager: SessionManager): TurnReceipt[] => manager.getEntries().flatMap((entry) =>
  entry.type === "custom" && entry.customType === "ducktape.turn_delivery" ? [entry.data as TurnReceipt] : []);
const handled = (manager: SessionManager, manifest: Manifest): boolean => manager.getEntries().some((entry) =>
  entry.type === "custom" && entry.customType === "ducktape.input_handled"
  && object(entry.data).conversation_id === manifest.conversation_id && object(entry.data).turn_id === manifest.turn_id);
const inputDelivered = (manager: SessionManager, manifest: Manifest): boolean =>
  handled(manager, manifest) || receipts(manager).some((item) => item.turn_id === manifest.turn_id);
const cancelId = (manager: SessionManager, manifest: Manifest): string | undefined => {
  const receipt = manager.getEntries().findLast((entry) => entry.type === "custom"
    && entry.customType === "ducktape.cancel_delivery"
    && object(entry.data).conversation_id === manifest.conversation_id && object(entry.data).turn_id === manifest.turn_id);
  return receipt?.type === "custom" ? string(object(receipt.data).id) : undefined;
};
const controlIds = (manager: SessionManager): string[] => manager.getEntries().flatMap((entry) => {
  const delivered = entry.type === "custom"
    && (entry.customType === "ducktape.control_delivery" || entry.customType === "ducktape.cancel_delivery");
  return delivered ? [string(object(entry.data).id)] : [];
});

// --- Awaited execution boundary -------------------------------------------

const attachDurability = (session: AgentSession, manifest: Manifest, path: string, host: Transport, secrets: string[], emit: (event: unknown) => Promise<void>, restoredSnapshot?: string) => {
  const manager = session.sessionManager;
  // Mutable state owns one serialized I/O boundary, never independently running
  // writes. A terminal latch also stops Pi's generated error turn after failure
  // or committed cancellation, without persisting a fabricated assistant.
  type Termination = { kind: "active" } | { kind: "failed" | "cancelled"; error: Error };
  const state: { termination: Termination; revision: number; snapshot?: string; leaf?: string | null } = {
    termination: { kind: "active" }, revision: manifest.revision, snapshot: restoredSnapshot,
  };
  const queued = new Set(controlIds(manager));
  const hasDelivery = () => inputDelivered(manager, manifest);
  const check = () => {
    switch (state.termination.kind) {
      case "active": return;
      case "failed": case "cancelled": throw state.termination.error;
    }
  };
  const fail = (error: unknown): never => {
    if (state.termination.kind !== "active") throw state.termination.error;
    const failure = error instanceof Error ? error : new Error("native_boundary_failed");
    state.termination = { kind: "failed", error: failure };
    throw failure;
  };
  const checkpoint = (complete = false): Promise<void> => Promise.resolve()
    .then(() => {
      check();
      const leaf = manager.getLeafId();
      // Token deltas do not append entries. Avoid serializing the entire native
      // tree per token; every native append advances the leaf to a fresh id.
      if (leaf === state.leaf && !complete) return;
      const jsonl = serialize(manager);
      assertNoSecrets(jsonl, secrets);
      if (jsonl === state.snapshot && !complete) { state.leaf = leaf; return; }
      return writeSnapshot(path, jsonl)
        .then(() => host.checkpoint({ kind: "checkpoint", jsonl, delivery: hasDelivery(), complete, delivered_control_ids: controlIds(manager) }))
        .then((result) => {
          const unchangedRetry = jsonl === state.snapshot && result.revision === state.revision;
          const advanced = result.revision > state.revision;
          if (!advanced && !unchangedRetry) throw new Error("native_checkpoint_revision_not_advanced");
          state.revision = result.revision;
          state.snapshot = jsonl;
          state.leaf = leaf;
        });
    }).catch(fail);
  const settleCancellation = (id: string): Promise<never> => Promise.resolve()
    .then(check)
    .then(() => {
      const prior = cancelId(manager, manifest);
      if (prior !== undefined && prior !== id) throw new Error("native_cancel_identity_changed");
      if (prior === undefined) manager.appendCustomEntry("ducktape.cancel_delivery", {
        conversation_id: manifest.conversation_id, turn_id: manifest.turn_id, id,
      });
      // The host response certifies all three committed operations: native
      // checkpoint, current-job control ACK, and cancellation settlement.
      return checkpoint(true);
    })
    .then(() => {
      const error = new Error("native_conversation_cancelled");
      state.termination = { kind: "cancelled", error };
      throw error;
    }).catch(fail);
  const wasCancelled = (error: unknown): boolean =>
    state.termination.kind === "cancelled" && state.termination.error === error;
  const controls = (): Promise<void> => Promise.resolve()
    .then(check)
    .then(() => host.control())
    .then((control) => {
      if (control.cancel) return settleCancellation(control.cancel.id);
      switch (control.control) {
        case "pause": throw new Error("native_conversation_paused");
        case "abort": throw new Error("native_conversation_aborted");
        case "continue": return control.messages.forEach((message) => {
          if (queued.has(message.id)) return;
          queued.add(message.id);
          const native: ControlledMessage = {
            role: "user", content: [{ type: "text", text: message.text }],
            timestamp: Date.now(), ducktape_control_id: message.id,
          };
          switch (message.kind) {
            case "steer": return session.agent.steer(native);
            case "follow_up": return session.agent.followUp(native);
          }
        });
      }
    }).catch(fail);
  const onMessage = (event: Extract<AgentEvent, { type: "message_end" }>): void => {
    const message = event.message as ControlledMessage;
    if (message.role !== "user") return;
    const message_id = manager.getLeafId();
    if (message.ducktape_control_id) {
      manager.appendCustomEntry("ducktape.control_delivery", { id: message.ducktape_control_id, message_id });
      return;
    }
    if (!hasDelivery()) manager.appendCustomEntry("ducktape.turn_delivery", {
      conversation_id: manifest.conversation_id, turn_id: manifest.turn_id, message_id,
    });
  };
  const unsubscribe = session.agent.subscribe((event) => Promise.resolve()
    .then(check)
    .then(() => { if (event.type === "message_end") onMessage(event); })
    .then(() => checkpoint())
    .then(() => {
      const boundary = event.type === "tool_execution_start" || event.type === "turn_start" || event.type === "turn_end";
      return boundary ? controls() : undefined;
    })
    .then(() => {
      assertNoSecrets(JSON.stringify(event), secrets);
      return emit(event);
    }).catch(fail));
  const stream = session.agent.streamFunction;
  session.agent.streamFunction = (...args) => Promise.resolve()
    .then(() => checkpoint()).then(controls).then(() => stream(...args));
  session.agent.toolExecution = "sequential";
  return { checkpoint, controls, check, unsubscribe, hasDelivery, wasCancelled };
};

const repairInterruptedTools = (session: AgentSession, checkpoint: () => Promise<void>): Promise<void> => {
  const messages = session.sessionManager.buildSessionContext().messages;
  const assistantIndex = messages.findLastIndex((message) => message.role === "assistant");
  const assistant = messages[assistantIndex];
  if (assistant?.role !== "assistant") return Promise.resolve();
  const completed = new Set(messages.slice(assistantIndex + 1).flatMap((message) => message.role === "toolResult" ? [message.toolCallId] : []));
  const pending = assistant.content.filter((part) => part.type === "toolCall" && !completed.has(part.id));
  return pending.reduce((previous, call) => previous.then(() => {
    if (call.type !== "toolCall") return;
    session.sessionManager.appendMessage({
      role: "toolResult", toolCallId: call.id, toolName: call.name,
      content: [{ type: "text", text: "interrupted_unknown_outcome: this call may have taken effect; it was not replayed. Reconcile before retrying." }],
      isError: true, timestamp: Date.now(), details: { reason: "interrupted_unknown_outcome" },
    });
    session.agent.state.messages = session.sessionManager.buildSessionContext().messages;
    return checkpoint();
  }), Promise.resolve());
};

// --- Authenticated resident input ------------------------------------------

const residentPreparation = () => {
  const base = createEventBus();
  const handlers = new Set<(data: unknown) => void>();
  // Pi's normal event bus swallows listener failures. The preparation boundary
  // must instead know whether exactly one synchronous acceptance occurred.
  const eventBus: EventBus = {
    emit: base.emit,
    on: (channel, handler) => {
      if (channel !== "ducktape:resident:prepare") return base.on(channel, handler);
      handlers.add(handler);
      return () => { handlers.delete(handler); };
    },
  };
  const prepare = (manifest: Manifest): Promise<Preparation> => Promise.resolve()
    .then(() => {
      if (handlers.size === 0) return { wake: true };
      if (handlers.size !== 1) throw new Error("native_multiple_preparation_handlers");
      const accepted: Promise<unknown>[] = [];
      const accept = (value: unknown) => {
        const promise = Promise.resolve(value);
        promise.catch(() => {});
        accepted.push(promise);
      };
      handlers.forEach((handler) => {
        const returned = handler({ conversation_id: manifest.conversation_id,
          turn_id: manifest.turn_id, events: structuredClone(manifest.events), accept });
        if (returned !== undefined) {
          Promise.resolve(returned).catch(() => {});
          throw new Error("native_preparation_handler_must_accept_synchronously");
        }
      });
      if (accepted.length !== 1) throw new Error("native_invalid_preparation_acceptance");
      return accepted[0];
    })
    .then((value) => {
      const directive = object(value);
      switch (directive.wake) {
        case false: return { wake: false };
        case true: return directive.prompt === undefined ? { wake: true } : { wake: true, prompt: string(directive.prompt) };
        default: throw new Error("native_invalid_preparation_directive");
      }
    });
  return { eventBus, prepare };
};

// --- Native turn execution -------------------------------------------------

const executePrepared = (session: AgentSession, manifest: Manifest, prompt: string,
  durable: ReturnType<typeof attachDurability>, directive: Preparation): Promise<ExecutionResult> => {
  const manager = session.sessionManager;
  if (!directive.wake) return Promise.resolve().then(() => {
    if (manifest.events.length === 0) throw new Error("native_handled_requires_events");
    if (!handled(manager, manifest)) manager.appendCustomEntry("ducktape.input_handled", {
      conversation_id: manifest.conversation_id, turn_id: manifest.turn_id,
      events: manifest.events.map(({ sequence, operation_id }) => ({ sequence, operation_id })),
    });
    return { kind: "handled" };
  });
  const continueNative = (): Promise<ExecutionResult> => Promise.resolve()
    .then(() => session.agent.continue()).then(() => ({ kind: "executed" }));
  return Promise.resolve()
    .then(() => repairInterruptedTools(session, durable.checkpoint))
    .then<ExecutionResult>(() => {
      if (!durable.hasDelivery()) return session.prompt(directive.prompt ?? prompt, { expandPromptTemplates: false })
        .then(() => ({ kind: "executed" as const }));
      const last = session.messages.at(-1);
      if (last?.role !== "assistant") return continueNative();
      const failed = last.stopReason === "error" || last.stopReason === "aborted";
      if (failed) {
        // Pi's provider conversion also omits failed assistant messages. Remove
        // only the failure suffix from LIVE context so Agent.continue accepts
        // it; the native tree keeps every entry and the original user receipt.
        const lastSuccessful = session.messages.findLastIndex((message) =>
          message.role !== "assistant" || (message.stopReason !== "error" && message.stopReason !== "aborted"));
        session.agent.state.messages = session.messages.slice(0, lastSuccessful + 1);
        return continueNative();
      }
      if (session.agent.hasQueuedMessages()) return continueNative();
      return { kind: "restored", message: last };
    });
};

const finishExecution = (session: AgentSession, durable: ReturnType<typeof attachDurability>, result: ExecutionResult): Promise<void> => Promise.resolve()
  .then(durable.check)
  .then(() => {
    const last = session.messages.at(-1);
    const modelFailed = last?.role === "assistant" && (last.stopReason === "error" || last.stopReason === "aborted");
    if (modelFailed && result.kind !== "handled") throw new Error("native_model_failed");
    if (!durable.hasDelivery()) throw new Error("native_input_not_delivered");
    return durable.checkpoint(true);
  })
  .then(() => {
    switch (result.kind) {
      case "handled": return emitEvent({ type: "native_conversation_handled" });
      case "restored": return emitEvent({ type: "message_end", message: result.message, restored: true });
      case "executed": return;
    }
  });

// Reconcile before SDK construction: an input-less native session otherwise
// gets new model/thinking metadata on every open, changing an idempotent final
// snapshot after the host may already have committed cancellation settlement.
const reconcileCancelledSnapshot = (manager: SessionManager, manifest: Manifest,
  path: string, host: Transport, secrets: string[]): Promise<void> => Promise.resolve()
  .then(() => {
    const jsonl = serialize(manager);
    assertNoSecrets(jsonl, secrets);
    return writeSnapshot(path, jsonl).then(() => host.checkpoint({
      kind: "checkpoint", jsonl, delivery: inputDelivered(manager, manifest),
      complete: true, delivered_control_ids: controlIds(manager),
    }));
  })
  .then((result) => {
    if (result.revision < manifest.revision) throw new Error("native_checkpoint_revision_regressed");
    return emitEvent({ type: "native_conversation_cancelled" });
  });

// --- Isolated SDK lifetime --------------------------------------------------

const run = (manifest: Manifest, prompt: string, ctx: ExtensionCommandContext, env: NodeJS.ProcessEnv): Promise<void> => {
  const cwd = resolve(ctx.cwd);
  const agentDir = required(env, "PI_CODING_AGENT_DIR");
  const host = hostTransport(required(env, "DUCKTAPE_RUN_ACTION_URL"), required(env, "DUCKTAPE_RUN_ACTION_TOKEN"));
  // Airlock exposes the HTTP SSE lane, not a provider WebSocket endpoint.
  const settingsManager = SettingsManager.inMemory({ transport: "sse", retry: { enabled: false }, packages: [] });
  const emit = emitEvent;
  return Promise.resolve()
    .then(() => Promise.all([
      workspacePath(cwd, manifest.session_path),
      Promise.all(manifest.packages.map((pin) => workspacePath(cwd, pin.path))),
      Promise.all([readConfig(join(agentDir, "auth.json")), readConfig(join(agentDir, "models.json"))]),
    ]))
    .then(([path, packages, configs]) => {
      const secrets = secretValues(env, configs);
      return restore(cwd, path, manifest.revision).then(({ manager, committedJsonl }) => {
        assertNoSecrets(serialize(manager), secrets);
        if (cancelId(manager, manifest) !== undefined) return reconcileCancelledSnapshot(manager, manifest, path, host, secrets);
        const preparation = residentPreparation();
        const resourceLoader = new DefaultResourceLoader({
          cwd, agentDir, settingsManager, eventBus: preparation.eventBus, noExtensions: true, noSkills: true,
          noPromptTemplates: true, noThemes: true, noContextFiles: true,
          additionalExtensionPaths: [join(agentDir, "ducktape.ts"), ...packages],
          systemPrompt: "", systemPromptOverride: () => manifest.system_prompt, appendSystemPrompt: [],
        });
        return Promise.all([
          resourceLoader.reload(),
          ModelRuntime.create({ authPath: join(agentDir, "auth.json"), modelsPath: join(agentDir, "models.json"), modelsStorePath: join(agentDir, "models-cache.json"), allowModelNetwork: false }),
        ]).then(([, modelRuntime]) => {
          const wrongConversation = receipts(manager).some((receipt) => receipt.conversation_id !== manifest.conversation_id);
          if (wrongConversation) throw new Error("native_conversation_mismatch");
          if (resourceLoader.getExtensions().errors.length > 0) throw new Error("native_package_load_failed");
          return createAgentSession({ cwd, agentDir, model: ctx.model, thinkingLevel: ctx.thinkingLevel, modelRuntime, sessionManager: manager, resourceLoader, settingsManager,
            customTools: manifest.job_reporting ? [jobReportTool(host)] : [] })
            .then(({ session }) => {
              const durable = attachDurability(session, manifest, path, host, secrets, emit, committedJsonl);
              return Promise.resolve()
                .then(() => session.bindExtensions({ mode: "json" }))
                .then(() => durable.checkpoint())
                .then(durable.controls)
                .then<Preparation>(() => {
                  if (handled(manager, manifest)) return { wake: false };
                  return durable.hasDelivery() ? { wake: true } : preparation.prepare(manifest);
                })
                .then((directive) => executePrepared(session, manifest, prompt, durable, directive))
                .then((result) => finishExecution(session, durable, result))
                .finally(() => session.extensionRunner.emit({ type: "session_shutdown", reason: "quit" }))
                .catch((error: unknown) => {
                  if (!durable.wasCancelled(error)) throw error;
                  return emitEvent({ type: "native_conversation_cancelled" });
                })
                .finally(() => { durable.unsubscribe(); session.dispose(); });
            });
        });
      });
    });
};

// --- CLI bootstrap ---------------------------------------------------------

// Pi replaces stdout.write with a stderr redirect. Invoke the native Writable
// method on the ONE persistent stdout stream instead. Its libuv pipe writer
// queues concurrent SDK onUpdate frames and waits for writable readiness;
// fd-based fs.WriteStreams both interleave and exhaust retries under EAGAIN.
// Every write callback rejects into the existing durability failure latch.
const output = process.stdout;
output.on("error", () => {});
const emitEvent = (event: unknown): Promise<void> => new Promise((resolve, reject) => {
  Writable.prototype.write.call(output, `${JSON.stringify(event)}\n`, "utf8", (error?: Error | null) => error ? reject(error) : resolve());
});

const conversationExtension = (pi: ExtensionAPI): void => {
  const env = { ...process.env };
  pi.registerCommand("ducktape-conversation", {
    description: "Run a fenced native conversation turn",
    handler: (encoded, ctx) => Promise.resolve()
      .then(() => {
        const prompt = Buffer.from(encoded.trim(), "base64");
        if (prompt.toString("base64") !== encoded.trim()) throw new Error("native_invalid_prompt_encoding");
        return readFile(join(required(env, "PI_CODING_AGENT_DIR"), "conversation.json"), "utf8")
          .then((text) => run(manifestFrom(JSON.parse(text)), prompt.toString("utf8"), ctx, env));
      })
      .catch((error: unknown) => {
        // Command dispatch catches errors; an explicit exit status is therefore
        // required. Never print raw errors: they can contain credential material.
        const exceedsRequestLimit = error instanceof Error && error.message === "native_history_request_too_large";
        const reason = exceedsRequestLimit ? "native_history_request_too_large" : "native_conversation_failed";
        process.exitCode = 1;
        return emitEvent({ type: "native_conversation_error", reason });
      }),
  });
};

export default conversationExtension;
