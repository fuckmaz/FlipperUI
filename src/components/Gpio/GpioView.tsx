import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { useFlipperStore } from "../../store/useFlipperStore";
import {
  gpioPulsePin,
  gpioReadPin,
  gpioSetMode,
  gpioSetOtg,
  gpioSetPull,
  gpioSnapshot,
  gpioWritePin,
} from "../../lib/tauri";
import {
  HEADER_PINS,
  RPC_GPIO_PINS,
  type GpioMode,
  type GpioObservedMode,
  type GpioPinName,
  type GpioPull,
  type HeaderPin,
} from "../../types/gpio";
import { ConfirmDialog } from "../ui/ConfirmDialog";
import flipperOutlineUrl from "../../assets/flipper-outline.svg";
import { GpioTopBar } from "./GpioTopBar";
import { PinColumn } from "./PinColumn";
import { InfoPinDetail, OtgPinDetail, RpcPinDetail } from "./PinDetail";

const SAMPLE_HISTORY = 50;
const DEFAULT_POLL_MS = 250;
const BLE_POLL_FLOOR_MS = 500;

/** Per-pin RPC state mirrored from `gpio_snapshot` and live edits. */
interface RpcPinUiState {
  mode: GpioObservedMode;
  value: 0 | 1 | null;
  /** Frontend-only: last pull value we sent. Defaults to "no". */
  pull: GpioPull;
  /** Whether the user has enabled Watch mode on this pin. */
  watching: boolean;
  /** Last action label rendered in the detail footer. */
  lastAction: string | null;
  lastActionAt: number | null;
}

type RpcPinMap = Record<GpioPinName, RpcPinUiState>;

const emptyPinMap = (): RpcPinMap =>
  Object.fromEntries(
    RPC_GPIO_PINS.map((pin) => [
      pin,
      {
        mode: "other" as GpioObservedMode,
        value: null,
        pull: "no" as GpioPull,
        watching: false,
        lastAction: null,
        lastActionAt: null,
      },
    ]),
  ) as RpcPinMap;

/**
 * Main GPIO view.
 *
 * Layout:
 *   ┌──────────── top bar (OTG chip · poll slider · reset · refresh) ────────────┐
 *   ├───────────────── pin column ─────────────────┬────── detail card ──────────┤
 *   │  18 header pins, click to select             │  per-pin controls           │
 *   └──────────────────────────────────────────────┴─────────────────────────────┘
 *
 * State lives entirely in this component — the Zustand store is only used to
 * read connection status. Sample history for sparklines is held in a ref so
 * mid-interval reads don't trigger a re-render unless the value actually
 * changed.
 */
