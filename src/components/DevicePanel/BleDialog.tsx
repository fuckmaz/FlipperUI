import { useEffect, useRef, useState } from "react";
import { Bluetooth, RefreshCw } from "lucide-react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  startBleScan,
  stopBleScan,
  connectBleDevice,
  type BleDevice,
} from "../../lib/tauri";
import { Spinner } from "../ui/Spinner";
import { useFlipperStore } from "../../store/useFlipperStore";
import { updateSettings } from "../../lib/settings";
import { syncClockOnConnectIfEnabled } from "../../lib/clockSync";

const IS_MACOS =
  typeof navigator !== "undefined" && /Mac/i.test(navigator.platform);

/** After this long with nothing found, show the usual suspects. */
const SLOW_SCAN_HINT_MS = 15_000;

interface BleDialogProps {
  onClose: () => void;
}

/** Map RSSI (in dBm) to a fill count out of 4. Anything ≥ -55 dBm is
 * effectively "right next to the antenna"; below -90 dBm is unusable. */
function rssiToBars(rssi: number | null): 0 | 1 | 2 | 3 | 4 {
  if (rssi == null) return 0;
  if (rssi >= -55) return 4;
  if (rssi >= -65) return 3;
  if (rssi >= -75) return 2;
  if (rssi >= -85) return 1;
  return 0;
}

/** Tier-coloured WiFi-style vertical bars (4 ascending heights). Unfilled
 * bars stay dim so the indicator reads even at zero signal. When RSSI is
 * unknown, every bar is dim and no colour is applied. */
function SignalBars({ rssi }: { rssi: number | null }) {
  const filled = rssiToBars(rssi);
  const tone =
    rssi == null
      ? "var(--color-dim)"
      : filled >= 4
        ? "var(--color-success)"
        : filled >= 3
          ? "var(--color-accent)"
          : filled >= 2
            ? "var(--color-warning)"
            : "var(--color-danger)";

  // Bar geometry: 4 bars, each 3px wide, 1px gap, heights 4/7/10/13, baseline 14.
  const bars = [
    { x: 0, h: 4 },
    { x: 4, h: 7 },
    { x: 8, h: 10 },
    { x: 12, h: 13 },
  ];
  return (
    <svg
      width={15}
      height={14}
      viewBox="0 0 15 14"
      aria-hidden
      className="shrink-0"
    >
      {bars.map((b, i) => (
        <rect
          key={i}
          x={b.x}
          y={14 - b.h}
          width={3}
          height={b.h}
          rx={0.5}
          fill={i < filled ? tone : "var(--color-elevated)"}
        />
      ))}
    </svg>
  );
}

function sortDevices(list: BleDevice[]): BleDevice[] {
  return [...list].sort((a, b) => {
    if (a.paired !== b.paired) return a.paired ? -1 : 1;
    return (b.rssi ?? -999) - (a.rssi ?? -999);
  });
}

