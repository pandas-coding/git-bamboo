/**
 * Shared rebase write-RPC runner (phase 2 review extraction): the
 * start/continue/skip/abort rebase RPCs run through ONE implementation
 * shared by both consumers — the graph banner (graphSession.ts) and the
 * rebase panel (rebasePanel.ts). Before this module each surface carried
 * its own dispatch ternary and failure classification (the "no copied
 * logic" red line's second two-consumer flow).
 *
 * The runner owns two concerns:
 *  - the shared net-op in-flight guard (netOps.tryBeginNetOp — one
 *    mutating operation per engine client, the same slot pull/push/sync
 *    take): a net op racing a rebase continue, or two rebase controls
 *    from different surfaces, can never interleave. A request arriving
 *    while the slot is held is dropped, not queued;
 *  - uniform failure classification: INVALID_ARGUMENT = the engine
 *    refused the op (unresolved conflicts before a continue, a stale
 *    todo chain, or a rebase already on disk — the engine message is
 *    user-readable) → 'refused'; anything else is a real engine or
 *    transport failure → 'failed'.
 * Consumers keep only their own UI (progress toasts, banners, busy
 * pushes) on top of the classified outcome.
 */
import * as netOps from './netOps';
import { EngineClient, RpcErrorCode, rpcErrorCode } from './engineClient';
import { errorMessage } from './util';
import type { RebaseResult, RebaseTodoEntry } from './types';

/** Rebase control ops both consumers can request. */
export type RebaseStepKind = 'continue' | 'skip' | 'abort';

/** Classified outcome of a rebase write RPC (see the module comment). */
export type RebaseStepOutcome =
  /** Dropped: another mutating op is in flight for this repo (the shared
   *  guard). Consumers surface the drop their own way. */
  | { kind: 'dropped' }
  /** Engine refusal (INVALID_ARGUMENT); `message` is user-readable. */
  | { kind: 'refused'; message: string }
  /** Real engine/transport failure; `message` is the raw error text. */
  | { kind: 'failed'; message: string }
  /** The RPC succeeded — the rebase result (stopped or finished). */
  | { kind: 'result'; result: RebaseResult };

/** Runs one rebase control (continue/skip/abort) under the shared
 *  in-flight guard, with the shared failure classification. */
export async function runRebaseStep(
  client: EngineClient,
  kind: RebaseStepKind,
): Promise<RebaseStepOutcome> {
  if (!netOps.tryBeginNetOp(client)) return { kind: 'dropped' };
  try {
    const result =
      kind === 'continue'
        ? await client.rebaseContinue()
        : kind === 'skip'
          ? await client.rebaseSkip()
          : await client.rebaseAbort();
    return { kind: 'result', result };
  } catch (err) {
    return classifyRebaseError(err);
  } finally {
    netOps.endNetOp(client);
  }
}

/** Starts an interactive rebase (the plan panel's Execute) under the
 *  same guard and classification. */
export async function runRebaseStart(
  client: EngineClient,
  onto: string,
  todo: RebaseTodoEntry[],
): Promise<RebaseStepOutcome> {
  if (!netOps.tryBeginNetOp(client)) return { kind: 'dropped' };
  try {
    return { kind: 'result', result: await client.startRebase(onto, todo) };
  } catch (err) {
    return classifyRebaseError(err);
  } finally {
    netOps.endNetOp(client);
  }
}

/** INVALID_ARGUMENT → 'refused' (engine message is user-readable);
 *  everything else → 'failed'. */
function classifyRebaseError(err: unknown): RebaseStepOutcome {
  if (rpcErrorCode(err) === RpcErrorCode.INVALID_ARGUMENT) {
    return { kind: 'refused', message: errorMessage(err) };
  }
  return { kind: 'failed', message: errorMessage(err) };
}
