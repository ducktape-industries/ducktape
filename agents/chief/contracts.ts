// In-process policy adapters, not network wire formats. The host authenticates
// member/job events and maps these methods to generic Pages, Files, Jobs and Runs.
// Credentials never enter this module's serializable values.

// -- Durable domain ---------------------------------------------------------
export interface FileRef { fileId: string; hash: string }
export interface MemberSource { kind: 'chat' | 'page_comment'; memberId: string; conversationId: string; messageId: string }
export interface TaskSpec {
  id: string; key: string; title: string; brief: string; scope: string[];
  access: 'read' | 'write'; dependencies: string[];
}
export type TaskStatus = 'queued' | 'running' | 'review' | 'done' | 'blocked' | 'cancelled' | 'merged';
export interface Acceptance { revision: number; runId: string; outcome: string; evidence: FileRef[] }
export interface Task extends TaskSpec {
  status: TaskStatus; evidence: FileRef[]; currentRun?: string; conversationId?: string;
  reason?: string; mergedInto?: string; acceptance?: Acceptance;
}
export interface Progress { sequence: number; summary: string; next: string; artifacts: FileRef[]; blocker?: string; source?: FileRef }
export type RunStatus = 'reserved' | 'queued' | 'running' | 'completed' | 'failed' | 'cancelled' | 'interrupted';
export interface Run {
  id: string; taskId: string; operationId: string; status: RunStatus;
  jobId?: string; conversationId?: string; report?: FileRef; progress?: Progress;
  observedSequence: number; reason?: string;
}
export interface Rule { id: string; when: string; instruction: string }
export interface AskSpec {
  id: string; key: string; title: string; question: string; whyMember: string;
  ifUnasked: string; recommendation: string;
  options: { label: string; consequence: string }[]; artifacts: FileRef[];
  blocks: string[]; sources: { taskId: string; runId: string; conversationId: string }[];
  addressedTo: string[];
}
export interface Ask extends AskSpec {
  status: 'open' | 'replied' | 'answered' | 'superseded' | 'dismissed';
  reply?: { source: MemberSource; text: string; decisionId: string };
  resolution?: string;
}
export type OutboxPayload =
  | { kind: 'dispatch'; runId: string; taskId: string; conversation: { kind: 'fresh' } | { kind: 'continue'; conversationId: string }; prompt: string }
  | { kind: 'control'; runId: string; jobId: string; control: 'steer' | 'cancel'; text: string }
  | { kind: 'wake'; conversationId: string; event: 'result' | 'blocker' | 'decision'; entityId: string };
export interface OutboxEntry {
  operationId: string; payload: OutboxPayload; status: 'reserved' | 'attempted';
  attemptId?: string; effect?: EffectIdentity;
}
export interface OperationMetadata { operationId: string; fingerprint: string; history: FileRef; result?: Record<string, unknown> }
export interface OperationReceipt extends OperationMetadata { revision: number }
export interface Board {
  conversationId: string; revision: number; tasks: Task[]; runs: Run[]; asks: Ask[];
  rules: Rule[]; outbox: OutboxEntry[]; history: FileRef | null;
  concurrencyLimit: number | null;
  checkpoint: { focus: string; nextActions: string[] };
  checkinMinutes: number | null;
}

// -- Commands: caller chooses stable IDs once, before retrying ----------------
export interface Mutation { operationId: string; expectedRevision: number }
export type DomainAction =
  | { kind: 'task_put'; task: TaskSpec }
  | { kind: 'task_status'; taskId: string; status: 'queued' | 'blocked' | 'cancelled'; reason: string }
  | { kind: 'task_merge'; sourceId: string; targetId: string }
  | { kind: 'accept'; taskId: string; outcome: string; evidence: FileRef[] }
  | { kind: 'ask_open'; ask: AskSpec }
  | { kind: 'ask_resolve'; askId: string; status: 'answered' | 'superseded' | 'dismissed'; resolution: string }
  | { kind: 'rule_put'; rule: Rule }
  | { kind: 'rule_remove'; ruleId: string }
  | { kind: 'limit'; limit: number | null }
  | { kind: 'checkpoint'; focus: string; nextActions: string[] }
  | { kind: 'checkin'; minutes: number | null };
