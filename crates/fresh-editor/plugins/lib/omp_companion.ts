/// <reference path="./fresh.d.ts" />
export interface OmpCompanionFacet {
  snapshot: OmpCompanionSnapshotV1;
  receivedAt: number;
  launchExecutable: string;
  resumeSessionId?: string;
}

/** The ephemeral slice of an Orchestrator session owned by this module. */
export interface OmpCompanionSession {
  id: number;
  terminalId: number | null;
  ompCompanion?: OmpCompanionFacet;
}

interface OmpCompanionResumeUpdate {
  windowId: number;
  terminalId: number;
  incarnation: string;
  sessionGeneration: number;
  sessionId: string;
}

export interface OmpCompanionSnapshotReceipt {
  previousFacet: OmpCompanionFacet | undefined;
  facet: OmpCompanionFacet;
}

const OMP_COMPANION_LIVENESS_MS = 12_000;

/** Terminal ids are only unique within their owning editor window. */
export function ompCompanionIdentity(
  windowId: number,
  terminalId: number,
): string {
  return `${windowId}:${terminalId}`;
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
  return isOmpCompanionLive(session.ompCompanion, nowMs)
    ? session.ompCompanion
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

/**
 * Owns companion-only maps. Nothing in this tracker is persisted: facets,
 * resume fencing, and exit tombstones are process-local host observations.
 */
export class OmpCompanionTracker {
  /** Exact dead identities, retained until their window is forgotten. */
  private readonly exitedTerminalsByWindow = new Map<number, Set<number>>();
  private readonly resumeUpdates = new Map<string, OmpCompanionResumeUpdate>();

  receiveSnapshot(
    session: OmpCompanionSession,
    payload: HookEventMap["omp_companion_snapshot"],
  ): OmpCompanionSnapshotReceipt | undefined {
    const { window_id: windowId, terminal_id: terminalId, snapshot } = payload;
    if (
      session.id !== windowId ||
      this.exitedTerminalsByWindow.get(windowId)?.has(terminalId)
    ) return undefined;

    // Reconciled/restored windows do not expose terminal ids through
    // listWindows. The first authenticated companion snapshot establishes it.
    const claimsUnreconciledSession = session.terminalId === null;
    if (!claimsUnreconciledSession && session.terminalId !== terminalId) {
      return undefined;
    }

    const previousFacet = session.ompCompanion;
    if (
      previousFacet?.snapshot.incarnation === snapshot.incarnation &&
      snapshot.sequence <= previousFacet.snapshot.sequence
    ) {
      return undefined;
    }

    const facet: OmpCompanionFacet = {
      snapshot,
      receivedAt: payload.received_at_ms,
      launchExecutable: payload.launch_executable,
      resumeSessionId: previousFacet?.snapshot.sessionId === snapshot.sessionId
        ? previousFacet.resumeSessionId
        : undefined,
    };
    if (claimsUnreconciledSession) session.terminalId = terminalId;
    session.ompCompanion = facet;
    return { previousFacet, facet };
  }

  markStale(session: OmpCompanionSession): boolean {
    if (!session.ompCompanion) return false;
    session.ompCompanion.receivedAt = 0;
    if (session.terminalId !== null) {
      this.resumeUpdates.delete(
        ompCompanionIdentity(session.id, session.terminalId),
      );
    }
    return true;
  }

  tombstoneTerminal(
    session: OmpCompanionSession | undefined,
    windowId: number,
    terminalId: number,
  ): void {
    if (session && session.id !== windowId) return;

    let exitedTerminals = this.exitedTerminalsByWindow.get(windowId);
    if (!exitedTerminals) {
      exitedTerminals = new Set();
      this.exitedTerminalsByWindow.set(windowId, exitedTerminals);
    }
    exitedTerminals.add(terminalId);

    if (!session || session.terminalId === null) return;
    if (session.terminalId !== terminalId) return;

    const identity = ompCompanionIdentity(windowId, terminalId);
    this.resumeUpdates.delete(identity);
    session.ompCompanion = undefined;
    session.terminalId = null;
  }

  forgetWindow(
    session: OmpCompanionSession | undefined,
    windowId: number,
  ): void {
    this.exitedTerminalsByWindow.delete(windowId);
    const prefix = `${windowId}:`;
    for (const identity of this.resumeUpdates.keys()) {
      if (identity.startsWith(prefix)) this.resumeUpdates.delete(identity);
    }
    if (session) session.ompCompanion = undefined;
  }

  scheduleExpiry(
    windowId: number,
    terminalId: number,
    facet: OmpCompanionFacet,
    delay: (ms: number) => Promise<void>,
    getSession: (windowId: number) => OmpCompanionSession | undefined,
    onExpired: () => void,
  ): void {
    const delayMs = Math.max(
      0,
      facet.receivedAt + OMP_COMPANION_LIVENESS_MS - Date.now() + 1,
    );
    void delay(delayMs).then(() => {
      const session = getSession(windowId);
      if (
        session?.terminalId === terminalId &&
        session.ompCompanion === facet &&
        !isOmpCompanionLive(facet, Date.now())
      ) {
        onExpired();
      }
    });
  }

  async persistResume(
    session: OmpCompanionSession,
    setTerminalResume: (
      windowId: number,
      terminalId: number,
      command: string[],
    ) => Promise<boolean>,
    getSession: (windowId: number) => OmpCompanionSession | undefined,
  ): Promise<void> {
    const facet = session.ompCompanion;
    const terminalId = session.terminalId;
    if (
      !facet || terminalId === null ||
      facet.resumeSessionId === facet.snapshot.sessionId
    ) return;

    const windowId = session.id;
    const identity = ompCompanionIdentity(windowId, terminalId);
    if (
      this.sameResumeUpdate(
        this.resumeUpdates.get(identity),
        windowId,
        terminalId,
        facet,
      )
    ) return;

    const update: OmpCompanionResumeUpdate = {
      windowId,
      terminalId,
      incarnation: facet.snapshot.incarnation,
      sessionGeneration: facet.snapshot.sessionGeneration,
      sessionId: facet.snapshot.sessionId,
    };
    this.resumeUpdates.set(identity, update);

    try {
      const persisted = await setTerminalResume(
        windowId,
        terminalId,
        [facet.launchExecutable, "--resume", update.sessionId],
      );
      if (
        !this.sameResumeUpdate(
          this.resumeUpdates.get(identity),
          windowId,
          terminalId,
          facet,
        )
      ) return;

      this.resumeUpdates.delete(identity);
      const current = getSession(windowId);
      const currentFacet = current?.ompCompanion;
      if (
        persisted &&
        current?.terminalId === terminalId &&
        currentFacet &&
        this.matchesResumeUpdate(currentFacet, update)
      ) {
        currentFacet.resumeSessionId = update.sessionId;
      }
    } catch {
      if (
        this.sameResumeUpdate(
          this.resumeUpdates.get(identity),
          windowId,
          terminalId,
          facet,
        )
      ) {
        this.resumeUpdates.delete(identity);
      }
    }
  }

  private sameResumeUpdate(
    update: OmpCompanionResumeUpdate | undefined,
    windowId: number,
    terminalId: number,
    facet: OmpCompanionFacet,
  ): boolean {
    return !!update &&
      update.windowId === windowId &&
      update.terminalId === terminalId &&
      this.matchesResumeUpdate(facet, update);
  }

  private matchesResumeUpdate(
    facet: OmpCompanionFacet | undefined,
    update: OmpCompanionResumeUpdate,
  ): boolean {
    const snapshot = facet?.snapshot;
    return !!snapshot &&
      snapshot.incarnation === update.incarnation &&
      snapshot.sessionGeneration === update.sessionGeneration &&
      snapshot.sessionId === update.sessionId;
  }
}
