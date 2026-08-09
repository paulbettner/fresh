export interface LeaseHeartbeat {
  assertOwned(): void;
  stop(): void;
}

/** Keep an inter-process lease alive across awaited effects and fail closed. */
export function startLeaseHeartbeat(
  renew: () => boolean,
  delay: (milliseconds: number) => Promise<void>,
  ttlMilliseconds: number,
  label: string,
): LeaseHeartbeat {
  let stopped = false;
  let lost = false;
  void (async () => {
    try {
      while (!stopped && !lost) {
        await delay(Math.max(1, Math.floor(ttlMilliseconds / 3)));
        if (!stopped && !renew()) lost = true;
      }
    } catch {
      lost = true;
    }
  })();
  return {
    assertOwned(): void {
      if (!lost && renew()) return;
      lost = true;
      throw new Error(`${label} lost its lock`);
    },
    stop(): void {
      stopped = true;
    },
  };
}

export type CreateProgress<Phase extends string> = (
  phase: Phase,
  message?: string,
) => void;

/** Typed durable checkpoints shared by the local-create transaction steps. */
export class DurableCreateTransaction<
  Phase extends string,
  Journal extends { phase: Phase },
> {
  constructor(
    readonly journal: Journal,
    private readonly save: (journal: Journal) => Promise<boolean>,
    private readonly progress: CreateProgress<Phase>,
  ) {}

  async checkpoint(
    phase: Phase,
    error: string,
    mutate?: (journal: Journal) => void,
    message?: string,
  ): Promise<void> {
    mutate?.(this.journal);
    this.journal.phase = phase;
    this.progress(phase, message);
    if (!(await this.save(this.journal))) throw new Error(error);
  }
}
