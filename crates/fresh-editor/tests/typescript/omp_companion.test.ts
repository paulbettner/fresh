/// <reference path="../../plugins/lib/fresh.d.ts" />

import {
  OmpCompanionController,
  type OmpCompanionControllerHost,
  type OmpCompanionControllerSession,
} from "../../plugins/lib/omp_companion_controller.ts";
import {
  windowTerminalId,
  windowTerminalKey,
} from "../../plugins/lib/omp_companion.ts";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

function snapshot(
  sequence: number,
  timestampMs: number,
): OmpCompanionSnapshotV1 {
  return {
    version: 1,
    incarnation: "123e4567-e89b-42d3-a456-426614174000",
    sequence,
    sessionGeneration: 1,
    workEpoch: 1,
    timestampMs,
    ompVersion: "1.0.0",
    processId: 42,
    sessionId: "00000000-0000-4000-8000-000000000001",
    sessionName: "companion",
    cwd: "/workspace",
    state: "working",
    statusText: "Working",
    runningTools: 1,
    pendingApprovals: 0,
  };
}

function controllerHarness() {
  const terminal = windowTerminalId(1, 7);
  const session: OmpCompanionControllerSession = {
    id: 1,
    terminalId: terminal,
    label: "workspace",
    lastOutputAt: null,
  };
  let now = 100;
  let refreshes = 0;
  const host = {
    getSession(windowId: number) {
      return windowId === session.id ? session : undefined;
    },
    reconcileSessions() {},
    activeWindowId() {
      return session.id;
    },
    now() {
      return now;
    },
    delay(_ms: number) {
      return new Promise<void>(() => {});
    },
    sendCommand() {
      return Promise.resolve(true);
    },
    t(key: string) {
      return key;
    },
    setStatus() {},
    refreshUi() {
      refreshes += 1;
    },
    refreshSessionLabel() {},
  } satisfies OmpCompanionControllerHost<typeof session>;
  return {
    controller: new OmpCompanionController(host),
    session,
    terminal,
    setNow(value: number) {
      now = value;
    },
    refreshes() {
      return refreshes;
    },
  };
}

Deno.test("heartbeat-only companion snapshots renew liveness without refreshing UI", () => {
  const harness = controllerHarness();
  harness.controller.handleSnapshot({
    window_id: 1,
    terminal_id: 7,
    received_at_ms: 100,
    snapshot: snapshot(1, 1),
  });
  assert(
    harness.refreshes() === 1,
    "the first semantic snapshot must refresh UI",
  );

  harness.setNow(200);
  harness.controller.handleSnapshot({
    window_id: 1,
    terminal_id: 7,
    received_at_ms: 200,
    snapshot: snapshot(2, 2),
  });
  assert(
    harness.refreshes() === 1,
    "sequence/timestamp-only heartbeat must not trigger a semantic UI refresh",
  );
  assert(
    harness.session.ompCompanion?.receivedAt === 200,
    "heartbeat must still renew the facet receipt time",
  );
});

Deno.test("expired approval snapshot falls back to ordinary activity", () => {
  const harness = controllerHarness();
  const approval = snapshot(1, 1);
  approval.state = "awaiting_approval";
  approval.statusText = "Awaiting approval";
  harness.controller.handleSnapshot({
    window_id: 1,
    terminal_id: 7,
    received_at_ms: 100,
    snapshot: approval,
  });

  assert(
    harness.controller.statusEntry(harness.session, 1_000).text === "* ",
    "a live approval snapshot must use the companion activity projection",
  );
  assert(
    harness.controller.statusTextEntry(harness.session)?.text === "Awaiting approval",
    "a live approval snapshot must retain its companion status text",
  );

  harness.setNow(12_101);
  assert(
    harness.controller.statusTextEntry(harness.session) === undefined,
    "an expired approval snapshot must not remain visible",
  );
  assert(
    harness.controller.statusEntry(harness.session, 1_000).text === "· ",
    "an expired approval snapshot must fall back to idle activity",
  );
});

Deno.test("selected terminal cleanup preserves unrelated fallback activity", () => {
  const harness = controllerHarness();
  const peer = windowTerminalId(1, 8);
  harness.controller.recordTerminalOutput(
    harness.session,
    harness.terminal,
    10,
  );
  harness.controller.recordTerminalOutput(harness.session, peer, 20);

  harness.controller.handleTerminalExit({
    window_id: harness.terminal.windowId,
    terminal_id: harness.terminal.terminalId,
    exit_code: 0,
  });

  assert(
    harness.session.terminalId === null,
    "the exited owner must be unbound",
  );
  assert(
    !harness.session.terminalActivities?.has(
      windowTerminalKey(harness.terminal),
    ),
    "the exited owner's fallback state must be removed",
  );
  assert(
    harness.session.terminalActivities?.get(windowTerminalKey(peer))
      ?.lastOutputAt === 20,
    "another terminal's fallback state must survive owner cleanup",
  );
  assert(
    harness.session.lastOutputAt === 20 && harness.session.liveOutputAt === 20,
    "aggregate fallback timestamps must be recomputed from surviving activity",
  );
});
