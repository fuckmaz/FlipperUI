import { useEffect, useRef, useState } from "react";
import { Usb, Power, BatteryLow, BatteryMedium, BatteryFull, BatteryWarning, Zap, HardDrive, Bluetooth, Signal, SignalLow, SignalMedium, SignalHigh, Link as LinkIcon, Unlink } from "lucide-react";
import { invoke } from "@tauri-apps/api/core";
import { disconnect, listPorts, reboot } from "../../lib/tauri";
import { useFlipperStore } from "../../store/useFlipperStore";
import { useDeviceTelemetry } from "../../lib/deviceTelemetry";
import {
  connectPreferredUsbDevice,
  connectSelectedUsbDevice,
  migrateLegacyUsbPort,
} from "../../lib/usbConnection";
import { loadSettings, subscribeSettings, updateSettings } from "../../lib/settings";
import { syncClockOnConnectIfEnabled } from "../../lib/clockSync";
import { Spinner } from "../ui/Spinner";
import { ConfirmDialog } from "../ui/ConfirmDialog";
import { BleDialog } from "./BleDialog";
import { GlobalSearch } from "../GlobalSearch/GlobalSearch";

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}

export function DevicePanel() {
  const ports = useFlipperStore((s) => s.ports);
  const selectedPort = useFlipperStore((s) => s.selectedPort);
  const deviceInfo = useFlipperStore((s) => s.deviceInfo);
  const isConnected = useFlipperStore((s) => s.isConnected);
  const isConnecting = useFlipperStore((s) => s.isConnecting);
  const setPorts = useFlipperStore((s) => s.setPorts);
  const setSelectedPort = useFlipperStore((s) => s.setSelectedPort);
  const setConnecting = useFlipperStore((s) => s.setConnecting);
  const setConnected = useFlipperStore((s) => s.setConnected);
  const setError = useFlipperStore((s) => s.setError);

  const telemetry = useDeviceTelemetry();
  const batteryCharge =
    telemetry.power?.["charge_level"] ?? telemetry.power?.["charge"] ?? null;
  const currentMa = Number(
    telemetry.power?.["battery_current"] ??
      telemetry.power?.["current_gauge"] ??
      telemetry.power?.["current"] ??
      "0",
  );
  const batteryCharging =
    telemetry.power?.["charging"] === "true" ||
    (Number.isFinite(currentMa) && currentMa > 5);
  const sdTotal = telemetry.storage?.total_space ?? null;
  const sdFree = telemetry.storage?.free_space ?? null;
  const latency = telemetry.latency;

  // Track whether the user manually disconnected — suppresses auto-connect
  // until the device is physically unplugged and re-plugged.
  const [userDisconnected, setUserDisconnected] = useState(false);
  // Per-port cooldown for failed USB auto-connects. Prevents the 2s port-poll
  // from re-firing connect() against a port that just rejected us, which would
  // produce a connecting → fail → connecting flicker loop. The user can still
  // trigger a manual connect from the DevicePanel button (handleConnect bypasses
  // this map). Cleared when the port disappears so a re-plug retries cleanly.
  const failedConnectRef = useRef<Map<string, number>>(new Map());
  const FAILED_CONNECT_COOLDOWN_MS = 15_000;
  const [showRebootConfirm, setShowRebootConfirm] = useState(false);
  const [showBleDialog, setShowBleDialog] = useState(false);
  const [transport, setTransport] = useState<"usb" | "ble">("usb");
  // Hydrate transport + last-used port from persisted settings on first mount.
  // Until this resolves, settings-driven port auto-select is suppressed so the
  // poll loop can't snap to an arbitrary first port before we know the
  // preferred one.
  const [settingsHydrated, setSettingsHydrated] = useState(false);
  const [autoReconnect, setAutoReconnect] = useState(false);
  const legacyLastPortRef = useRef<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    loadSettings()
      .then(async (s) => {
        if (cancelled) return;
        setTransport(s.connection.transport);
        setAutoReconnect(s.connection.autoReconnect);
        legacyLastPortRef.current = s.connection.lastPort;
        const preferredPort = await migrateLegacyUsbPort(s.connection.lastPort);
        if (preferredPort && !cancelled) {
          const state = useFlipperStore.getState();
          if (!state.selectedPort) setSelectedPort(preferredPort);
        }
      })
      .catch(() => {})
      .finally(() => {
        if (!cancelled) setSettingsHydrated(true);
      });
    return () => {
      cancelled = true;
    };
  }, [setSelectedPort]);

  // Keep autoReconnect in sync with persisted settings — SettingsPane flips it.
  useEffect(() => {
    return subscribeSettings((s) => {
      setAutoReconnect(s.connection.autoReconnect);
    });
  }, []);

  const handleTransportChange = (next: "usb" | "ble") => {
    setTransport(next);
    void updateSettings({ connection: { transport: next } }).catch(() => {});
  };

  const handleSelectPort = (port: string | null) => {
    setSelectedPort(port);
    void updateSettings({ connection: { lastPort: port } }).catch(() => {});
  };

  const maybeSyncClockAfterConnect = async (): Promise<string | null> => {
    try {
      await syncClockOnConnectIfEnabled();
      return null;
    } catch (e) {
      return `Clock sync failed: ${e instanceof Error ? e.message : String(e)}`;
    }
  };

  // Poll for port changes every 2 seconds + auto-connect
  useEffect(() => {
    if (!settingsHydrated) return;
    const poll = async () => {
      try {
        const p = await listPorts();
        setPorts(p);

        const state = useFlipperStore.getState();
        const flipper = p.find((x) => x.is_flipper);

        // Auto-select first Flipper port if none selected. The persisted
        // lastPort has already been applied in the hydrate effect, so if
        // selectedPort is still null at this point either there was no stored
        // port or the stored port isn't currently present.
        if (!state.selectedPort && flipper) {
          setSelectedPort(flipper.name);
        }

        // Clear userDisconnected when no Flipper ports are present
        // (device was physically unplugged)
        if (!flipper && userDisconnected) {
          setUserDisconnected(false);
        }

        // Drop cooldown entries for ports that have disappeared — once the
        // port re-enumerates (re-plug or driver re-bind), auto-connect should
        // try again immediately instead of waiting out the old cooldown.
        const presentNames = new Set(p.map((x) => x.name));
        for (const name of Array.from(failedConnectRef.current.keys())) {
          if (!presentNames.has(name)) failedConnectRef.current.delete(name);
        }

        // Auto-connect: Flipper detected, not connected, not connecting,
        // user hasn't manually disconnected, USB transport selected, the
        // user has opted in via settings, and the target port isn't in
        // cooldown from a recent failed attempt.
        if (autoReconnect && transport === "usb" && flipper && !state.isConnected && !state.isConnecting && !userDisconnected) {
          const candidates = p.filter((candidate) => {
            const lastFailedAt = failedConnectRef.current.get(candidate.name) ?? 0;
            return Date.now() - lastFailedAt >= FAILED_CONNECT_COOLDOWN_MS;
          });
          if (candidates.some((candidate) => candidate.is_flipper)) {
            setConnecting(true);
            setError(null);
            try {
              const info = await connectPreferredUsbDevice(
                candidates,
                legacyLastPortRef.current,
              );
              const clockError = await maybeSyncClockAfterConnect();
              setSelectedPort(info.port);
              legacyLastPortRef.current = info.port;
              void updateSettings({ connection: { lastPort: info.port } }).catch(() => {});
              setConnected(info, "serial");
              failedConnectRef.current.delete(info.port);
              if (clockError) setError(clockError);
            } catch (err) {
              setConnecting(false);
              for (const candidate of candidates) {
                if (candidate.is_flipper) {
                  failedConnectRef.current.set(candidate.name, Date.now());
                }
              }
              setError(
                `Auto-connect to the saved Flipper failed${err instanceof Error && err.message ? `: ${err.message}` : ""} — will retry in ${Math.round(FAILED_CONNECT_COOLDOWN_MS / 1000)}s`,
              );
            }
          }
        }

        // Detect disconnection: USB port disappeared while connected via serial.
        // BLE sessions are torn down by the backend (flipper-disconnected event),
        // so the port-presence check must not run when the active transport
        // isn't serial.
        if (
          state.isConnected &&
          state.connectionKind === "serial" &&
          state.selectedPort
        ) {
          const stillPresent = p.some((x) => x.name === state.selectedPort);
          if (!stillPresent) {
            try { await disconnect(); } catch { /* ignore */ }
            setConnected(null);
            // userDisconnected stays false so we auto-reconnect when it returns
          }
        }
      } catch {
        // Ignore poll errors
      }
    };
    poll();
    const id = setInterval(poll, 2000);
    return () => clearInterval(id);
  }, [setPorts, setSelectedPort, setConnecting, setConnected, setError, userDisconnected, transport, settingsHydrated, autoReconnect]);

  // Mirror device state into the system-tray flyout menu so users see
  // connection status + battery without opening the window. The tray rebuilds
  // its menu on every push, so we only call when one of the inputs changes.
  useEffect(() => {
    const charge =
      batteryCharge != null && Number.isFinite(Number(batteryCharge))
        ? Math.max(0, Math.min(100, Math.round(Number(batteryCharge))))
        : null;
    void invoke("update_tray_status", {
      status: {
        connected: isConnected,
        deviceName: deviceInfo?.hardware_name ?? null,
        firmwareVersion: deviceInfo?.firmware_version ?? null,
        batteryCharge: charge,
        batteryCharging,
      },
    }).catch(() => {});
  }, [isConnected, deviceInfo, batteryCharge, batteryCharging]);

  // Successful connect clears the manual-disconnect flag, so the next
  // unexpected drop is eligible for auto-reconnect again.
  useEffect(() => {
    if (isConnected && userDisconnected) setUserDisconnected(false);
  }, [isConnected, userDisconnected]);

  // BLE/USB auto-reconnect on `flipper-disconnected` is handled centrally in
  // App.tsx — having a second reconnect chain here would race the App-level one
  // for the BLE adapter on every drop.

  const handleConnect = async () => {
    if (!selectedPort) return;
    // Manual connect always overrides any auto-connect cooldown — the user is
    // explicitly asking to retry, so reset both the failed-port map and the
    // userDisconnected flag.
    failedConnectRef.current.delete(selectedPort);
    setUserDisconnected(false);
    setConnecting(true);
    setError(null);
    try {
      const info = await connectSelectedUsbDevice(selectedPort);
      const clockError = await maybeSyncClockAfterConnect();
      legacyLastPortRef.current = info.port;
      void updateSettings({ connection: { lastPort: info.port } }).catch(() => {});
      setConnected(info, "serial");
      if (clockError) setError(clockError);
    } catch (e: unknown) {
      setError(String(e));
      setConnecting(false);
      failedConnectRef.current.set(selectedPort, Date.now());
    }
  };

  const handleDisconnect = async () => {
    setUserDisconnected(true); // suppress auto-connect until re-plug
    try {
      await disconnect();
    } catch {
      // Ignore disconnect errors
    }
    setConnected(null);
  };

  const handleReboot = async () => {
    // userDisconnected stays false so we auto-reconnect after reboot
    try {
      await reboot(0); // 0 = normal OS reboot
    } catch {
      // Expected — device disconnects immediately
    }
    setConnected(null);
    setShowRebootConfirm(false);
  };

  const sdUsedPct =
    sdTotal && sdFree != null
      ? Math.round(((sdTotal - sdFree) / sdTotal) * 100)
      : null;

  return (
    <div className="flex items-center gap-3 px-4 py-2.5 bg-panel border-b border-flipper shrink-0">
      {/* Title — the "UI" orange is the FlipperUI brand mark, intentionally
          pinned to Flipper Zero orange regardless of the theme accent. */}
      <span className="font-semibold text-sm text-white">
        Flipper<span style={{ color: "#ff8300" }}>UI</span>
      </span>

      <div className="w-px h-4 bg-elevated mx-1" />

      {/* Transport toggle (USB / BLE) */}
      <div className="flex items-center gap-2 select-none">
        <Usb
          className={`w-3.5 h-3.5 ${transport === "usb" ? "text-primary" : "text-muted"}`}
          aria-label="USB"
        />
        <button
          type="button"
          role="switch"
          aria-checked={transport === "ble"}
          aria-label="Toggle connection transport"
          onClick={() => handleTransportChange(transport === "usb" ? "ble" : "usb")}
          disabled={isConnected || isConnecting}
          className={`relative inline-flex h-5 w-9 items-center rounded-full transition-colors disabled:opacity-40 disabled:cursor-not-allowed ${
            transport === "ble" ? "bg-accent" : "bg-elevated"
          }`}
        >
          <span
            className={`inline-block h-4 w-4 rounded-full bg-white shadow transform transition-transform ${
              transport === "ble" ? "translate-x-[18px]" : "translate-x-[2px]"
            }`}
          />
        </button>
        <Bluetooth
          className={`w-3.5 h-3.5 ${transport === "ble" ? "text-primary" : "text-muted"}`}
          aria-label="BLE"
        />
      </div>

      {/* Port selector (USB only) */}
      {transport === "usb" && (
        <select
          value={selectedPort ?? ""}
          onChange={(e) => handleSelectPort(e.target.value || null)}
          disabled={isConnected || isConnecting}
          className="max-w-[100px] bg-surface text-primary text-sm border border-elevated rounded px-2 py-1 disabled:opacity-50 focus:outline-none focus:border-accent [&>option]:bg-surface [&>option]:text-primary"
        >
          <option value="" className="bg-surface text-primary">Select port…</option>
          {ports.map((p) => (
            <option key={p.name} value={p.name} className="bg-surface text-primary">
              {p.name.startsWith("/dev/") ? p.name.slice(5) : p.name}
            </option>
          ))}
        </select>
      )}

      {/* Connect / Disconnect button */}
      {!isConnected ? (
        transport === "usb" ? (
          <button
            onClick={handleConnect}
            disabled={!selectedPort || isConnecting}
            aria-label={isConnecting ? "Connecting" : "Connect"}
            title={isConnecting ? "Connecting…" : "Connect"}
            className="flex items-center justify-center px-2 py-1 bg-accent-dim hover:bg-accent-hover disabled:opacity-40 disabled:cursor-not-allowed text-white rounded transition-colors"
          >
            {isConnecting ? <Spinner size={14} /> : <LinkIcon size={14} />}
          </button>
        ) : (
          <button
            onClick={() => setShowBleDialog(true)}
            disabled={isConnecting}
            aria-label={isConnecting ? "Connecting" : "Connect"}
            title={isConnecting ? "Connecting…" : "Connect"}
            className="flex items-center justify-center gap-1.5 px-2 py-1 text-xs bg-accent-dim hover:bg-accent-hover disabled:opacity-40 disabled:cursor-not-allowed text-white rounded transition-colors"
          >
            {isConnecting ? <Spinner size={14} /> : <LinkIcon size={14} />}
          </button>
        )
      ) : (
        <button
          onClick={handleDisconnect}
          aria-label="Disconnect"
          title="Disconnect"
          className="flex items-center justify-center px-2 py-1 bg-elevated hover:bg-muted text-primary rounded transition-colors"
        >
          <Unlink size={14} />
        </button>
      )}

      {/* Device info */}
      {deviceInfo && (
        <div className="flex items-center gap-2 ml-1 text-xs">
          {/* Status pill: pulsing dot + device name + fw */}
          <div
            className="flex items-center gap-2 px-2.5 py-1 rounded-full bg-surface border border-elevated"
            title={
              deviceInfo.firmware_build_date
                ? `Built ${deviceInfo.firmware_build_date}`
                : undefined
            }
          >
            <span className="relative flex h-2 w-2 shrink-0">
              <span className="animate-ping absolute inline-flex h-full w-full rounded-full bg-success opacity-60" />
              <span className="relative inline-flex h-2 w-2 rounded-full bg-success" />
            </span>
            {deviceInfo.hardware_name && (
              <span className="text-primary font-medium">{deviceInfo.hardware_name}</span>
            )}
            {deviceInfo.firmware_version && (
              <span className="text-muted tabular-nums">{deviceInfo.firmware_version}</span>
            )}
          </div>

          {/* Signal/latency chip */}
          <SignalChip latencyMs={latency} transport={transport} />

          {/* Battery chip */}
          {batteryCharge != null && (
            <BatteryChip charge={Number(batteryCharge)} charging={batteryCharging} />
          )}

          {/* SD card chip */}
          {sdTotal != null && sdFree != null && (
            <div
              className="flex items-center gap-1.5 px-2.5 py-1 rounded-full bg-surface border border-elevated text-secondary"
              title={`SD: ${formatBytes(sdTotal - sdFree)} used / ${formatBytes(sdTotal)} total (${formatBytes(sdFree)} free)`}
            >
              <HardDrive size={12} className="text-muted" />
              <span className="tabular-nums">{sdUsedPct}%</span>
              <div className="w-12 h-1 bg-elevated rounded-full overflow-hidden">
                <div
                  className={`h-full rounded-full transition-all ${
                    (sdUsedPct ?? 0) > 90 ? "bg-danger" : "bg-accent-hover"
                  }`}
                  style={{ width: `${sdUsedPct}%` }}
                />
              </div>
            </div>
          )}

          {/* Reboot button */}
          <button
            onClick={() => setShowRebootConfirm(true)}
            className="ml-1 p-1.5 text-muted hover:text-accent hover:bg-elevated rounded-full transition-colors"
            title="Reboot device"
          >
            <Power size={13} />
          </button>
        </div>
      )}

      <div className="flex-1" />

      <GlobalSearch />

      {showRebootConfirm && (
        <ConfirmDialog
          title="Reboot Device"
          message="The Flipper Zero will restart. Reconnect manually after it comes back, or enable auto-reconnect in Settings."
          confirmLabel="Reboot"
          destructive
          onConfirm={handleReboot}
          onCancel={() => setShowRebootConfirm(false)}
        />
      )}

      {showBleDialog && <BleDialog onClose={() => setShowBleDialog(false)} />}
    </div>
  );
}

