/// <reference path="./fresh.d.ts" />
export interface OmpCompanionFacet {
  terminalId: WindowTerminalId;
  snapshot: OmpCompanionSnapshotV1;
  receivedAt: number;
}
export interface OmpCompanionSnapshotPayload {
  window_id: number;
  terminal_id: number;
  received_at_ms: number;
  snapshot: OmpCompanionSnapshotV1;
}

/** The ephemeral slice of an Orchestrator session owned by this module. */
export interface OmpCompanionSession {
  id: number;
  terminalId: WindowTerminalId | null;
  ompCompanion?: OmpCompanionFacet;
}

export interface OmpCompanionSnapshotReceipt {
  previousFacet: OmpCompanionFacet | undefined;
  facet: OmpCompanionFacet;
  semanticChanged: boolean;
}

interface PendingExpiry {
  terminalId: WindowTerminalId;
  facet: OmpCompanionFacet;
  onExpired: () => void;
}

interface OmpCompanionFreshness {
  incarnation: string;
  sequence: number;
  sessionGeneration: number;
  retiredIncarnations: Set<string>;
}

const OMP_COMPANION_LIVENESS_MS = 12_000;
const OMP_RETIRED_INCARNATIONS_MAX = 64;

function sameOmpCompanionSemantics(
  left: OmpCompanionSnapshotV1,
  right: OmpCompanionSnapshotV1,
): boolean {
  return left.version === right.version &&
    left.incarnation === right.incarnation &&
    left.sessionGeneration === right.sessionGeneration &&
    left.workEpoch === right.workEpoch &&
    left.ompVersion === right.ompVersion &&
    left.processId === right.processId &&
    left.sessionId === right.sessionId &&
    left.sessionName === right.sessionName &&
    left.cwd === right.cwd &&
    left.state === right.state &&
    left.statusText === right.statusText &&
    left.model?.provider === right.model?.provider &&
    left.model?.id === right.model?.id &&
    left.thinkingLevel === right.thinkingLevel &&
    left.runningTools === right.runningTools &&
    left.currentTool?.name === right.currentTool?.name &&
    left.currentTool?.intent === right.currentTool?.intent &&
    left.goal?.objective === right.goal?.objective &&
    left.goal?.status === right.goal?.status &&
    left.todos?.pending === right.todos?.pending &&
    left.todos?.inProgress === right.todos?.inProgress &&
    left.todos?.blocked === right.todos?.blocked &&
    left.todos?.completed === right.todos?.completed &&
    left.todos?.abandoned === right.todos?.abandoned &&
    left.todos?.current === right.todos?.current &&
    left.context?.tokens === right.context?.tokens &&
    left.context?.contextWindow === right.context?.contextWindow &&
    left.context?.percentBps === right.context?.percentBps &&
    left.pendingApprovals === right.pendingApprovals &&
    left.asyncJobs?.running === right.asyncJobs?.running &&
    left.asyncJobs?.recentFailures === right.asyncJobs?.recentFailures &&
    left.asyncJobs?.pendingDelivery === right.asyncJobs?.pendingDelivery;
}
export function sameWindowTerminalId(
  left: WindowTerminalId | null | undefined,
  right: WindowTerminalId | null | undefined,
): boolean {
  if (left == null || right == null) return left === right;
  return left.windowId === right.windowId &&
    left.terminalId === right.terminalId;
}

export function windowTerminalId(
  windowId: number,
  terminalId: number,
): WindowTerminalId {
  return { windowId, terminalId };
}

export function windowTerminalKey(terminal: WindowTerminalId): string {
  return `${terminal.windowId}:${terminal.terminalId}`;
}

export function isOmpCompanionLive(
  facet: OmpCompanionFacet | undefined,
  nowMs: number,
): boolean {
  return !!facet && nowMs >= facet.receivedAt &&
    nowMs - facet.receivedAt <= OMP_COMPANION_LIVENESS_MS;
}

export function liveOmpCompanion(
  session: OmpCompanionSession,
  nowMs: number,
): OmpCompanionFacet | undefined {
  const facet = session.ompCompanion;
  return session.terminalId !== null &&
      sameWindowTerminalId(facet?.terminalId, session.terminalId) &&
      isOmpCompanionLive(facet, nowMs)
    ? facet
    : undefined;
}

export function isOmpCompanionWorking(
  facet: OmpCompanionFacet | undefined,
): boolean {
  switch (facet?.snapshot.state) {
    case "working":
    case "awaiting_approval":
    case "retrying":
    case "compacting":
      return true;
    default:
      return false;
  }
}

export function needsOmpApprovalAttention(
  previousFacet: OmpCompanionFacet | undefined,
  nextFacet: OmpCompanionFacet,
  isActive: boolean,
): boolean {
  return !isActive &&
    (previousFacet?.snapshot.pendingApprovals ?? 0) === 0 &&
    nextFacet.snapshot.pendingApprovals > 0;
}

export function canInterruptOmpCompanion(
  session: OmpCompanionSession,
  nowMs: number,
): boolean {
  const snapshot = liveOmpCompanion(session, nowMs)?.snapshot;
  return !!snapshot &&
    session.terminalId !== null &&
    snapshot.state !== "idle" &&
    snapshot.state !== "stopped" &&
    snapshot.state !== "error";
}