export function GpioView() {
  const isConnected = useFlipperStore((s) => s.isConnected);
  const connectionKind = useFlipperStore((s) => s.connectionKind);

  const [pins, setPins] = useState<RpcPinMap>(emptyPinMap);
  const [otg, setOtg] = useState<boolean>(false);
  const [selectedPin, setSelectedPin] = useState<number>(2); // PA7
  const [busy, setBusy] = useState<boolean>(false);
  const [error, setError] = useState<string | null>(null);
  const [confirmingReset, setConfirmingReset] = useState<boolean>(false);
  const [pollIntervalMs, setPollIntervalMs] = useState<number>(DEFAULT_POLL_MS);

  // Per-pin sample history — kept in a ref so we can append without
  // re-rendering the entire pane on every poll. We only setState when the
  // value actually changes.
  const samplesRef = useRef<Record<GpioPinName, Array<0 | 1>>>(
    Object.fromEntries(RPC_GPIO_PINS.map((p) => [p, [] as Array<0 | 1>])) as Record<
      GpioPinName,
      Array<0 | 1>
    >,
  );
  // Mounted-flag for sequencing teardown of async loops.
  const mountedRef = useRef(true);
  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  // BLE floor — the slider is clamped to ≥ 500ms when over BLE.
  const minPollMs = connectionKind === "ble" ? BLE_POLL_FLOOR_MS : 100;
  const pollFloorReason =
    connectionKind === "ble"
      ? "BLE bandwidth is limited — polling below 500 ms is disabled to keep the link responsive."
      : null;

  // Clamp the slider value up if the floor moves (e.g. user just connected
  // over BLE while the slider was at 100 ms).
  useEffect(() => {
    if (pollIntervalMs < minPollMs) setPollIntervalMs(minPollMs);
  }, [minPollMs, pollIntervalMs]);

  // ── Snapshot (initial + Refresh) ──────────────────────────────────────────

  const applySnapshot = useCallback(
    (next: Awaited<ReturnType<typeof gpioSnapshot>>) => {
      setPins((prev) => {
        const updated: RpcPinMap = { ...prev };
        for (const p of next.pins) {
          const previous = prev[p.pin];
          updated[p.pin] = {
            ...previous,
            mode: p.mode,
            value: p.value,
          };
          // Seed history with the snapshot value so the sparkline isn't
          // blank on first render.
          samplesRef.current[p.pin] = p.value === null ? [] : [p.value];
        }
        return updated;
      });
      setOtg(next.otg);
    },
    [],
  );

  const refresh = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const snap = await gpioSnapshot();
      if (!mountedRef.current) return;
      applySnapshot(snap);
    } catch (e) {
      if (mountedRef.current) {
        setError(`Snapshot failed: ${errMsg(e)}`);
      }
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  }, [applySnapshot]);

  useEffect(() => {
    if (!isConnected) {
      // Clear everything on disconnect — no stale state when the user
      // reconnects.
      setPins(emptyPinMap());
      samplesRef.current = Object.fromEntries(
        RPC_GPIO_PINS.map((p) => [p, [] as Array<0 | 1>]),
      ) as Record<GpioPinName, Array<0 | 1>>;
      setOtg(false);
      setError(null);
      return;
    }
    void refresh();
  }, [isConnected, refresh]);

  // ── Polling for watched INPUT pins ────────────────────────────────────────

  // Recompute the watched set on every render — cheap, ≤8 entries.
  const watchedPins = useMemo<GpioPinName[]>(
    () =>
      RPC_GPIO_PINS.filter(
        (p) => pins[p].watching && pins[p].mode === "input",
      ),
    [pins],
  );

  // Stash the latest watched-list in a ref so the interval callback always
  // reads it without needing to re-create the timer on each pin toggle.
  const watchedRef = useRef<GpioPinName[]>(watchedPins);
  useEffect(() => {
    watchedRef.current = watchedPins;
  }, [watchedPins]);

  useEffect(() => {
    if (!isConnected) return;
    if (watchedPins.length === 0) return;

    let cancelled = false;
    let busyTick = false;

    const tick = async () => {
      if (cancelled || busyTick) return;
      busyTick = true;
      try {
        for (const pin of watchedRef.current) {
          if (cancelled) break;
          try {
            const v = await gpioReadPin(pin);
            if (cancelled || !mountedRef.current) break;
            // Append to history regardless (sparkline density), but only
            // trigger a React re-render when the value actually changed.
            const history = samplesRef.current[pin];
            history.push(v);
            if (history.length > SAMPLE_HISTORY) {
              history.splice(0, history.length - SAMPLE_HISTORY);
            }
            setPins((prev) => {
              if (prev[pin].value === v) return prev;
              return {
                ...prev,
                [pin]: { ...prev[pin], value: v },
              };
            });
          } catch (e) {
            if (!cancelled && mountedRef.current) {
              setError(`Read ${pin} failed: ${errMsg(e)}`);
            }
          }
        }
      } finally {
        busyTick = false;
      }
    };

    // Fire one immediately so the user gets a sample without waiting an
    // entire interval, then poll.
    void tick();
    const id = window.setInterval(() => void tick(), pollIntervalMs);
    return () => {
      cancelled = true;
      window.clearInterval(id);
    };
  }, [isConnected, watchedPins, pollIntervalMs]);

  // ── Mutation helpers ──────────────────────────────────────────────────────

  const markAction = useCallback(
    (pin: GpioPinName, label: string) => {
      setPins((prev) => ({
        ...prev,
        [pin]: { ...prev[pin], lastAction: label, lastActionAt: Date.now() },
      }));
    },
    [],
  );

  const handleSetMode = useCallback(
    async (pin: GpioPinName, mode: GpioMode) => {
      setBusy(true);
      setError(null);
      try {
        await gpioSetMode(pin, mode);
        if (!mountedRef.current) return;
        // Firmware drives a newly configured OUTPUT low. INPUT can be sampled
        // immediately, but keep it unknown until that read completes.
        const initialValue: 0 | null = mode === "output" ? 0 : null;
        setPins((prev) => ({
          ...prev,
          [pin]: {
            ...prev[pin],
            mode,
            value: initialValue,
            // Switching to OUTPUT — turn Watch off; switching to INPUT —
            // leave Watch where the user had it.
            watching: mode === "output" ? false : prev[pin].watching,
          },
        }));

        if (mode === "input") {
          try {
            const value = await gpioReadPin(pin);
            if (!mountedRef.current) return;
            setPins((prev) => ({
              ...prev,
              [pin]: { ...prev[pin], value },
            }));
            samplesRef.current[pin] = [value];
          } catch (e) {
            if (mountedRef.current) {
              setError(
                `Set ${pin} to INPUT, but its initial read failed: ${errMsg(e)}`,
              );
            }
          }
        }
        markAction(pin, `Set ${mode.toUpperCase()}`);
      } catch (e) {
        if (mountedRef.current) {
          setError(`Set mode failed: ${errMsg(e)}`);
        }
      } finally {
        if (mountedRef.current) setBusy(false);
      }
    },
    [markAction],
  );

  const handleSetPull = useCallback(
    async (pin: GpioPinName, pull: GpioPull) => {
      setBusy(true);
      setError(null);
      try {
        await gpioSetPull(pin, pull);
        if (!mountedRef.current) return;
        setPins((prev) => ({ ...prev, [pin]: { ...prev[pin], pull } }));
        markAction(
          pin,
          pull === "no"
            ? "Pull: none"
            : pull === "up"
              ? "Pull-up"
              : "Pull-down",
        );
      } catch (e) {
        if (mountedRef.current) {
          setError(`Set pull failed: ${errMsg(e)}`);
        }
      } finally {
        if (mountedRef.current) setBusy(false);
      }
    },
    [markAction],
  );

  const handleWriteValue = useCallback(
    async (pin: GpioPinName, value: 0 | 1) => {
      setBusy(true);
      setError(null);
      try {
        await gpioWritePin(pin, value);
        if (!mountedRef.current) return;
        setPins((prev) => ({ ...prev, [pin]: { ...prev[pin], value } }));
        markAction(pin, `Set OUTPUT ${value === 1 ? "HIGH" : "LOW"}`);
      } catch (e) {
        if (mountedRef.current) {
          setError(`Write ${pin} failed: ${errMsg(e)}`);
        }
      } finally {
        if (mountedRef.current) setBusy(false);
      }
    },
    [markAction],
  );

  const handlePulse = useCallback(
    async (pin: GpioPinName) => {
      setBusy(true);
      setError(null);
      try {
        await gpioPulsePin(pin, 100);
        if (!mountedRef.current) return;
        setPins((prev) => ({ ...prev, [pin]: { ...prev[pin], value: 0 } }));
        markAction(pin, "Pulse 100 ms");
      } catch (e) {
        if (mountedRef.current) {
          setError(`Pulse ${pin} failed: ${errMsg(e)}`);
        }
      } finally {
        if (mountedRef.current) setBusy(false);
      }
    },
    [markAction],
  );

  const handleReadNow = useCallback(
    async (pin: GpioPinName) => {
      setBusy(true);
      setError(null);
      try {
        const v = await gpioReadPin(pin);
        if (!mountedRef.current) return;
        const history = samplesRef.current[pin];
        history.push(v);
        if (history.length > SAMPLE_HISTORY) {
          history.splice(0, history.length - SAMPLE_HISTORY);
        }
        setPins((prev) => ({ ...prev, [pin]: { ...prev[pin], value: v } }));
        markAction(pin, `Read ${v}`);
      } catch (e) {
        if (mountedRef.current) {
          setError(`Read ${pin} failed: ${errMsg(e)}`);
        }
      } finally {
        if (mountedRef.current) setBusy(false);
      }
    },
    [markAction],
  );

  const handleToggleWatch = useCallback((pin: GpioPinName) => {
    setPins((prev) => ({
      ...prev,
      [pin]: { ...prev[pin], watching: !prev[pin].watching },
    }));
  }, []);

  const handleToggleOtg = useCallback(
    async (next: boolean) => {
      setBusy(true);
      setError(null);
      try {
        await gpioSetOtg(next);
        if (!mountedRef.current) return;
        setOtg(next);
      } catch (e) {
        if (mountedRef.current) {
          setError(`OTG ${next ? "on" : "off"} failed: ${errMsg(e)}`);
        }
      } finally {
        if (mountedRef.current) setBusy(false);
      }
    },
    [],
  );

  const handleResetAll = useCallback(async () => {
    setConfirmingReset(false);
    setBusy(true);
    setError(null);
    try {
      // Sequential — same serial mutex on the Rust side, predictable error
      // attribution.
      for (const pin of RPC_GPIO_PINS) {
        await gpioSetMode(pin, "input");
        await gpioSetPull(pin, "no");
      }
      if (!mountedRef.current) return;
      const now = Date.now();
      setPins((prev) => {
        const updated: RpcPinMap = { ...prev };
        for (const pin of RPC_GPIO_PINS) {
          updated[pin] = {
            ...prev[pin],
            mode: "input",
            pull: "no",
            watching: false,
            lastAction: "Reset",
            lastActionAt: now,
          };
        }
        return updated;
      });
      // Re-snapshot so the value column reflects what the now-input pins are
      // floating at.
      const snap = await gpioSnapshot();
      if (!mountedRef.current) return;
      applySnapshot(snap);
    } catch (e) {
      if (mountedRef.current) {
        setError(`Reset failed: ${errMsg(e)}`);
      }
    } finally {
      if (mountedRef.current) setBusy(false);
    }
  }, [applySnapshot]);

  // Memoised view-model for the chip strip — keep this above any early
  // returns so the Hook order stays stable when the connection drops.
  const chips = useMemo(() => {
    const out: Record<
      string,
      { mode: GpioObservedMode; value: 0 | 1 | null } | undefined
    > = {};
    for (const pin of RPC_GPIO_PINS) {
      out[pin] = { mode: pins[pin].mode, value: pins[pin].value };
    }
    return out;
  }, [pins]);

  // ── Render ────────────────────────────────────────────────────────────────

  if (!isConnected) {
    return <DisconnectedEmptyState />;
  }

  const selected =
    HEADER_PINS.find((p) => p.number === selectedPin) ?? HEADER_PINS[1];

  return (
    <div className="flex-1 min-h-0 flex flex-col overflow-hidden">
      <GpioTopBar
        otg={otg}
        busy={busy}
        onToggleOtg={() => void handleToggleOtg(!otg)}
        onResetAll={() => setConfirmingReset(true)}
        onRefresh={() => void refresh()}
        pollIntervalMs={pollIntervalMs}
        onPollIntervalChange={setPollIntervalMs}
        minPollMs={minPollMs}
        pollFloorReason={pollFloorReason}
      />

      {error && (
        <div className="shrink-0 px-3 py-1.5 text-[11px] text-danger bg-danger/10 border-b border-danger/20">
          {error}
        </div>
      )}

      <div className="flex-1 min-h-0 grid grid-cols-[minmax(220px,260px)_1fr] gap-0">
        <aside className="border-r border-border-subtle bg-panel/40 min-h-0 overflow-hidden">
          <PinColumn
            selectedPin={selectedPin}
            onSelect={setSelectedPin}
            chips={chips}
            connected={isConnected}
            otg={otg}
          />
        </aside>
        <main className="min-h-0 overflow-y-auto">
          <div className="max-w-2xl mx-auto px-4 py-5">
            <DetailPaneSwitch
              pin={selected}
              pins={pins}
              otg={otg}
              busy={busy}
              samplesRef={samplesRef}
              onSetMode={handleSetMode}
              onSetPull={handleSetPull}
              onToggleWatch={handleToggleWatch}
              onReadNow={handleReadNow}
              onWriteValue={handleWriteValue}
              onPulse={handlePulse}
              onToggleOtg={handleToggleOtg}
            />
          </div>
        </main>
      </div>

      {confirmingReset && (
        <ConfirmDialog
          title="Reset all GPIO pins?"
          message="All eight controllable pins will be set to INPUT with no pull. Any drives in progress will stop."
          confirmLabel="Reset"
          cancelLabel="Cancel"
          destructive
          onConfirm={() => void handleResetAll()}
          onCancel={() => setConfirmingReset(false)}
        />
      )}
    </div>
  );
}