function BatteryChip({ charge, charging }: { charge: number; charging: boolean }) {
  const Icon =
    charge <= 15
      ? BatteryWarning
      : charge <= 35
      ? BatteryLow
      : charge <= 75
      ? BatteryMedium
      : BatteryFull;
  const iconColor =
    charge <= 15
      ? "text-danger"
      : charge <= 35
      ? "text-warning"
      : "text-success";
  const barColor =
    charge <= 15
      ? "bg-danger"
      : charge <= 35
      ? "bg-warning"
      : "bg-success";
  const pct = Math.max(0, Math.min(100, Math.round(charge)));
  return (
    <div
      className="relative flex items-center gap-1.5 px-2.5 py-1 rounded-full bg-surface border border-elevated text-secondary"
      title={`Battery: ${charge}%${charging ? " (charging)" : ""}`}
    >
      <span className="relative inline-flex">
        <Icon size={13} className={iconColor} />
        {charging && (
          <Zap
            size={9}
            className="absolute -top-0.5 -right-1 text-warning fill-warning"
          />
        )}
      </span>
      <span className="tabular-nums">{pct}%</span>
      <div className="w-12 h-1 bg-elevated rounded-full overflow-hidden">
        <div
          className={`h-full rounded-full transition-all ${barColor}`}
          style={{ width: `${pct}%` }}
        />
      </div>
    </div>
  );
}

function SignalChip({
  latencyMs,
  transport,
}: {
  latencyMs: number | null;
  transport: "usb" | "ble";
}) {
  const Icon =
    latencyMs == null
      ? Signal
      : latencyMs < 50
      ? SignalHigh
      : latencyMs < 150
      ? SignalMedium
      : SignalLow;
  const color =
    latencyMs == null
      ? "text-muted"
      : latencyMs < 50
      ? "text-success"
      : latencyMs < 150
      ? "text-warning"
      : "text-danger";
  const label = latencyMs == null ? "—" : `${latencyMs} ms`;
  return (
    <div
      className="flex items-center gap-1.5 px-2.5 py-1 rounded-full bg-surface border border-elevated text-secondary"
      title={`${transport.toUpperCase()} link: ${label} round-trip`}
    >
      <Icon size={13} className={color} />
      <span className="tabular-nums">{label}</span>
    </div>
  );
}
