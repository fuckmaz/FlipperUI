import { useState } from "react";
import {
  Battery,
  BatteryCharging,
  BatteryLow,
  Bluetooth,
  HardDrive,
  HardDriveDownload,
  Home,
  Info,
  RefreshCw,
  Thermometer,
  Usb,
  Zap,
} from "lucide-react";
import { useFlipperStore } from "../../store/useFlipperStore";
import { deviceTelemetry, useDeviceTelemetry } from "../../lib/deviceTelemetry";
import { FlipperSvgIcon } from "../ui/FlipperSvgIcon";
import { DeviceSettingsCard } from "../DeviceSettings/DeviceSettingsCard";
import { FirmwareFlashModal } from "../FirmwareFlash/FirmwareFlashModal";
import type { StorageInfo as StorageInfoType } from "../../types/flipper";
import { resolveFeatureAvailability } from "../../lib/capabilities";

import blackFlipper from "../../assets/flipper-zero/FZBlackNormal.svg";
import whiteFlipper from "../../assets/flipper-zero/FZWhiteNormal.svg";
import transparentFlipper from "../../assets/flipper-zero/FZClearNormal.svg";

import subghzIconSvg from "../../assets/icons/sub1.svg?raw";
import infraredIconSvg from "../../assets/icons/infrared.svg?raw";
import nfcIconSvg from "../../assets/icons/nfc.svg?raw";
import rfidIconSvg from "../../assets/icons/125.svg?raw";
import badusbIconSvg from "../../assets/icons/badusb.svg?raw";
import pluginsIconSvg from "../../assets/icons/plugins.svg?raw";

const flipperVariants: Record<string, string> = {
  "1": blackFlipper,
  "2": whiteFlipper,
  "3": transparentFlipper,
};