/** Owns companion-only process-local observations and identity fences. */
export class OmpCompanionTracker {
  /** Highest terminal id that host reconciliation proved belonged to the row. */
  private readonly ownedExitHighWaterByWindow = new Map<number, number>();
  /** Freshness is needed only for the terminal currently selected by a row. */
  private readonly freshnessByWindow = new Map<number, OmpCompanionFreshness>();
  /** One live expiry candidate per workspace window. */
  private readonly expiries = new Map<number, PendingExpiry>();

  private expirySweepRunning = false;

  receiveSnapshot(
    session: OmpCompanionSession,
    payload: OmpCompanionSnapshotPayload,
  ): OmpCompanionSnapshotReceipt | undefined {
    const { window_id: windowId, terminal_id: terminalId, snapshot } = payload;
    const terminal = windowTerminalId(windowId, terminalId);
    if (
      session.id !== windowId ||
      !sameWindowTerminalId(session.terminalId, terminal) ||
      terminalId <= (this.ownedExitHighWaterByWindow.get(windowId) ?? -1)
    ) return undefined;

    let freshness = this.freshnessByWindow.get(windowId);
    if (!freshness) {
      freshness = {
        incarnation: snapshot.incarnation,
        sequence: snapshot.sequence,
        sessionGeneration: snapshot.sessionGeneration,
        retiredIncarnations: new Set(),
      };
      this.freshnessByWindow.set(windowId, freshness);
    } else {
      if (
        freshness.retiredIncarnations.has(snapshot.incarnation) ||
        snapshot.sessionGeneration < freshness.sessionGeneration
      ) return undefined;
      if (freshness.incarnation === snapshot.incarnation) {
        if (snapshot.sequence <= freshness.sequence) return undefined;
      } else {
        if (
          freshness.retiredIncarnations.size >= OMP_RETIRED_INCARNATIONS_MAX
        ) {
          return undefined;
        }
        freshness.retiredIncarnations.add(freshness.incarnation);
        freshness.incarnation = snapshot.incarnation;
      }
      freshness.sequence = snapshot.sequence;
      freshness.sessionGeneration = snapshot.sessionGeneration;
    }

    const previousFacet = session.ompCompanion;
    const facet: OmpCompanionFacet = {
      terminalId: terminal,
      snapshot,
      receivedAt: payload.received_at_ms,
    };
    session.ompCompanion = facet;
    return {
      previousFacet,
      facet,
      semanticChanged: previousFacet === undefined ||
        !sameOmpCompanionSemantics(previousFacet.snapshot, snapshot),
    };
  }

  rebindTerminal(
    session: OmpCompanionSession,
    terminalId: WindowTerminalId | null,
  ): boolean {
    if (sameWindowTerminalId(session.terminalId, terminalId)) return false;
    const previous = session.terminalId;
    this.expiries.delete(session.id);
    if (previous) {
      this.freshnessByWindow.delete(previous.windowId);
    }
    session.ompCompanion = undefined;
    session.terminalId = terminalId;
    return true;
  }

  markStale(session: OmpCompanionSession): boolean {
    if (!session.ompCompanion) return false;
    session.ompCompanion.receivedAt = 0;
    return true;
  }

  terminalExited(
    session: OmpCompanionSession | undefined,
    terminal: WindowTerminalId,
  ): boolean {
    if (
      !session || session.id !== terminal.windowId ||
      !sameWindowTerminalId(session.terminalId, terminal)
    ) return false;
    this.ownedExitHighWaterByWindow.set(
      terminal.windowId,
      Math.max(
        terminal.terminalId,
        this.ownedExitHighWaterByWindow.get(terminal.windowId) ?? -1,
      ),
    );
    return this.rebindTerminal(session, null);
  }

  forgetWindow(
    session: OmpCompanionSession | undefined,
    windowId: number,
  ): void {
    this.expiries.delete(windowId);
    this.ownedExitHighWaterByWindow.delete(windowId);
    this.freshnessByWindow.delete(windowId);
    if (session) session.ompCompanion = undefined;
  }

  scheduleExpiry(
    terminalId: WindowTerminalId,
    facet: OmpCompanionFacet,
    now: () => number,
    delay: (ms: number) => Promise<void>,
    getSession: (windowId: number) => OmpCompanionSession | undefined,
    onExpired: () => void,
  ): void {
    this.expiries.set(terminalId.windowId, { terminalId, facet, onExpired });
    if (this.expirySweepRunning) return;
    this.expirySweepRunning = true;
    void this.runExpirySweep(now, delay, getSession);
  }

  private async runExpirySweep(
    now: () => number,
    delay: (ms: number) => Promise<void>,
    getSession: (windowId: number) => OmpCompanionSession | undefined,
  ): Promise<void> {
    try {
      while (this.expiries.size > 0) {
        let nextExpiry = Number.POSITIVE_INFINITY;
        for (const pending of this.expiries.values()) {
          nextExpiry = Math.min(
            nextExpiry,
            pending.facet.receivedAt + OMP_COMPANION_LIVENESS_MS + 1,
          );
        }
        await delay(Math.max(0, nextExpiry - now()));

        let notify: (() => void) | undefined;
        const currentTime = now();
        for (const [windowId, pending] of this.expiries) {
          const session = getSession(windowId);
          if (
            !sameWindowTerminalId(session?.terminalId, pending.terminalId) ||
            session?.ompCompanion !== pending.facet
          ) {
            this.expiries.delete(windowId);
          } else if (!isOmpCompanionLive(pending.facet, currentTime)) {
            this.expiries.delete(windowId);
            notify ??= pending.onExpired;
          }
        }
        notify?.();
      }
    } finally {
      this.expirySweepRunning = false;
    }
  }
}
