import type { StorageInfo } from "../types/flipper";

export interface DeviceTelemetrySnapshot {
  connectionKey: string | null;
  power: Record<string, string> | null;
  storage: StorageInfo | null;
  internalBytes: number | null;
  latency: number | null;
  deviceInfo: Record<string, string> | null;
  refreshedAt: number | null;
  loading: boolean;
  errors: {
    power: string | null;
    storage: string | null;
    internal: string | null;
    latency: string | null;
    deviceInfo: string | null;
  };
}

export interface DeviceTelemetryReaders {
  powerInfo(): Promise<Record<string, string>>;
  storageInfo(path: string): Promise<StorageInfo>;
  storageDu(path: string): Promise<number>;
  ping(): Promise<number>;
  deviceInfoAll(): Promise<Record<string, string>>;
}

export interface TelemetryScheduler {
  setInterval(callback: () => void, delayMs: number): unknown;
  clearInterval(timer: unknown): void;
  now(): number;
}

export interface VisibilitySource {
  isVisible(): boolean;
  subscribe(callback: (visible: boolean) => void): () => void;
}

export interface DeviceTelemetryOptions {
  slowIntervalMs?: number;
  pingIntervalMs?: number;
}

const EMPTY_ERRORS: DeviceTelemetrySnapshot["errors"] = {
  power: null,
  storage: null,
  internal: null,
  latency: null,
  deviceInfo: null,
};

function emptySnapshot(connectionKey: string | null): DeviceTelemetrySnapshot {
  return {
    connectionKey,
    power: null,
    storage: null,
    internalBytes: null,
    latency: null,
    deviceInfo: null,
    refreshedAt: null,
    loading: false,
    errors: { ...EMPTY_ERRORS },
  };
}

/**
 * One bounded polling owner for all low-frequency device telemetry.
 *
 * The service never queues more than one refresh and one ping. Hiding the
 * webview stops both timers and invalidates in-flight publication; becoming
 * visible performs one fresh sample before timers resume.
 */
export class DeviceTelemetryService {
  private readonly readers: DeviceTelemetryReaders;
  private readonly scheduler: TelemetryScheduler;
  private readonly listeners = new Set<() => void>();
  private readonly slowIntervalMs: number;
  private readonly pingIntervalMs: number;
  private readonly unsubscribeVisibility: () => void;
  private snapshot = emptySnapshot(null);
  private visible: boolean;
  private generation = 0;
  private slowTimer: unknown = null;
  private pingTimer: unknown = null;
  private drainPromise: Promise<void> | null = null;
  private refreshPending = false;
  private detailsPending = false;
  private pingPending = false;

  constructor(
    readers: DeviceTelemetryReaders,
    scheduler: TelemetryScheduler,
    visibility: VisibilitySource,
    options: DeviceTelemetryOptions = {},
  ) {
    this.readers = readers;
    this.scheduler = scheduler;
    this.slowIntervalMs = options.slowIntervalMs ?? 30_000;
    this.pingIntervalMs = options.pingIntervalMs ?? 4_000;
    this.visible = visibility.isVisible();
    this.unsubscribeVisibility = visibility.subscribe((visible) => {
      if (this.visible === visible) return;
      this.visible = visible;
      this.generation += 1;
      this.clearTimers();
      if (visible && this.snapshot.connectionKey) this.arm();
    });
  }

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  getSnapshot = (): DeviceTelemetrySnapshot => this.snapshot;

  setConnection(connectionKey: string | null): void {
    if (this.snapshot.connectionKey === connectionKey) return;
    this.generation += 1;
    this.clearTimers();
    this.refreshPending = false;
    this.detailsPending = false;
    this.pingPending = false;
    this.publish(emptySnapshot(connectionKey));
    if (connectionKey && this.visible) this.arm();
  }

  refresh(includeDeviceInfo = false): Promise<void> {
    if (!this.snapshot.connectionKey || !this.visible) return Promise.resolve();
    this.refreshPending = true;
    this.detailsPending ||= includeDeviceInfo;
    return this.drain();
  }