// ── Detail pane router ───────────────────────────────────────────────────────

function DetailPaneSwitch({
  pin,
  pins,
  otg,
  busy,
  samplesRef,
  onSetMode,
  onSetPull,
  onToggleWatch,
  onReadNow,
  onWriteValue,
  onPulse,
  onToggleOtg,
}: {
  pin: HeaderPin;
  pins: RpcPinMap;
  otg: boolean;
  busy: boolean;
  samplesRef: React.MutableRefObject<Record<GpioPinName, Array<0 | 1>>>;
  onSetMode: (pin: GpioPinName, mode: GpioMode) => void;
  onSetPull: (pin: GpioPinName, pull: GpioPull) => void;
  onToggleWatch: (pin: GpioPinName) => void;
  onReadNow: (pin: GpioPinName) => void;
  onWriteValue: (pin: GpioPinName, value: 0 | 1) => void;
  onPulse: (pin: GpioPinName) => void;
  onToggleOtg: (on: boolean) => void;
}) {
  // Pin 1 — +5V OTG: its own dedicated card.
  if (pin.number === 1) {
    return (
      <OtgPinDetail
        pin={pin}
        otg={otg}
        busy={busy}
        // No per-pin action log for OTG — keep the footer empty.
        lastAction={null}
        lastActionAt={null}
        onToggleOtg={onToggleOtg}
      />
    );
  }

  // RPC pin — full controls.
  if (pin.kind === "gpio") {
    const name = pin.name as GpioPinName;
    const state = pins[name];
    return (
      <RpcPinDetail
        pin={pin}
        mode={state.mode}
        value={state.value}
        pull={state.pull}
        watching={state.watching}
        samples={samplesRef.current[name]}
        lastAction={state.lastAction}
        lastActionAt={state.lastActionAt}
        busy={busy}
        onSetMode={(m) => onSetMode(name, m)}
        onSetPull={(p) => onSetPull(name, p)}
        onToggleWatch={() => onToggleWatch(name)}
        onReadNow={() => onReadNow(name)}
        onWriteValue={(v) => onWriteValue(name, v)}
        onPulse={() => onPulse(name)}
      />
    );
  }

  // Everything else — power, ground, debug, i2c, 1wire.
  return <InfoPinDetail pin={pin} />;
}

// ── Disconnected empty state ─────────────────────────────────────────────────

function DisconnectedEmptyState() {
  return (
    <div className="flex-1 flex flex-col items-center justify-center gap-4 text-dim">
      <div
        aria-hidden
        className="text-elevated"
        style={{
          width: 240,
          height: 178,
          backgroundColor: "currentColor",
          WebkitMaskImage: `url(${flipperOutlineUrl})`,
          maskImage: `url(${flipperOutlineUrl})`,
          WebkitMaskRepeat: "no-repeat",
          maskRepeat: "no-repeat",
          WebkitMaskPosition: "center",
          maskPosition: "center",
          WebkitMaskSize: "contain",
          maskSize: "contain",
        }}
      />
      <p className="text-sm">Connect to your Flipper to use GPIO</p>
    </div>
  );
}

// ── Helpers ──────────────────────────────────────────────────────────────────

function errMsg(e: unknown): string {
  if (e instanceof Error) return e.message;
  if (typeof e === "string") return e;
  return String(e);
}