export function Dashboard() {
  const isConnected = useFlipperStore((s) => s.isConnected);
  const deviceInfo = useFlipperStore((s) => s.deviceInfo);
  const connectionKind = useFlipperStore((s) => s.connectionKind);
  const setActiveView = useFlipperStore((s) => s.setActiveView);
  const subghzCount = useFlipperStore((s) => s.subghzEntries.length);
  const irCount = useFlipperStore((s) => s.irEntries.length);
  const nfcCount = useFlipperStore((s) => s.nfcEntries.length);
  const rfidCount = useFlipperStore((s) => s.rfidEntries.length);
  const badusbCount = useFlipperStore((s) => s.badusbEntries.length);
  const appsCount = useFlipperStore((s) => s.appEntries.length);

  const {
    power,
    storage: sd,
    internalBytes,
    loading,
    refreshedAt,
  } = useDeviceTelemetry();
  const [showFirmware, setShowFirmware] = useState(false);

  const hardwareColor = deviceInfo?.hardware_name?.includes("Black")
    ? "1"
    : deviceInfo?.hardware_name?.includes("White")
      ? "2"
      : "3";
  const flipperImg = flipperVariants[hardwareColor] ?? whiteFlipper;

  const firmwareAvailability = resolveFeatureAvailability(
    deviceInfo?.capabilities.firmware_update,
  );
  const firmwareEnabled = isConnected && firmwareAvailability.available;
  const firmwareTooltip = !isConnected
    ? "Connect a Flipper via USB to flash firmware."
    : firmwareAvailability.available
      ? "Flash firmware."
      : firmwareAvailability.reason;

  return (
    <div className="flex-1 min-h-0 flex flex-col overflow-hidden">
      <header className="shrink-0 border-b border-border-subtle bg-panel">
        <div className="flex items-center gap-2 px-3 py-2">
          <Home size={14} className="text-accent" />
          <h2 className="text-xs font-medium text-primary">Dashboard</h2>
          <div className="flex-1" />
          {refreshedAt && (
            <span
              className="text-[11px] text-dim"
              title={new Date(refreshedAt).toLocaleString()}
            >
              updated {formatRelative(refreshedAt)}
            </span>
          )}
          <button
            onClick={() => void deviceTelemetry.refresh()}
            disabled={!isConnected || loading}
            title={isConnected ? "Refresh" : "Connect to refresh"}
            className="flex items-center gap-1 px-2 py-1 text-[11px] text-secondary hover:text-primary border border-border-subtle rounded hover:bg-surface/60 disabled:opacity-40 disabled:cursor-not-allowed"
          >
            <RefreshCw size={11} className={loading ? "animate-spin" : ""} />
            Refresh
          </button>
        </div>
      </header>

      <div className="flex-1 min-h-0 overflow-y-auto">
        <div className="max-w-6xl mx-auto px-4 py-5 flex flex-col gap-4">
          {/* Hero */}
          <section className="relative flex flex-col sm:flex-row items-center sm:items-stretch gap-5 px-4 py-4 bg-panel/60 border border-border-subtle rounded-lg">
            <img
              src={flipperImg}
              alt="Flipper Zero"
              className="w-44 max-w-full h-auto select-none"
              draggable={false}
            />
            <div className="flex-1 flex flex-col gap-2 min-w-0 text-center sm:text-left justify-center">
              <div className="flex items-center justify-center sm:justify-start gap-2 flex-wrap">
                <h1 className="text-xl font-semibold text-primary truncate">
                  {deviceInfo?.hardware_name ?? "Flipper Zero"}
                </h1>
                <ConnectionPill kind={connectionKind} connected={isConnected} />
              </div>
              <div className="text-xs text-secondary flex flex-wrap gap-x-3 gap-y-0.5 justify-center sm:justify-start">
                {deviceInfo?.firmware_version && (
                  <span>fw {deviceInfo.firmware_version}</span>
                )}
                {deviceInfo?.firmware_build_date && (
                  <span>built {deviceInfo.firmware_build_date}</span>
                )}
                {deviceInfo?.port && (
                  <span className="font-mono">{deviceInfo.port}</span>
                )}
              </div>
              {deviceInfo?.hardware_uid && (
                <div
                  className="text-[11px] text-dim font-mono truncate"
                  title={deviceInfo.hardware_uid}
                >
                  UID {deviceInfo.hardware_uid}
                </div>
              )}
            </div>
            <div className="absolute bottom-2 right-2 flex items-center gap-2">
              <div className="relative group/fw">
                <button
                  type="button"
                  onClick={() => setShowFirmware(true)}
                  disabled={!firmwareEnabled}
                  aria-label="Open firmware flash tool"
                  className="flex items-center gap-1.5 px-2.5 py-1 text-[11px] font-medium text-black bg-accent hover:bg-accent-hover rounded shadow-sm disabled:opacity-40 disabled:pointer-events-none transition-colors"
                >
                  <HardDriveDownload size={12} />
                  Firmware
                </button>
                {/* Styled hover tooltip — sits on the wrapper so it still
                    appears while the button is disabled (BLE / offline). */}
                <span
                  role="tooltip"
                  className="pointer-events-none absolute bottom-full right-0 mb-1.5 w-max max-w-[220px] px-2 py-1 rounded border border-border-subtle bg-elevated text-primary text-[11px] leading-snug text-left shadow-lg opacity-0 translate-y-0.5 group-hover/fw:opacity-100 group-hover/fw:translate-y-0 transition-all duration-150 z-10"
                >
                  {firmwareTooltip}
                </span>
              </div>
              <button
                type="button"
                onClick={() => setActiveView("info")}
                disabled={!isConnected}
                title="Detailed view"
                aria-label="Open device info detailed view"
                className="flex items-center gap-1.5 px-2 py-1 text-[11px] text-muted hover:text-primary border border-border-subtle rounded hover:bg-surface/60 disabled:opacity-40 disabled:cursor-not-allowed transition-colors"
              >
                <Info size={12} />
                Detailed view
              </button>
            </div>
          </section>

          {/* Stat cards */}
          <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-3">
            <BatteryCard power={power} loading={loading && !power} />
            <StorageCard
              sd={sd}
              internalBytes={internalBytes}
              loading={loading && !sd}
            />
            <DeviceSettingsCard />
          </div>

          {/* Library stats */}
          <section className="px-4 py-4 bg-panel/60 border border-border-subtle rounded-lg">
            <h3 className="text-sm font-semibold text-primary mb-3 flex items-center gap-1.5">
              <Zap size={14} className="text-accent" />
              Libraries
            </h3>
            <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-6 gap-2">
              <LibraryStat
                label="Sub-GHz"
                count={subghzCount}
                svg={subghzIconSvg}
                onClick={() => setActiveView("subghz")}
                disabled={!isConnected && subghzCount === 0}
              />
              <LibraryStat
                label="Infrared"
                count={irCount}
                svg={infraredIconSvg}
                onClick={() => setActiveView("infrared")}
                disabled={!isConnected && irCount === 0}
              />
              <LibraryStat
                label="NFC"
                count={nfcCount}
                svg={nfcIconSvg}
                onClick={() => setActiveView("nfc")}
                disabled={!isConnected && nfcCount === 0}
              />
              <LibraryStat
                label="RFID"
                count={rfidCount}
                svg={rfidIconSvg}
                onClick={() => setActiveView("rfid")}
                disabled={!isConnected && rfidCount === 0}
              />
              <LibraryStat
                label="BadUSB"
                count={badusbCount}
                svg={badusbIconSvg}
                onClick={() => setActiveView("badusb")}
                disabled={!isConnected && badusbCount === 0}
              />
              <LibraryStat
                label="Apps"
                count={appsCount}
                svg={pluginsIconSvg}
                onClick={() => setActiveView("apps")}
                disabled={!isConnected}
              />
            </div>
          </section>
        </div>
      </div>

      {showFirmware && (
        <FirmwareFlashModal onClose={() => setShowFirmware(false)} />
      )}
    </div>
  );
}

