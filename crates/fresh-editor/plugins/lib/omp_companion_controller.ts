/// <reference path="./fresh.d.ts" />

import {
  canInterruptOmpCompanion,
  isOmpCompanionLive,
  isOmpCompanionWorking,
  liveOmpCompanion,
  needsOmpApprovalAttention,
  type OmpCompanionSession,
  OmpCompanionTracker,
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
export interface OmpCompanionControllerSession extends OmpCompanionSession {
  label: string;
  oscRunning?: boolean | null;
  lastOutputAt: number | null;
}

export interface OmpCompanionStatusEntry {
  text: string;
  style?: Record<string, unknown>;
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
    windowId: number,
    terminalId: number,
    type: OmpCompanionCommandType,
  ): Promise<boolean>;
  setTerminalResume(
    windowId: number,
    terminalId: number,
    command: string[],
  ): Promise<boolean>;
  t(key: string, args?: Record<string, unknown>): string;
  setStatus(message: string): void;
  refreshUi(): void;
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

  constructor(private readonly host: OmpCompanionControllerHost<S>) {}

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
    if (session.oscRunning === true) return "working";
    if (session.oscRunning === false) return "idle";
    if (session.lastOutputAt === null) return "idle";
    return this.host.now() - session.lastOutputAt < idleAfterMs
      ? "working"
      : "idle";
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
        } · ${this.host.t("preview.omp_state", { state: snapshot.state })}`,
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
          snapshot.model ? `${snapshot.model.provider}/${snapshot.model.id}` : "—"
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
            snapshot.currentTool.intent ? ` — ${snapshot.currentTool.intent}` : ""
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
    return canInterruptOmpCompanion(session, this.host.now())
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
  handleSnapshot(payload: HookEventMap["omp_companion_snapshot"]): void {
    const { window_id: windowId, terminal_id: terminalId } = payload;
    let session = this.host.getSession(windowId);
    if (!session) {
      this.host.reconcileSessions();
      session = this.host.getSession(windowId);
    }
    if (!session) return;

    const receipt = this.tracker.receiveSnapshot(session, payload);
    if (!receipt) return;

    const { previousFacet, facet } = receipt;
    if (
      isOmpCompanionLive(facet, this.host.now()) &&
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
      windowId,
      terminalId,
      facet,
      (delayMs) => this.host.delay(delayMs),
      (id) => this.host.getSession(id),
      () => this.host.refreshUi(),
    );
    void this.tracker.persistResume(
      session,
      (id, terminal, command) =>
        this.host.setTerminalResume(id, terminal, command),
      (id) => this.host.getSession(id),
    );
    this.host.refreshUi();
  }

  /** Fence an exited terminal identity before general terminal-exit handling. */
  handleTerminalExit(payload: HookEventMap["terminal_exit"]): void {
    this.tracker.tombstoneTerminal(
      this.host.getSession(payload.window_id),
      payload.window_id,
      payload.terminal_id,
    );
  }

  /** Drop every process-local companion observation for a closed window. */
  handleWindowClosed(windowId: number): void {
    this.tracker.forgetWindow(this.host.getSession(windowId), windowId);
  }

  private async interrupt(session: S): Promise<void> {
    if (
      !canInterruptOmpCompanion(session, this.host.now()) ||
      session.terminalId === null
    ) return;
    let commandSucceeded = false;
    try {
      commandSucceeded = await this.host.sendCommand(
        session.id,
        session.terminalId,
        "cancel",
      );
    } catch {
      // Keep the terminal alive; expiry returns to the ordinary fallback.
    }
    if (commandSucceeded) return;
    if (this.tracker.markStale(session)) this.host.refreshUi();
    this.host.setStatus(
      this.host.t("status.omp_disconnected", { name: session.label }),
    );
  }
}
