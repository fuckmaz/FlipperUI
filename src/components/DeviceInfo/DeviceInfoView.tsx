import { useEffect, useMemo, useState } from "react";
import {
  AlertTriangle,
  Check,
  ChevronDown,
  ChevronUp,
  Copy,
  HardDrive,
  Info,
  RefreshCw,
  Search,
  Terminal,
} from "lucide-react";
import { useFlipperStore } from "../../store/useFlipperStore";
import { Spinner } from "../ui/Spinner";
import { deviceTelemetry, useDeviceTelemetry } from "../../lib/deviceTelemetry";
import type { StorageInfo as StorageInfoType } from "../../types/flipper";

import blackFlipper from "../../assets/flipper-zero/FZBlackNormal.svg";
import whiteFlipper from "../../assets/flipper-zero/FZWhiteNormal.svg";
import transparentFlipper from "../../assets/flipper-zero/FZClearNormal.svg";

const flipperVariants: Record<string, string> = {
  "1": blackFlipper,
  "2": whiteFlipper,
  "3": transparentFlipper,
};

const COLOR_LABELS: Record<string, string> = {
  "1": "Black",
  "2": "White",
  "3": "Transparent",
};

function imageForColor(_code: string): string {
  return flipperVariants[_code] ?? whiteFlipper; // default to white if unknown
}

function labelForColor(code: string): string {
  if (!code) return " ";
  return COLOR_LABELS[code] ?? " ";
}