function ConnectionPill({
  kind,
  connected,
}: {
  kind: "serial" | "ble" | null;
  connected: boolean;
}) {
  if (!connected) {
    return (
      <span className="inline-flex items-center gap-1 px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-dim border border-border-subtle rounded">
        Offline
      </span>
    );
  }
  const Icon = kind === "ble" ? Bluetooth : Usb;
  const label = kind === "ble" ? "BLE" : "USB";
  return (
    <span className="inline-flex items-center gap-1 px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-success bg-success/10 border border-success/30 rounded">
      <Icon size={10} />
      {label}
    </span>
  );
}

// ── Cards ────────────────────────────────────────────────────────────────────

function Card({
  title,
  icon,
  children,
}: {
  title: string;
  icon?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <section className="flex flex-col bg-panel/60 border border-border-subtle rounded-lg p-4 min-h-[160px]">
      <div className="flex items-center gap-1.5 mb-3 text-primary">
        {icon}
        <h3 className="text-sm font-semibold">{title}</h3>
      </div>
      <div className="flex-1 flex flex-col">{children}</div>
    </section>
  );
}

function BatteryCard({
  power,
  loading,
}: {
  power: Record<string, string> | null;
  loading: boolean;
}) {
  const pick = (...keys: string[]): string | undefined => {
    if (!power) return undefined;
    for (const k of keys) {
      const v = power[k];
      if (v != null && v !== "") return v;
    }
    return undefined;
  };
  const charge = numOrNull(pick("charge_level", "charge"));
  const voltage = numOrNull(pick("battery_voltage", "voltage_gauge", "voltage"));
  const current = numOrNull(pick("battery_current", "current_gauge", "current"));
  const temp = numOrNull(pick("battery_temp", "temperature_gauge", "temperature"));
  const charging = current != null && current > 5; // mA, positive while charging
  const Icon = charging
    ? BatteryCharging
    : charge != null && charge < 20
      ? BatteryLow
      : Battery;

  return (
    <Card title="Battery" icon={<Icon size={14} className="text-accent" />}>
      {loading ? (
        <SkeletonRows rows={3} />
      ) : charge == null ? (
        <Empty hint="No battery data" />
      ) : (
        <div className="flex flex-col gap-3">
          <div className="flex items-center gap-3">
            <div className="text-3xl font-semibold text-primary tabular-nums">
              {Math.round(charge)}
              <span className="text-base text-secondary">%</span>
            </div>
            <div className="flex-1">
              <div className="h-2 bg-elevated rounded-full overflow-hidden">
                <div
                  className={`h-full rounded-full transition-[width] duration-300 ${
                    charge < 20
                      ? "bg-danger"
                      : charge < 50
                        ? "bg-accent"
                        : "bg-success"
                  }`}
                  style={{ width: `${Math.max(0, Math.min(100, charge))}%` }}
                />
              </div>
              {charging && (
                <div className="mt-1 text-[10px] uppercase tracking-wide text-success flex items-center gap-1">
                  <BatteryCharging size={10} />
                  Charging
                </div>
              )}
            </div>
          </div>
          <div className="grid grid-cols-3 gap-2 text-[11px]">
            <Metric label="Voltage" value={voltage != null ? `${(voltage * 0.001).toFixed(3)} V` : "—"} />
            <Metric
              label="Current"
              value={current != null ? `${Math.round(current)} mA` : "—"}
            />
            <Metric
              label={
                <span className="inline-flex items-center gap-0.5">
                  <Thermometer size={9} /> Temp
                </span>
              }
              value={temp != null ? `${temp.toFixed(1)} °C` : "—"}
            />
          </div>
        </div>
      )}
    </Card>
  );
}

