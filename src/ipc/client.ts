// Typed IPC wrapper (spec §8.1). Every command takes {meta, args} and returns CommandResult.
import { invoke, Channel } from '@tauri-apps/api/core';
import type { WireError } from './generated/WireError';
import type { StreamEnvelope } from './generated/StreamEnvelope';

export type CommandResult<T> =
  | { ok: true; requestId: string; data: T }
  | { ok: false; requestId: string; error: WireError };

export class CommandError extends Error {
  constructor(public readonly error: WireError, public readonly requestId: string) {
    super(error.message);
  }
  get code() { return this.error.code; }
}

export function newRequestId(): string {
  return crypto.randomUUID();
}

export async function call<T>(cmd: string, args?: unknown, opts: { requestId?: string; channel?: Channel<StreamEnvelope> } = {}): Promise<T> {
  const requestId = opts.requestId ?? newRequestId();
  const payload: Record<string, unknown> = { meta: { apiVersion: 1, requestId } };
  if (args !== undefined) payload.args = args;
  if (opts.channel) payload.onEvent = opts.channel;
  const res = await invoke<CommandResult<T>>(cmd, payload);
  if (!res || typeof res !== 'object') throw new CommandError({ code: 'INTERNAL', message: '응답이 올바르지 않습니다.', retryable: false }, requestId);
  if (res.ok) return res.data;
  throw new CommandError(res.error, res.requestId);
}

/**
 * Mutations retry with the same requestId when the transport fails, so the backend
 * receipt makes them idempotent (spec §8.3).
 */
export async function mutate<T>(cmd: string, args: unknown, attempts = 2): Promise<T> {
  const requestId = newRequestId();
  let last: unknown;
  for (let i = 0; i < attempts; i++) {
    try {
      return await call<T>(cmd, args, { requestId });
    } catch (e) {
      if (e instanceof CommandError) throw e;
      last = e;
    }
  }
  throw last;
}

export function errorMessage(e: unknown): string {
  if (e instanceof CommandError) return e.message;
  if (e instanceof Error) return e.message;
  return String(e);
}
