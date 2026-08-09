/// <reference path="./fresh.d.ts" />

import {
  canInterruptOmpCompanion,
  isOmpCompanionLive,
  isOmpCompanionWorking,
  liveOmpCompanion,
  needsOmpApprovalAttention,
  type OmpCompanionFacet,
  type OmpCompanionSession,
  type OmpCompanionSnapshotPayload,
  OmpCompanionTracker,
  sameWindowTerminalId,
  windowTerminalId,
  windowTerminalKey,
} from "./omp_companion.ts";
import {
  button,
  spacer,
  styledRow,
  type TextPropertyEntry,
  type WidgetSpec,
} from "./widgets.ts";

export type OmpCompanionActivityState = "working" | "idle";

/**
 * The narrow Orchestrator session projection needed by the companion layer.
 * The host continues to own the session map and every non-companion field.
 */
export interface OmpTerminalFallbackActivity {
  oscRunning?: boolean | null;
  oscChangedAt?: number;
  lastOutputAt?: number;
}

export interface OmpCompanionControllerSession extends OmpCompanionSession {
  label: string;
  terminalTitle?: string;
  terminalSpinner?: string;
  lastOutputAt: number | null;
  /** Most recent output from a terminal that is still live. */
  liveOutputAt?: number | null;
  terminalActivities?: Map<string, OmpTerminalFallbackActivity>;
}

export interface OmpCompanionStatusEntry {
  text: string;
  style?: Record<string, unknown>;
  shimmer?: boolean;
}

export interface OmpCompanionControllerHost<
  S extends OmpCompanionControllerSession,
> {
  getSession(windowId: number): S | undefined;
  reconcileSessions(): void;
  activeWindowId(): number;
  now(): number;
  delay(ms: number): Promise<void>;
  sendCommand(
    terminalId: WindowTerminalId,
    type: OmpCompanionCommandType,
    target: OmpCompanionCommandTargetV1,
  ): Promise<boolean>;
  t(key: string, args?: Record<string, unknown>): string;
  setStatus(message: string): void;
  refreshUi(): void;
  refreshSessionLabel(session: S): void;
}

const STATE_SYMBOL: Record<
  OmpCompanionActivityState,
  { glyph: string; fg: string }
> = {
  working: { glyph: "*", fg: "diagnostic.warning_fg" },
  idle: { glyph: "·", fg: "ui.menu_disabled_fg" },
};

/**
 * Owns the Fresh-facing OMP companion projection, presentation, actions, and
 * hook reduction. `OmpCompanionTracker` may update only the narrow companion
 * projection it receives (`terminalId` reconciliation plus `ompCompanion`);
 * the Orchestrator host remains authoritative for map membership and every
 * other workspace/session lifecycle field.
 */
export class OmpCompanionController<
  S extends OmpCompanionControllerSession,