export function BleDialog({ onClose }: BleDialogProps) {
  const [devices, setDevices] = useState<BleDevice[]>([]);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Why the scan can't find anything (Bluetooth off, no permission, no
  // adapter) — sent by the backend, cleared when it recovers.
  const [problem, setProblem] = useState<string | null>(null);
  const [slow, setSlow] = useState(false);
  const [connectingId, setConnectingId] = useState<string | null>(null);

  const setConnecting = useFlipperStore((s) => s.setConnecting);
  const setConnected = useFlipperStore((s) => s.setConnected);
  const setStoreError = useFlipperStore((s) => s.setError);

  // Holds the most recent map of devices keyed by id, so the listener can
  // merge updates without depending on a setState closure (which would race
  // when many ble-scan-device events fire in quick succession).
  const devicesRef = useRef<Map<string, BleDevice>>(new Map());

  const start = async () => {
    setError(null);
    setProblem(null);
    setSlow(false);
    setScanning(true);
    try {
      await startBleScan();
    } catch (e) {
      setError(String(e));
      setScanning(false);
    }
  };

  const stop = async () => {
    try {
      await stopBleScan();
    } catch {
      // Ignore — backend will emit ble-scan-stopped regardless
    }
  };

  const reset = () => {
    devicesRef.current = new Map();
    setDevices([]);
  };

  // Subscribe to live discovery events, kick off the scan, and tear everything down on unmount.
  useEffect(() => {
    let unlistenDevice: UnlistenFn | null = null;
    let unlistenStopped: UnlistenFn | null = null;
    let unlistenProblem: UnlistenFn | null = null;
    let cancelled = false;

    (async () => {
      const u1 = await listen<BleDevice>("ble-scan-device", (e) => {
        const d = e.payload;
        const map = devicesRef.current;
        const existing = map.get(d.id);
        // Merge so a stale "no RSSI" update doesn't blow away a fresh value.
        const merged: BleDevice = existing
          ? {
              ...existing,
              ...d,
              rssi: d.rssi ?? existing.rssi,
              paired: existing.paired || d.paired,
            }
          : d;
        map.set(d.id, merged);
        setDevices(sortDevices(Array.from(map.values())));
      });
      if (cancelled) { u1(); return; }
      unlistenDevice = u1;

      const u2 = await listen("ble-scan-stopped", () => {
        setScanning(false);
      });
      if (cancelled) { u2(); return; }
      unlistenStopped = u2;

      const u3 = await listen<string | null>("ble-scan-problem", (e) => {
        setProblem(e.payload);
      });
      if (cancelled) { u3(); return; }
      unlistenProblem = u3;

      if (!cancelled) await start();
    })();

    return () => {
      cancelled = true;
      unlistenDevice?.();
      unlistenStopped?.();
      unlistenProblem?.();
      // Best-effort: stop the backend scan when the dialog closes.
      void stopBleScan().catch(() => {});
    };
  }, []);

  useEffect(() => {
    if (!scanning || devices.length > 0 || problem) return;
    const t = window.setTimeout(() => setSlow(true), SLOW_SCAN_HINT_MS);
    return () => window.clearTimeout(t);
  }, [scanning, devices.length, problem]);

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  const handleConnect = async (device: BleDevice) => {
    // Connecting needs exclusive use of the BLE adapter — pause discovery.
    await stop();
    setConnectingId(device.id);
    setConnecting(true);
    setStoreError(null);
    try {
      const info = await connectBleDevice(device.id, device.name);
      let clockError: string | null = null;
      try {
        await syncClockOnConnectIfEnabled();
      } catch (e) {
        clockError = `Clock sync failed: ${e instanceof Error ? e.message : String(e)}`;
      }
      setConnected(info, "ble");
      if (clockError) setStoreError(clockError);
      void updateSettings({
        connection: { lastBleId: device.id, lastBleName: device.name },
      }).catch(() => {});
      onClose();
    } catch (e) {
      const msg = String(e);
      setError(msg);
      setStoreError(msg);
      setConnecting(false);
      setConnectingId(null);
    }
  };

  const handleRescan = async () => {
    await stop();
    reset();
    await start();
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60"
      onClick={onClose}
    >
      <div
        className="bg-panel border border-border-subtle rounded-lg shadow-2xl p-5 max-w-md w-full mx-4"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between mb-3">
          <h3 className="text-sm font-semibold text-primary flex items-center gap-2">
            <Bluetooth size={14} className="text-accent" />
            Connect via BLE
          </h3>
          {scanning ? (
            <span
              className="flex items-center gap-1.5 text-xs text-secondary px-2 py-1"
              role="status"
              aria-live="polite"
            >
              <Spinner size={12} />
              <span>Scanning…</span>
            </span>
          ) : (
            <button
              onClick={handleRescan}
              disabled={connectingId !== null}
              className="flex items-center gap-1.5 text-xs text-secondary hover:text-primary disabled:opacity-40 px-2 py-1 rounded transition-colors"
              title="Rescan"
            >
              <RefreshCw size={12} />
              <span>Rescan</span>
            </button>
          )}
        </div>

        <p className="text-xs text-secondary mb-3">
          {IS_MACOS
            ? "No need to pair in System Settings first: on the first connect the Flipper shows a PIN, enter it in the macOS prompt."
            : "Pair the Flipper in your OS Bluetooth settings first."}{" "}
          BLE supports files, screen, and apps — but not CLI. Devices appear as
          they're discovered; the scan runs until you connect or close the
          dialog.
        </p>

        {problem && (
          <div
            role="alert"
            className="text-xs text-warning bg-warning/10 border border-warning/30 rounded px-2 py-1.5 mb-3"
          >
            {problem}
          </div>
        )}

        {error && (
          <div className="text-xs text-danger bg-danger/10 border border-danger/30 rounded px-2 py-1.5 mb-3">
            {error}
          </div>
        )}

        <div className="max-h-64 overflow-y-auto rounded border border-border-subtle divide-y divide-border-subtle">
          {scanning && devices.length === 0 && !problem && (
            <div className="p-3 text-xs text-secondary">
              <div className="flex items-center gap-2">
                <Spinner size={13} />
                Looking for Flippers…
              </div>
              {slow && (
                <p className="mt-2 text-muted">
                  Still nothing? Make sure Bluetooth is on on the Flipper
                  (Settings → Bluetooth), the screen is awake, and it isn't
                  connected to your phone — a connected Flipper stops
                  advertising.
                </p>
              )}
            </div>
          )}
          {!scanning && devices.length === 0 && !error && !problem && (
            <div className="p-3 text-xs text-muted">
              No Flipper devices found. Make sure Bluetooth is enabled on the
              Flipper (Settings → Bluetooth)
              {IS_MACOS
                ? " and that it isn't connected to your phone."
                : " and that it has been paired in your OS Bluetooth settings."}
            </div>
          )}
          {devices.map((d) => {
            const busy = connectingId === d.id;
            return (
              <button
                key={d.id}
                onClick={() => handleConnect(d)}
                disabled={connectingId !== null}
                className="w-full flex items-center gap-3 p-2.5 text-left hover:bg-elevated disabled:opacity-40 disabled:cursor-not-allowed transition-colors"
              >
                <Bluetooth
                  size={14}
                  className={d.paired ? "text-success" : "text-muted"}
                />
                <div className="flex-1 min-w-0">
                  <div className="text-sm text-primary truncate">{d.name}</div>
                  <div className="text-[11px] text-muted truncate">
                    {d.paired ? "paired" : "not paired"}
                  </div>
                </div>
                <span
                  className="flex items-center gap-1.5 tabular-nums"
                  title={d.rssi != null ? `RSSI ${d.rssi} dBm` : "no RSSI"}
                >
                  <SignalBars rssi={d.rssi} />
                  <span className="text-[10px] text-muted">
                    {d.rssi != null ? `${d.rssi} dBm` : "—"}
                  </span>
                </span>
                {busy && <Spinner size={13} />}
              </button>
            );
          })}
        </div>

        <div className="flex justify-end mt-4">
          <button
            onClick={onClose}
            className="px-3 py-1.5 text-xs rounded bg-surface text-secondary hover:text-primary hover:bg-elevated transition-colors"
          >
            Close
          </button>
        </div>
      </div>
    </div>
  );
}