export function DeviceInfoView() {
  const isConnected = useFlipperStore((s) => s.isConnected);
  const deviceInfo = useFlipperStore((s) => s.deviceInfo);

  const {
    deviceInfo: info,
    power,
    storage: sd,
    internalBytes,
    loading,
    refreshedAt: fetchedAt,
    errors,
  } = useDeviceTelemetry();
  const error = errors.deviceInfo;
  const [rawOpen, setRawOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [copiedAll, setCopiedAll] = useState(false);
  const [copiedKey, setCopiedKey] = useState<string | null>(null);

  const fetchAll = () => deviceTelemetry.refresh(true);

  useEffect(() => {
    if (!isConnected) {
      return;
    }
    void deviceTelemetry.refresh(true);
  }, [isConnected]);

  const hardwareColor = info?.["hardware_color"] ?? "";
  const colorLabel = labelForColor(hardwareColor);
  const hwName = deviceInfo?.hardware_name ?? info?.["hardware_name"] ?? "Flipper Zero";
  const hwUid = deviceInfo?.hardware_uid ?? info?.["hardware_uid"] ?? null;
  const hwVer = info?.["hardware_ver"] ?? null;

  // Raw-list filter + copy. Power info (SystemPowerInfoRequest) comes back as
  // a separate kv bag from device_info; merge it in with a `power_` prefix so
  // both sets show up in one sorted list without key collisions.
  const mergedRaw = useMemo(() => {
    const out: Record<string, string> = { ...(info ?? {}) };
    if (power) {
      for (const [k, v] of Object.entries(power)) {
        out[`power_${k}`] = v;
      }
    }
    return out;
  }, [info, power]);

  const rawEntries = useMemo(() => {
    const q = query.trim().toLowerCase();
    const all = Object.entries(mergedRaw).sort(([a], [b]) =>
      a.localeCompare(b),
    );
    if (!q) return all;
    return all.filter(
      ([k, v]) => k.toLowerCase().includes(q) || v.toLowerCase().includes(q),
    );
  }, [mergedRaw, query]);

  const copyAllRaw = async () => {
    const text = Object.entries(mergedRaw)
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([k, v]) => `${k}: ${v}`)
      .join("\n");
    try {
      await navigator.clipboard.writeText(text);
      setCopiedAll(true);
      setTimeout(() => setCopiedAll(false), 1200);
    } catch {
      /* ignore */
    }
  };
  const copyValue = async (key: string, value: string) => {
    try {
      await navigator.clipboard.writeText(value);
      setCopiedKey(key);
      setTimeout(() => setCopiedKey((k) => (k === key ? null : k)), 1200);
    } catch {
      /* ignore */
    }
  };

  if (!isConnected) return null;

  return (
    <div className="flex-1 min-h-0 flex flex-col overflow-hidden">
      <header className="shrink-0 border-b border-border-subtle bg-panel">
        <div className="flex items-center gap-2 px-3 py-2">
          <Info size={14} className="text-accent" />
          <h2 className="text-xs font-medium text-primary">Device Info</h2>
          <div className="flex-1" />
          {fetchedAt && (
            <span
              className="text-[11px] text-dim"
              title={new Date(fetchedAt).toLocaleString()}
            >
              updated {formatRelative(fetchedAt)}
            </span>
          )}
          <button
            onClick={() => void fetchAll()}
            disabled={loading}
            title="Refresh"
            className="flex items-center gap-1 px-2 py-1 text-[11px] text-secondary hover:text-primary border border-border-subtle rounded hover:bg-surface/60 disabled:opacity-40 disabled:cursor-not-allowed"
          >
            <RefreshCw size={11} className={loading ? "animate-spin" : ""} />
            Refresh
          </button>
        </div>
      </header>

      {error && (
        <div className="flex items-start gap-2 px-3 py-2 bg-danger/10 border-b border-danger/30 text-xs text-danger">
          <AlertTriangle size={13} className="mt-0.5 shrink-0" />
          <span className="flex-1">{error}</span>
        </div>
      )}

      <div className="flex-1 min-h-0 overflow-y-auto">
        <div className="max-w-5xl mx-auto px-4 py-5 flex flex-col gap-5">
          {/* Hero: device image + identity */}
          <section className="flex flex-col sm:flex-row items-center sm:items-center gap-5 px-3 py-4 bg-panel/60 border border-border-subtle rounded-lg">
            <img
              src={imageForColor(hardwareColor)}
              alt={`Flipper Zero ${colorLabel ?? ""}`.trim()}
              className="w-52 max-w-full h-auto select-none"
              draggable={false}
            />
            <div className="flex flex-col gap-1 min-w-0 text-center sm:text-left">
              <h1 className="text-xl font-semibold text-primary truncate">
                {hwName}
              </h1>
              <div className="text-xs text-secondary flex flex-wrap gap-x-3 gap-y-0.5 justify-center sm:justify-start">
                {hwVer && <span>hw {hwVer}</span>}
                {colorLabel && <span>{colorLabel}</span>}
                {info?.["hardware_region"] && (
                  <span>region {info["hardware_region"]}</span>
                )}
              </div>
              {hwUid && (
                <div
                  className="text-[11px] text-dim font-mono truncate"
                  title={hwUid}
                >
                  UID {hwUid}
                </div>
              )}
            </div>
          </section>

          {loading && !info ? (
            <div className="flex items-center justify-center gap-2 text-xs text-dim py-10">
              <Spinner size={14} />
              Reading device info…
            </div>
          ) : (
            <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
              <SystemTile info={info} />
              <BatteryTile power={power} />
              <StorageTile title="Storage (SD)" mount="/ext" data={sd} />
              <InternalNamespaceTile bytes={internalBytes} />
            </div>
          )}
        </div>
      </div>

      {/* Toggleable raw list — slots into the spot that CLI takes in File Explorer */}
      <div className="shrink-0 border-t border-border-subtle bg-panel/60">
        <button
          onClick={() => setRawOpen((o) => !o)}
          className="w-full flex items-center gap-2 px-3 py-1.5 text-[11px] text-secondary hover:text-primary hover:bg-surface/40 transition-colors"
          title={rawOpen ? "Hide raw device info" : "Show raw device info"}
        >
          <Terminal size={12} />
          <span>Raw device info</span>
          <span className="text-dim">
            ({rawEntries.length}
            {query ? ` / ${Object.keys(mergedRaw).length}` : ""})
          </span>
          <div className="flex-1" />
          {rawOpen ? <ChevronDown size={12} /> : <ChevronUp size={12} />}
        </button>
      </div>

      {rawOpen && (
        <div className="shrink-0 border-t border-border-subtle bg-panel h-56 flex flex-col">
          <div className="flex items-center gap-2 px-3 py-1.5 border-b border-border-subtle">
            <div className="relative flex-1 max-w-xs">
              <Search
                size={11}
                className="absolute left-2 top-1/2 -translate-y-1/2 text-dim pointer-events-none"
              />
              <input
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                placeholder="Filter keys or values…"
                className="w-full bg-surface border border-border-subtle rounded pl-6 pr-2 py-0.5 text-[11px] text-primary placeholder:text-dim focus:outline-none focus:border-accent"
              />
            </div>
            <button
              onClick={copyAllRaw}
              disabled={Object.keys(mergedRaw).length === 0}
              title="Copy all as text"
              className="flex items-center gap-1 px-2 py-0.5 text-[11px] text-secondary hover:text-primary border border-border-subtle rounded hover:bg-surface/60 disabled:opacity-40 disabled:cursor-not-allowed"
            >
              {copiedAll ? <Check size={10} /> : <Copy size={10} />}
              {copiedAll ? "Copied" : "Copy all"}
            </button>
          </div>
          <div className="flex-1 min-h-0 overflow-y-auto font-mono text-[11px]">
            {rawEntries.length === 0 ? (
              <div className="flex items-center justify-center h-full text-dim">
                {Object.keys(mergedRaw).length === 0
                  ? "No data."
                  : `No entries match "${query}".`}
              </div>
            ) : (
              rawEntries.map(([k, v], i) => (
                <div
                  key={k}
                  className={`group grid grid-cols-[14rem_1fr_auto] gap-3 items-center px-3 py-0.5 hover:bg-surface/40 ${
                    i === rawEntries.length - 1
                      ? ""
                      : "border-b border-border-subtle/40"
                  }`}
                >
                  <span className="text-secondary truncate" title={k}>
                    {k}
                  </span>
                  <span className="text-primary truncate tabular-nums" title={v}>
                    {v || <span className="text-dim italic">empty</span>}
                  </span>
                  <button
                    onClick={() => copyValue(k, v)}
                    title={copiedKey === k ? "Copied!" : "Copy value"}
                    className="opacity-0 group-hover:opacity-100 focus:opacity-100 transition-opacity text-muted hover:text-accent p-0.5 rounded"
                  >
                    {copiedKey === k ? <Check size={10} /> : <Copy size={10} />}
                  </button>
                </div>
              ))
            )}
          </div>
        </div>
      )}
    </div>
  );
}

