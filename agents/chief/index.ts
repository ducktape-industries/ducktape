// Pi policy wiring only. The ordinary Ducktape extension supplies its existing
// authenticated tool adapter; this package builds its own Chief service. This
// file owns no transport, storage or worker lifetime and injects no snapshots.
import { truncateHead } from '@earendil-works/pi-coding-agent';
import type { ExtensionAPI } from '@earendil-works/pi-coding-agent';

import type { ChiefBridge, ChiefControlNotice } from './contracts.ts';
import { PolicyError, requirePolicy } from './domain.ts';
import type { CommittedConversationEvent } from './inputs.ts';
import { createNetworkService } from './network.ts';
import type { NetworkChiefService, NetworkToolAdapter } from './network.ts';
import { CHIEF_PROMPT } from './prompt.ts';
import { CHIEF_TOOL_NAMES, registerChiefTools } from './tools.ts';

// -- Native conversation policy ---------------------------------------------
const appendControl = (pi: ExtensionAPI, notice: ChiefControlNotice): void => {
  // Serialize identities only, even if a host object carries extra fields.
  const content = JSON.stringify({
    operationId: notice.operationId, revision: notice.revision,
    kind: notice.kind, entityId: notice.entityId,
  });
  const bounded = truncateHead(content, { maxBytes: 2000, maxLines: 10 });
  pi.sendMessage({
    customType: 'chief-control', display: true,
    content: `Chief control notice (not a snapshot or authorization): ${bounded.content}${bounded.truncated ? ' [notice truncated; pull current state]' : ''}`,
  }, { triggerTurn: false, deliverAs: 'nextTurn' });
};

export const registerChief = (pi: ExtensionAPI, bridge: ChiefBridge): void => {
  const lifecycle: { unsubscribe?: () => void; activation?: object } = {};
  const allowed = new Set<string>(CHIEF_TOOL_NAMES);
  registerChiefTools(pi, bridge);

  pi.on('session_start', () => {
    pi.setActiveTools([...CHIEF_TOOL_NAMES]);
    const activation = {};
    lifecycle.activation = activation;
    lifecycle.unsubscribe?.();
    lifecycle.unsubscribe = bridge.subscribeControl(notice => {
      const stillActive = lifecycle.activation === activation;
      if (!stillActive) return;
      appendControl(pi, notice);
    });
  });
  pi.on('before_agent_start', event => {
    // Reassert the exact allowlist after other runtime tool activation changes.
    pi.setActiveTools([...CHIEF_TOOL_NAMES]);
    return { systemPrompt: `${event.systemPrompt}\n\n${CHIEF_PROMPT}` };
  });
  pi.on('tool_call', event => {
    const permitted = allowed.has(event.toolName);
    if (permitted) return;
    return { block: true, reason: 'Chief coordinates only through its registered chief_* tools; independent Jobs execute work.' };
  });
  pi.on('session_shutdown', () => {
    const unsubscribe = lifecycle.unsubscribe;
    delete lifecycle.unsubscribe;
    delete lifecycle.activation;
    unsubscribe?.();
  });
};