  refreshPing(): Promise<void> {
    if (!this.snapshot.connectionKey || !this.visible) return Promise.resolve();
    this.pingPending = true;
    return this.drain();
  }

  dispose(): void {
    this.generation += 1;
    this.clearTimers();
    this.unsubscribeVisibility();
    this.listeners.clear();
  }

  private arm(): void {
    void this.refresh();
    void this.refreshPing();
    this.slowTimer = this.scheduler.setInterval(
      () => void this.refresh(),
      this.slowIntervalMs,
    );
    this.pingTimer = this.scheduler.setInterval(
      () => void this.refreshPing(),
      this.pingIntervalMs,
    );
  }

  private clearTimers(): void {
    if (this.slowTimer !== null) this.scheduler.clearInterval(this.slowTimer);
    if (this.pingTimer !== null) this.scheduler.clearInterval(this.pingTimer);
    this.slowTimer = null;
    this.pingTimer = null;
  }

  private drain(): Promise<void> {
    if (this.drainPromise) return this.drainPromise;
    this.drainPromise = this.runDrain().finally(() => {
      this.drainPromise = null;
      if (
        this.snapshot.connectionKey &&
        this.visible &&
        (this.refreshPending || this.pingPending)
      ) {
        void this.drain();
      }
    });
    return this.drainPromise;
  }

  private async runDrain(): Promise<void> {
    while (this.snapshot.connectionKey && this.visible) {
      if (this.refreshPending) {
        const includeDeviceInfo = this.detailsPending;
        this.refreshPending = false;
        this.detailsPending = false;
        await this.runRefresh(includeDeviceInfo);
        continue;
      }
      if (this.pingPending) {
        this.pingPending = false;
        await this.runPing();
        continue;
      }
      break;
    }
  }

  private async runRefresh(includeDeviceInfo: boolean): Promise<void> {
    const generation = this.generation;
    const base = this.snapshot;
    this.publish({ ...base, loading: true });

    const errors = { ...base.errors };
    const power = await this.read("power", () => this.readers.powerInfo(), errors);
    if (!this.isCurrent(generation)) return;
    const storage = await this.read(
      "storage",
      () => this.readers.storageInfo("/ext"),
      errors,
    );
    if (!this.isCurrent(generation)) return;
    const internalBytes = await this.read(
      "internal",
      () => this.readers.storageDu("/int"),
      errors,
    );
    if (!this.isCurrent(generation)) return;
    const deviceInfo = includeDeviceInfo
      ? await this.read("deviceInfo", () => this.readers.deviceInfoAll(), errors)
      : base.deviceInfo;
    if (!this.isCurrent(generation)) return;

    this.publish({
      ...this.snapshot,
      power,
      storage,
      internalBytes,
      deviceInfo,
      refreshedAt: this.scheduler.now(),
      loading: false,
      errors,
    });
  }

  private async runPing(): Promise<void> {
    const generation = this.generation;
    const errors = { ...this.snapshot.errors };
    const latency = await this.read("latency", () => this.readers.ping(), errors);
    if (!this.isCurrent(generation)) return;
    this.publish({ ...this.snapshot, latency, errors });
  }

  private async read<K extends keyof DeviceTelemetrySnapshot["errors"], T>(
    key: K,
    operation: () => Promise<T>,
    errors: DeviceTelemetrySnapshot["errors"],
  ): Promise<T | null> {
    try {
      const value = await operation();
      errors[key] = null;
      return value;
    } catch (error) {
      errors[key] = errorMessage(error);
      return null;
    }
  }

  private isCurrent(generation: number): boolean {
    return generation === this.generation && this.visible && !!this.snapshot.connectionKey;
  }

  private publish(snapshot: DeviceTelemetrySnapshot): void {
    this.snapshot = snapshot;
    for (const listener of this.listeners) listener();
  }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