// ── Tiles ────────────────────────────────────────────────────────────────

function Tile({
  title,
  icon,
  children,
}: {
  title: string;
  icon?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <section className="flex flex-col bg-panel/60 border border-border-subtle rounded-lg p-4">
      <div className="flex items-center gap-1.5 mb-3 text-primary">
        {icon}
        <h3 className="text-sm font-semibold">{title}</h3>
      </div>
      <dl className="flex flex-col gap-1">{children}</dl>
    </section>
  );
}

function Row({
  label,
  value,
  mono,
}: {
  label: string;
  value: React.ReactNode;
  mono?: boolean;
}) {
  return (
    <div className="grid grid-cols-[minmax(0,10rem)_1fr] gap-3 text-xs py-1 border-b border-border-subtle/40 last:border-b-0">
      <dt className="text-muted">{label}</dt>
      <dd className={`text-primary truncate ${mono ? "font-mono" : ""}`}>
        {value ?? <span className="text-dim">—</span>}
      </dd>
    </div>
  );
}

function SystemTile({ info }: { info: Record<string, string> | null }) {
  return (
    <Tile title="System" icon={<Info size={14} className="text-accent" />}>
      <Row
        label="Firmware"
        value={info?.["firmware_version"] || null}
        mono
      />
      <Row
        label="Build date"
        value={formatBuildDate(info?.["firmware_build_date"])}
      />
      <Row label="Branch" value={info?.["firmware_branch"] || null} />
      <Row
        label="Commit"
        value={info?.["firmware_commit"] || null}
        mono
      />
      <Row label="Target" value={info?.["firmware_target"] || null} />
    </Tile>
  );
}

function BatteryTile({ power }: { power: Record<string, string> | null }) {
  // Firmware emits keys via furi_hal_power_info_get() joined with '_' — so the
  // wire keys are `charge_level`, `battery_voltage`, `battery_current`,
  // `battery_temp`, `battery_health`, `capacity_remain`, `capacity_full`,
  // `capacity_design`. Older forks sometimes used unprefixed names, kept as
  // fallbacks.
  const pick = (...keys: string[]): string | undefined => {
    if (!power) return undefined;
    for (const k of keys) {
      const v = power[k];
      if (v != null && v !== "") return v;
    }
    return undefined;
  };
  const charge = pick("charge_level", "charge");
  const voltage = pick("battery_voltage", "voltage_gauge", "voltage");
  const current = pick("battery_current", "current_gauge", "current");
  const temperature = pick("battery_temp", "temperature_gauge", "temperature");
  const capacityRemain = pick("capacity_remain", "capacity_remaining");
  const capacityFull = pick("capacity_full");
  const health = pick("battery_health", "health");

  // Only surface "full" separately when it differs from remaining — otherwise
  // it's just noise. Compared as numbers so "1200.0" and "1200" match.
  const remainNum = capacityRemain != null ? Number(capacityRemain) : NaN;
  const fullNum = capacityFull != null ? Number(capacityFull) : NaN;
  const showFull =
    capacityFull != null &&
    !(Number.isFinite(remainNum) && Number.isFinite(fullNum) && remainNum === fullNum);

  return (
    <Tile
      title="Battery"
      icon={
        <span className="text-accent">
          {/* inline battery glyph, lucide Battery is used elsewhere in DevicePanel */}
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
            <rect x="2" y="7" width="18" height="10" rx="2" />
            <path d="M22 11v2" />
          </svg>
        </span>
      }
    >
      <Row label="Charge" value={formatPercent(charge)} />
      <Row label="Voltage" value={formatVoltage(voltage)} />
      <Row label="Current" value={formatNumber(current, " mA", 0)} />
      <Row label="Temperature" value={formatNumber(temperature, " °C", 1)} />
      <Row label="Capacity" value={formatNumber(capacityRemain, " mAh", 0)} />
      {showFull && (
        <Row label="Capacity (full)" value={formatNumber(capacityFull, " mAh", 0)} />
      )}
      <Row label="Health" value={formatPercent(health)} />
    </Tile>
  );
}