function StorageCard({
  sd,
  internalBytes,
  loading,
}: {
  sd: StorageInfoType | null;
  internalBytes: number | null;
  loading: boolean;
}) {
  const total = sd?.total_space ?? null;
  const free = sd?.free_space ?? null;
  const used = total != null && free != null ? total - free : null;
  const pct =
    total != null && used != null && total > 0
      ? Math.round((used / total) * 100)
      : null;

  return (
    <Card title="Storage" icon={<HardDrive size={14} className="text-accent" />}>
      {loading ? (
        <SkeletonRows rows={3} />
      ) : total == null ? (
        <Empty hint="No storage data" />
      ) : (
        <div className="flex flex-col gap-3">
          <div className="flex items-baseline gap-2">
            <div className="text-2xl font-semibold text-primary tabular-nums">
              {pct ?? 0}
              <span className="text-base text-secondary">%</span>
            </div>
            <span className="text-[11px] text-secondary">used on SD</span>
          </div>
          <div className="h-2 bg-elevated rounded-full overflow-hidden">
            <div
              className={`h-full rounded-full transition-[width] duration-300 ${
                (pct ?? 0) > 90 ? "bg-danger" : "bg-accent-hover"
              }`}
              style={{ width: `${Math.max(0, Math.min(100, pct ?? 0))}%` }}
            />
          </div>
          <div className="grid grid-cols-3 gap-2 text-[11px]">
            <Metric label="Used" value={used != null ? formatBytes(used) : "—"} />
            <Metric label="Free" value={free != null ? formatBytes(free) : "—"} />
            <Metric
              label="/int"
              value={internalBytes != null ? formatBytes(internalBytes) : "—"}
            />
          </div>
        </div>
      )}
    </Card>
  );
}

function LibraryStat({
  label,
  count,
  svg,
  onClick,
  disabled,
}: {
  label: string;
  count: number;
  svg: string;
  onClick: () => void;
  disabled: boolean;
}) {
  return (
    <button
      onClick={onClick}
      disabled={disabled}
      className="flex items-center gap-3 px-3 py-2.5 bg-surface/60 hover:bg-elevated border border-border-subtle rounded transition-colors text-left disabled:opacity-40 disabled:cursor-not-allowed"
    >
      <FlipperSvgIcon svg={svg} size={20} />
      <div className="flex flex-col min-w-0">
        <span className="text-[10px] uppercase tracking-wide text-muted">{label}</span>
        <span className="text-base font-semibold text-primary tabular-nums">{count}</span>
      </div>
    </button>
  );
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function Metric({ label, value }: { label: React.ReactNode; value: string }) {
  return (
    <div className="flex flex-col">
      <span className="text-[10px] uppercase tracking-wide text-muted">{label}</span>
      <span className="text-primary tabular-nums">{value}</span>
    </div>
  );
}

function Empty({ hint }: { hint: string }) {
  return (
    <div className="flex-1 flex items-center justify-center text-[11px] text-dim">
      {hint}
    </div>
  );
}

function SkeletonRows({ rows }: { rows: number }) {
  return (
    <div className="flex flex-col gap-2 animate-pulse">
      {Array.from({ length: rows }).map((_, i) => (
        <div key={i} className="h-3 bg-elevated/60 rounded" />
      ))}
    </div>
  );
}

function numOrNull(v: string | undefined): number | null {
  if (v == null || v === "") return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024)
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

function formatRelative(ts: number): string {
  const diffSec = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  if (diffSec < 60) return "just now";
  if (diffSec < 3600) return `${Math.floor(diffSec / 60)}m ago`;
  if (diffSec < 86400) return `${Math.floor(diffSec / 3600)}h ago`;
  return `${Math.floor(diffSec / 86400)}d ago`;
}