export type ChiefCommand =
  | { kind: 'board'; section: 'overview' | 'tasks' | 'runs' | 'asks' | 'rules' | 'outbox'; offset: number; limit: number; id?: string; detailOffset?: number; query?: string }
  | { kind: 'report'; runId: string; artifact?: FileRef; anchor?: ArtifactAnchor; offset?: number; limit?: number }
  | ({ kind: 'decision'; askId: string; messageId: string } & Mutation)
  | ({ kind: 'recover'; runId: string } & Mutation)
  | ({ kind: 'change'; action: DomainAction } & Mutation)
  | ({ kind: 'dispatch'; taskId: string; fresh: boolean } & Mutation)
  | ({ kind: 'control'; runId: string; control: 'steer' | 'cancel'; text: string } & Mutation)
  | { kind: 'reconcile'; operationId: string };
export type ChiefResult =
  | { success: true; data: Record<string, unknown> }
  | { success: false; error: string };
export interface ChiefControlNotice { operationId: string; revision: number; kind: string; entityId: string }
export interface ChiefBridge {
  execute(command: ChiefCommand, signal?: AbortSignal): Promise<ChiefResult>;
  subscribeControl(listener: (notice: ChiefControlNotice) => void): () => void;
}

// -- Authoritative storage ---------------------------------------------------
export interface ArtifactAnchor { operationId: string; history: FileRef }
export interface RecordSnapshot { revision: number; value: unknown }
export type CommitReceipt =
  | { kind: 'committed'; requestId: string; revision: number }
  | { kind: 'conflict'; revision: number };
export interface PagesAdapter {
  read(signal?: AbortSignal): Promise<RecordSnapshot>;
  receipt(operationId: string, signal?: AbortSignal): Promise<OperationReceipt | null>;
  // Resolves only after the managed-record commit is authoritative. A transport
  // admission/tx hash is NOT a committed receipt. Duplicate IDs bind exact data.
  commit(input: { expectedRevision: number; requestId: string; value: Board; metadata: OperationMetadata; artifacts: FileRef[] }, signal?: AbortSignal): Promise<CommitReceipt>;
}
export interface FilesAdapter {
  project(ref: FileRef, signal?: AbortSignal): Promise<FileRef>;
  // Raw result/history is immutable network Files data. No local path strings.
  put(input: { operationId: string; content: string }, signal?: AbortSignal): Promise<FileRef>;
  read(ref: FileRef, signal?: AbortSignal): Promise<string>;
}
export type EffectReceipt =
  | { kind: 'dispatch'; operationId: string; jobId: string; conversationId: string }
  | { kind: 'control'; operationId: string; jobId: string; status: 'applied' | 'rejected' }
  | { kind: 'wake'; operationId: string; inboxId: string }
  | { kind: 'rejected'; operationId: string; requestId: string; receiptId: string; reason: 'network_action_rejected' | 'network_action_refused' | 'network_rejection_unrepresentable' };
export interface EffectIdentity {
  receiptId: string; requestId: string; runId: string; target: 'tasks' | 'runs'; payloadFingerprint: string;
}
export interface PreparedEffect {
  identity: EffectIdentity;
  submit(signal?: AbortSignal): Promise<EffectReceipt>;
}
export interface EffectsAdapter {
  // Preparation only reads and freezes exact native intent. The executor CASes
  // its receipt locator/fingerprint before the distinct winner may submit.
  prepare(operationId: string, payload: OutboxPayload, signal?: AbortSignal): Promise<PreparedEffect>;
  // Lookup survives later native turns, including known rejection whose policy
  // acknowledgement failed. Control applied means worker acknowledged, not queued.
  lookup(operationId: string, signal?: AbortSignal): Promise<EffectReceipt | null>;
}
export interface ChiefAdapters { pages: PagesAdapter; files: FilesAdapter; effects: EffectsAdapter }

// -- Authenticated provider inputs, never model-callable ----------------------
export type ChiefInput = (
  | { kind: 'progress'; operationId: string; runId: string; jobId: string; progress: Progress; raw: string }
  | { kind: 'report_claim'; operationId: string; runId: string; jobId: string; report: string }
  | { kind: 'result'; operationId: string; runId: string; jobId: string; status: 'completed' | 'failed' | 'cancelled'; report: string }
  | { kind: 'decision'; operationId: string; askId: string; source: MemberSource; text: string }
  | { kind: 'unavailable'; operationId: string; runId: string; jobId: string }
) & { inboxOperationId?: string };
export interface ChiefService extends ChiefBridge { receive(input: ChiefInput, signal?: AbortSignal): Promise<ChiefResult> }