function StorageTile({
  title,
  mount,
  data,
}: {
  title: string;
  mount: string;
  data: StorageInfoType | null;
}) {
  const total = data?.total_space ?? null;
  const free = data?.free_space ?? null;
  const used = total != null && free != null ? total - free : null;
  const pct =
    total != null && used != null && total > 0
      ? Math.round((used / total) * 100)
      : null;

  return (
    <Tile title={title} icon={<HardDrive size={14} className="text-accent" />}>
      <Row label="Total" value={total != null ? formatBytes(total) : null} mono />
      <Row label="Used" value={used != null ? formatBytes(used) : null} mono />
      <Row label="Free" value={free != null ? formatBytes(free) : null} mono />
      <Row
        label="Used %"
        value={
          pct != null ? (
            <span className="flex items-center gap-2">
              <span className="tabular-nums">{pct}%</span>
              <span className="flex-1 h-1.5 bg-elevated rounded-full overflow-hidden max-w-[10rem]">
                <span
                  className={`block h-full rounded-full ${
                    pct > 90 ? "bg-danger" : "bg-accent-hover"
                  }`}
                  style={{ width: `${pct}%` }}
                />
              </span>
            </span>
          ) : null
        }
      />
      <Row label="Mount" value={<span className="font-mono">{mount}</span>} />
    </Tile>
  );
}

// Modern Flipper firmware (`storage_processing.c`: `storage_process_alias`)
// unconditionally rewrites any `/int` path to `/ext/<internal_dir>` before
// dispatch, so `StorageInfoRequest("/int")` returns the SD card's numbers.
// There's no RPC call that reports true on-chip LFS space. We show the
// recursive footprint of `/int` contents (which live on the SD) instead —
// that's the number the user can actually act on.
function InternalNamespaceTile({ bytes }: { bytes: number | null }) {
  return (
    <Tile
      title="Internal (/int)"
      icon={<HardDrive size={14} className="text-accent" />}
    >
      <Row label="Used" value={bytes != null ? formatBytes(bytes) : null} mono />
      <Row label="Mount" value={<span className="font-mono">/int</span>} />
      <div className="mt-2 text-[11px] text-dim leading-snug">
        On modern firmware, <span className="font-mono">/int</span> is a virtual
        folder on the SD card — there is no separate on-chip flash budget to
        report. Shown value is the recursive size of <span className="font-mono">
        /int</span>'s contents.
      </div>
    </Tile>
  );
}

// ── Formatters ───────────────────────────────────────────────────────────

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024)
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

function formatPercent(v: string | undefined | null): string | null {
  if (v == null || v === "") return null;
  const n = Number(v);
  if (Number.isNaN(n)) return v;
  return `${n}%`;
}

function formatNumber(
  v: string | undefined | null,
  unit: string,
  decimals: number,
): string | null {
  if (v == null || v === "") return null;
  const n = Number(v);
  if (Number.isNaN(n)) return `${v}${unit}`;
  return `${n.toFixed(decimals)}${unit}`;
}

// Firmware reports battery_voltage in millivolts; convert to volts.
function formatVoltage(v: string | undefined | null): string | null {
  if (v == null || v === "") return null;
  const n = Number(v);
  if (Number.isNaN(n)) return `${v} V`;
  return `${(n * 0.001).toFixed(3)} V`;
}

// Accepts both "27-03-2026" and "2026-03-27"-ish firmware strings; pass-through if
// it doesn't match.
function formatBuildDate(v: string | undefined | null): string | null {
  if (!v) return null;
  return v;
}

function formatRelative(ts: number): string {
  const diffSec = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  if (diffSec < 60) return "just now";
  if (diffSec < 3600) return `${Math.floor(diffSec / 60)}m ago`;
  if (diffSec < 86400) return `${Math.floor(diffSec / 3600)}h ago`;
  return `${Math.floor(diffSec / 86400)}d ago`;
}