// -- Generic network handshake ----------------------------------------------
interface ResidentPreparationRequest {
  conversation_id: string; turn_id: string; events: CommittedConversationEvent[];
  accept(directive: Promise<{ wake: false } | { wake: true; prompt: string }>): void;
}
// Request the ordinary network adapter at session_start without awaiting it in
// that hook: another extension may finish authentication in a later start hook.
// before_agent_start waits for service readiness; no turn sees an unbound tool.
const chiefExtension = (pi: ExtensionAPI): void => {
  const lifetime = new AbortController();
  const listeners = new Set<(notice: ChiefControlNotice) => void>();
  const binding: { service?: ChiefBridge; unsubscribe?: () => void } = {};
  const disconnect = (): void => {
    const unsubscribe = binding.unsubscribe;
    delete binding.unsubscribe;
    unsubscribe?.();
  };
  const connect = (): void => {
    const service = binding.service;
    const hasSubscribers = listeners.size > 0;
    if (!service || !hasSubscribers) return;
    binding.unsubscribe = service.subscribeControl(notice => {
      if (lifetime.signal.aborted) return;
      listeners.forEach(listener => listener(notice));
    });
  };
  const adapterReady = new Promise<NetworkToolAdapter>(accept => {
    pi.on('session_start', () => {
      pi.events.emit('ducktape:network:bind', { accept });
    });
  });
  const serviceReady = Promise.resolve()
    .then(() => adapterReady)
    .then(adapter => {
      lifetime.signal.throwIfAborted();
      return createNetworkService(adapter);
    })
    .then(service => {
      lifetime.signal.throwIfAborted();
      binding.service = service;
      connect();
      return { success: true as const, data: service };
    })
    // Initialization can reject before any turn waits; retain the failure, not
    // an unhandled rejection or a fallback to unauthenticated/local execution.
    .catch(() => ({ success: false as const }));
  const waitForService = (signal?: AbortSignal): Promise<NetworkChiefService> => {
    const cancellation = signal ? AbortSignal.any([signal, lifetime.signal]) : lifetime.signal;
    return Promise.resolve()
      .then(() => new Promise<Awaited<typeof serviceReady>>((resolve, reject) => {
        const onAbort = (): void => {
          cancellation.removeEventListener('abort', onAbort);
          reject(new Error('chief_wait_cancelled'));
        };
        cancellation.addEventListener('abort', onAbort, { once: true });
        if (cancellation.aborted) onAbort();
        // The settlement handlers consume both paths; cleanup cannot leave a
        // rejected background promise when cancellation wins the readiness race.
        void serviceReady.then(resolve, reject)
          .finally(() => cancellation.removeEventListener('abort', onAbort));
      }))
      .then(result => {
        if (cancellation.aborted) throw new Error('chief_wait_cancelled');
        if (!result.success) throw new Error('chief_initialization_failed');
        return result.data;
      });
  };
  const proxy: ChiefBridge = {
    execute: (command, signal) => Promise.resolve()
      .then(() => waitForService(signal))
      .then(service => {
        lifetime.signal.throwIfAborted();
        signal?.throwIfAborted();
        return service.execute(command, signal);
      }),
    subscribeControl: listener => {
      listeners.add(listener);
      const firstSubscriber = listeners.size === 1;
      if (firstSubscriber) connect();
      return () => {
        listeners.delete(listener);
        const noSubscribers = listeners.size === 0;
        if (noSubscribers) disconnect();
      };
    },
  };

  // This provider boundary requires a synchronous single accept(Promise) and
  // an undefined handler return. Preparation persists structured committed
  // inputs before deciding whether the provider should invoke the model at all.
  const stopPreparing = pi.events.on('ducktape:resident:prepare', raw => {
    const hasAcceptance = typeof raw === 'object' && raw !== null && 'accept' in raw && typeof raw.accept === 'function';
    requirePolicy(hasAcceptance, 'invalid_resident_preparation');
    const request = raw as ResidentPreparationRequest;
    request.accept(Promise.resolve()
      .then(() => {
        const hasIdentity = typeof request.conversation_id === 'string' && request.conversation_id.length > 0
          && typeof request.turn_id === 'string' && request.turn_id.length > 0;
        requirePolicy(hasIdentity && Array.isArray(request.events), 'invalid_resident_preparation');
        return waitForService(lifetime.signal);
      })
      .then(service => {
        requirePolicy(request.conversation_id === service.conversationId, 'wrong_chief_conversation');
        return service.prepareInputs(request.events, lifetime.signal);
      })
      .then(result => {
        if (!result.wake) return { wake: false as const };
        const boundedPrompt = typeof result.prompt === 'string' && result.prompt.length > 0 && Buffer.byteLength(result.prompt, 'utf8') <= 24000;
        requirePolicy(boundedPrompt, 'invalid_resident_prompt');
        return { wake: true as const, prompt: result.prompt as string };
      })
      .catch((error: unknown) => {
        const knownCode = error instanceof PolicyError && /^[a-z][a-z0-9_]{0,79}$/.test(error.code);
        throw new Error(knownCode ? error.code : 'chief_input_preparation_failed');
      }));
  });

  registerChief(pi, proxy);
  pi.on('before_agent_start', (_event, ctx) => Promise.resolve()
    .then(() => waitForService(ctx.signal))
    .then(() => undefined));
  pi.on('session_shutdown', () => {
    stopPreparing();
    lifetime.abort(new Error('Chief session shut down.'));
    delete binding.service;
    disconnect();
  });
};

export default chiefExtension;