> {
  private readonly tracker = new OmpCompanionTracker();
  private readonly interruptsInFlight = new Map<string, OmpCompanionFacet>();

  constructor(private readonly host: OmpCompanionControllerHost<S>) {}
  /** Atomically move every terminal-owned facet onto a new host-selected id. */
  rebindTerminal(session: S, terminalId: WindowTerminalId | null): boolean {
    const previous = session.terminalId;
    if (!this.tracker.rebindTerminal(session, terminalId)) return false;
    if (previous) this.interruptsInFlight.delete(windowTerminalKey(previous));
    this.clearTerminalPresentation(session, previous ?? undefined);
    return true;
  }

  recordTerminalOutput(
    session: S,
    terminalId: WindowTerminalId,
    at: number,
  ): void {
    this.activityFor(session, terminalId).lastOutputAt = at;
    session.lastOutputAt = at;
    session.liveOutputAt = at;
  }

  recordOscActivity(
    session: S,
    terminalId: WindowTerminalId,
    running: boolean,
    at: number,
  ): void {
    if (!sameWindowTerminalId(session.terminalId, terminalId)) return;
    const activity = this.activityFor(session, terminalId);
    if (activity.oscRunning !== running) activity.oscChangedAt = at;
    activity.oscRunning = running;
  }

  private activityFor(
    session: S,
    terminalId: WindowTerminalId,
  ): OmpTerminalFallbackActivity {
    session.terminalActivities ??= new Map();
    const key = windowTerminalKey(terminalId);
    let activity = session.terminalActivities.get(key);
    if (!activity) {
      activity = {};
      session.terminalActivities.set(key, activity);
    }
    return activity;
  }

  private refreshFallbackOutputTimes(session: S): void {
    let latest: number | null = null;
    if (session.terminalActivities) {
      for (const activity of session.terminalActivities.values()) {
        if (
          activity.lastOutputAt !== undefined &&
          (latest === null || activity.lastOutputAt > latest)
        ) {
          latest = activity.lastOutputAt;
        }
      }
    }
    session.lastOutputAt = latest;
    session.liveOutputAt = latest;
  }

  private clearTerminalPresentation(
    session: S,
    terminalId: WindowTerminalId | undefined,
  ): void {
    if (terminalId) {
      session.terminalActivities?.delete(windowTerminalKey(terminalId));
    }
    this.refreshFallbackOutputTimes(session);
    session.terminalTitle = undefined;
    session.terminalSpinner = undefined;
    this.host.refreshSessionLabel(session);
  }

  /**
   * A live structured snapshot overrides the legacy OSC/output heuristic.
   * Once the facet expires, projection deliberately falls back unchanged.
   */
  activityState(
    session: S,
    idleAfterMs: number,
  ): OmpCompanionActivityState {
    const companion = liveOmpCompanion(session, this.host.now());
    if (companion) {
      return isOmpCompanionWorking(companion) ? "working" : "idle";
    }

    const owner = session.terminalId === null
      ? undefined
      : session.terminalActivities?.get(windowTerminalKey(session.terminalId));
    const explicitRunning = owner?.oscRunning;
    if (explicitRunning === true) return "working";

    const liveOutputAt = session.liveOutputAt ?? null;
    const recentlyPrinted = liveOutputAt !== null &&
      this.host.now() - liveOutputAt < idleAfterMs;
    if (
      recentlyPrinted &&
      (explicitRunning !== false || liveOutputAt > (owner?.oscChangedAt ?? 0))
    ) return "working";
    if (explicitRunning === false) return "idle";
    return recentlyPrinted ? "working" : "idle";
  }

  /** Build the complete live-session status glyph, including OMP error state. */
  statusEntry(session: S, idleAfterMs: number): OmpCompanionStatusEntry {
    if (
      liveOmpCompanion(session, this.host.now())?.snapshot.state === "error"
    ) {
      return {
        text: "! ",
        style: { fg: "ui.status_error_indicator_fg", bold: true },
      };
    }
    const symbol = STATE_SYMBOL[this.activityState(session, idleAfterMs)];
    return {
      text: symbol.glyph + " ",
      style: { fg: symbol.fg, bold: true },
    };
  }

  private protocolStateLabel(state: OmpCompanionSnapshotV1["state"]): string {
    switch (state) {
      case "working":
        return this.host.t("preview.state_working");
      case "awaiting_approval":
        return this.host.t("pill.omp_awaiting_approval");
      case "retrying":
        return this.host.t("pill.omp_retrying");
      case "compacting":
        return this.host.t("pill.omp_compacting");
      case "idle":
        return this.host.t("preview.state_idle");
      case "stopped":
        return this.host.t("status.verb_stopped");
      case "error":
        return this.host.t("err.failed");
    }
  }

  /** Build the live OMP activity text shown directly in session rows. */
  statusTextEntry(session: S): OmpCompanionStatusEntry | undefined {
    const snapshot = liveOmpCompanion(session, this.host.now())?.snapshot;
    if (!snapshot) return undefined;

    const text = snapshot.statusText ?? this.protocolStateLabel(snapshot.state);
    let style: Record<string, unknown>;
    let shimmer = false;
    switch (snapshot.state) {
      case "working":
        style = { fg: "diagnostic.warning_fg", italic: true };
        shimmer = true;
        break;
      case "awaiting_approval":
        style = { fg: "diagnostic.warning_fg", bold: true };
        break;
      case "retrying":
      case "compacting":
        style = { fg: "diagnostic.warning_fg", italic: true };
        shimmer = true;
        break;
      case "idle":
      case "stopped":
        style = { fg: "ui.menu_disabled_fg", italic: true };
        break;
      case "error":
        style = { fg: "ui.status_error_indicator_fg", bold: true };
        break;
    }

    return { text, style, shimmer };
  }

  /** Build the allowlisted OMP rows appended to Orchestrator preview details. */
  previewEntries(session: S): TextPropertyEntry[] {
    const facet = session.ompCompanion;
    if (!facet) return [];

    const snapshot = facet.snapshot;
    const connected = isOmpCompanionLive(facet, this.host.now());
    const fg = snapshot.state === "error"
      ? "ui.status_error_indicator_fg"
      : connected
      ? "diagnostic.info_fg"
      : "ui.menu_disabled_fg";
    const percent = snapshot.context?.percentBps === undefined
      ? undefined
      : snapshot.context.percentBps / 100;
    const percentText = percent === undefined
      ? ""
      : Number.isInteger(percent)
      ? String(percent)
      : percent.toFixed(2).replace(/0+$/, "").replace(/\.$/, "");
    const entries: TextPropertyEntry[] = [
      styledRow([{
        text: `${
          this.host.t(
            connected ? "preview.omp_connected" : "preview.omp_disconnected",
          )
        } · ${
          this.host.t("preview.omp_state", {
            state: this.protocolStateLabel(snapshot.state),
          })
        }`,
        style: { fg, bold: connected },
      }]),
      styledRow([{
        text: `${this.host.t("preview.omp_session")}: ${
          snapshot.sessionId.slice(0, 8)
        }${snapshot.sessionName ? ` · ${snapshot.sessionName}` : ""}`,
      }]),
    ];
    if (snapshot.model || snapshot.thinkingLevel) {
      entries.push(styledRow([{
        text: `${this.host.t("preview.omp_model")}: ${
          snapshot.model
            ? `${snapshot.model.provider}/${snapshot.model.id}`
            : "—"
        }${snapshot.thinkingLevel ? ` · ${snapshot.thinkingLevel}` : ""}`,
      }]));
    }
    if (snapshot.context) {
      entries.push(styledRow([{
        text: `${
          this.host.t("preview.omp_context")
        }: ${snapshot.context.tokens}/${snapshot.context.contextWindow}${
          percentText ? ` · ${percentText}%` : ""
        }`,
      }]));
    }
    entries.push(styledRow([{
      text: `${this.host.t("preview.omp_tool")}: ${snapshot.runningTools}${
        snapshot.currentTool
          ? ` · ${snapshot.currentTool.name}${
            snapshot.currentTool.intent
              ? ` — ${snapshot.currentTool.intent}`
              : ""
          }`
          : ""
      }`,
    }]));
    entries.push(styledRow([{
      text: `${
        this.host.t("preview.omp_approvals")
      }: ${snapshot.pendingApprovals}`,
    }]));
    if (snapshot.goal) {
      entries.push(styledRow([{
        text: `${
          this.host.t("preview.omp_goal")
        }: ${snapshot.goal.status} · ${snapshot.goal.objective}`,
      }]));
    }
    if (snapshot.todos) {
      const todos = snapshot.todos;
      entries.push(styledRow([{
        text: `${
          this.host.t("preview.omp_todos")
        }: P ${todos.pending} · I ${todos.inProgress} · B ${todos.blocked} · C ${todos.completed} · A ${todos.abandoned}${
          todos.current ? ` · ${todos.current}` : ""
        }`,
      }]));
    }
    if (snapshot.asyncJobs) {
      const jobs = snapshot.asyncJobs;
      entries.push(styledRow([{
        text: `${
          this.host.t("preview.omp_async")
        }: ${jobs.running} · ${jobs.recentFailures} · ${jobs.pendingDelivery}`,
      }]));
    }
    return entries;
  }

  /** Companion-only preview action widgets, preserving the existing key. */
  previewActions(session: S): WidgetSpec[] {
    const facet = liveOmpCompanion(session, this.host.now());
    return canInterruptOmpCompanion(session, this.host.now()) &&
        facet !== undefined &&
        this.interruptsInFlight.get(windowTerminalKey(facet.terminalId)) !==
          facet
      ? [
        spacer(2),
        button(this.host.t("preview.btn_interrupt"), { key: "omp-interrupt" }),
      ]
      : [];
  }

  /** Route the companion action out of Orchestrator's general widget switch. */
  handleWidgetEvent(
    event: HookEventMap["widget_event"],
    selectedSession: S | undefined,
  ): boolean {
    if (
      event.event_type !== "activate" || event.widget_key !== "omp-interrupt"
    ) return false;
    if (selectedSession) void this.interrupt(selectedSession);
    return true;
  }

  /** Reduce one authenticated snapshot and coordinate attention/resume/UI. */
  handleSnapshot(payload: OmpCompanionSnapshotPayload): void {
    const { window_id: windowId, terminal_id: terminalId } = payload;
    const terminal = windowTerminalId(windowId, terminalId);
    // The host owns terminal selection. Refresh it before comparing the hook's
    // identity: a delayed hook from the formerly selected terminal can still
    // match this controller's stale cache after the host has rebound the row.
    this.host.reconcileSessions();
    const session = this.host.getSession(windowId);
    if (!session) return;

    const receipt = this.tracker.receiveSnapshot(session, payload);
    if (!receipt) return;

    const { previousFacet, facet, semanticChanged } = receipt;
    const now = this.host.now();
    const livenessChanged = isOmpCompanionLive(previousFacet, now) !==
      isOmpCompanionLive(facet, now);
    if (semanticChanged) {
      this.interruptsInFlight.delete(windowTerminalKey(terminal));
    }
    if (
      isOmpCompanionLive(facet, now) &&
      needsOmpApprovalAttention(
        previousFacet,
        facet,
        this.host.activeWindowId() === session.id,
      )
    ) {
      this.host.setStatus(
        this.host.t("status.omp_approval_attention", { name: session.label }),
      );
    }
    this.tracker.scheduleExpiry(
      terminal,
      facet,
      () => this.host.now(),
      (delayMs) => this.host.delay(delayMs),
      (id) => this.host.getSession(id),
      () => this.host.refreshUi(),
    );
    if (semanticChanged || livenessChanged) this.host.refreshUi();
  }

  /** Drop one terminal's fallback state; only its owner exit clears the facet. */
  handleTerminalExit(payload: HookEventMap["terminal_exit"]): void {
    // As with snapshots, an exit can race the async continuation that updates
    // the plugin model after the host has already selected a replacement.
    this.host.reconcileSessions();
    const terminal = windowTerminalId(payload.window_id, payload.terminal_id);
    this.interruptsInFlight.delete(windowTerminalKey(terminal));
    const session = this.host.getSession(terminal.windowId);
    const trackedExited = this.tracker.terminalExited(session, terminal);
    if (!session) return;
    if (trackedExited) {
      this.clearTerminalPresentation(session, terminal);
      return;
    }
    session.terminalActivities?.delete(windowTerminalKey(terminal));
    this.refreshFallbackOutputTimes(session);
  }

  /** Drop every process-local companion observation for a closed window. */
  handleWindowClosed(windowId: number): void {
    this.tracker.forgetWindow(this.host.getSession(windowId), windowId);
    for (const key of this.interruptsInFlight.keys()) {
      if (key.startsWith(`${windowId}:`)) this.interruptsInFlight.delete(key);
    }
  }
  private async interrupt(session: S): Promise<void> {
    if (
      !canInterruptOmpCompanion(session, this.host.now()) ||
      session.terminalId === null
    ) return;
    const windowId = session.id;
    const terminalId = session.terminalId;
    const facet = session.ompCompanion;
    if (!facet) return;
    const key = windowTerminalKey(terminalId);
    if (this.interruptsInFlight.get(key) === facet) return;
    this.interruptsInFlight.set(key, facet);
    this.host.refreshUi();
    let commandSucceeded = false;
    try {
      commandSucceeded = await this.host.sendCommand(terminalId, "cancel", {
        incarnation: facet.snapshot.incarnation,
        sessionGeneration: facet.snapshot.sessionGeneration,
        sessionId: facet.snapshot.sessionId,
        workEpoch: facet.snapshot.workEpoch,
      });
    } catch {
      // Keep the terminal alive; expiry returns to the ordinary fallback.
    }
    if (commandSucceeded) return;
    this.interruptsInFlight.delete(key);
    const current = this.host.getSession(windowId);
    if (
      !current || !sameWindowTerminalId(current.terminalId, terminalId) ||
      current.ompCompanion !== facet
    ) return;
    if (this.tracker.markStale(current)) this.host.refreshUi();
    this.host.setStatus(
      this.host.t("status.omp_disconnected", { name: current.label }),
    );
  }
}
