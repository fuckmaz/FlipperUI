import type { AppSettings } from "./settingsSchema";
import type { DeviceTelemetrySnapshot } from "./deviceTelemetryCore";
import type { DeviceInfo } from "../types/flipper";
import type { DiagEntry } from "./tauri";

export const SUPPORT_BUNDLE_SCHEMA_VERSION = 1 as const;
export const MAX_SUPPORT_DIAGNOSTICS = 200;
export const MAX_SUPPORT_BUNDLE_BYTES = 64 * 1024;

export interface SupportBundleInput {
  generatedAt: Date;
  appVersion: string | null;
  platform: string;
  userAgent: string;
  settings: AppSettings;
  device: DeviceInfo | null;
  connectionKind: "serial" | "ble" | null;
  telemetry: DeviceTelemetrySnapshot;
  diagnostics: DiagEntry[];
}

interface SafeDiagnosticEntry {
  tsMs: number;
  direction: string;
  commandId: number;
  contentKind: string;
  hasNext: boolean;
  payloadBytes: number;
  commandStatus: number;
  commandStatusName: string;
}

export function buildSupportBundle(input: SupportBundleInput) {
  const sourceDiagnostics = input.diagnostics.slice(-MAX_SUPPORT_DIAGNOSTICS);
  const diagnostics = sourceDiagnostics.map(safeDiagnostic);
  let removedForSize = 0;

  const create = () => ({
    schemaVersion: SUPPORT_BUNDLE_SCHEMA_VERSION,
    generatedAt: input.generatedAt.toISOString(),
    app: {
      version: bounded(input.appVersion, 64),
      platform: bounded(input.platform, 128),
      userAgent: bounded(input.userAgent, 256),
    },
    device: input.device
      ? {
          connected: true,
          transport: input.connectionKind,
          hardwareName: bounded(input.device.hardware_name, 128),
          hardwareVersion: bounded(input.device.hardware_version, 128),
          firmwareVersion: bounded(input.device.firmware_version, 128),
          firmwareBuildDate: bounded(input.device.firmware_build_date, 128),
          capabilities: input.device.capabilities,
        }
      : { connected: false, transport: null },
    telemetry: {
      sampledAt: input.telemetry.refreshedAt
        ? new Date(input.telemetry.refreshedAt).toISOString()
        : null,
      power: safePower(input.telemetry.power),
      storage: input.telemetry.storage,
      internalBytes: finite(input.telemetry.internalBytes),
      latencyMs: finite(input.telemetry.latency),
      errorStates: Object.fromEntries(
        Object.entries(input.telemetry.errors).map(([key, value]) => [
          key,
          value === null ? "ok" : "error",
        ]),
      ),
    },
    preferences: {
      autoReconnect: input.settings.connection.autoReconnect,
      syncClockOnConnect: input.settings.connection.syncClockOnConnect,
      trayEnabled: input.settings.tray.enabled,
      notifications: input.settings.notifications,
      libraryPathCounts: {
        subghz: input.settings.subghz.excludedDirs.length,
        infrared: input.settings.infrared.excludedDirs.length,
        nfc: input.settings.nfc.excludedDirs.length,
        rfid: input.settings.rfid.excludedDirs.length,
        badusb: input.settings.badusb.excludedDirs.length,
        apps: input.settings.apps.excludedDirs.length + input.settings.apps.extraDirs.length,
      },
    },
    diagnostics: {
      available: input.diagnostics.length,
      included: diagnostics.length,
      truncated:
        input.diagnostics.length > diagnostics.length || removedForSize > 0,
      entries: diagnostics,
    },
    redactions: [
      "device UID and port",
      "BLE identifiers and names",
      "local and device paths",
      "diagnostic detail and payload contents",
      "settings directory values",
    ],
  });

  let bundle = create();
  while (utf8Bytes(JSON.stringify(bundle)) > MAX_SUPPORT_BUNDLE_BYTES && diagnostics.length) {
    diagnostics.shift();
    removedForSize += 1;
    bundle = create();
  }
  if (utf8Bytes(JSON.stringify(bundle)) > MAX_SUPPORT_BUNDLE_BYTES) {
    throw new Error("The redacted support bundle exceeded its safety limit.");
  }
  return bundle;
}

export function serializeSupportBundle(input: SupportBundleInput): string {
  const json = `${JSON.stringify(buildSupportBundle(input))}\n`;
  if (utf8Bytes(json) > MAX_SUPPORT_BUNDLE_BYTES) {
    throw new Error("The redacted support bundle exceeded its safety limit.");
  }
  return json;
}

function safeDiagnostic(entry: DiagEntry): SafeDiagnosticEntry {
  return {
    tsMs: finite(entry.ts_ms) ?? 0,
    direction: bounded(entry.dir, 16) ?? "",
    commandId: finite(entry.command_id) ?? 0,
    contentKind: bounded(entry.content_kind, 96) ?? "",
    hasNext: entry.has_next === true,
    payloadBytes: finite(entry.payload_bytes) ?? 0,
    commandStatus: finite(entry.command_status) ?? 0,
    commandStatusName: bounded(entry.command_status_name, 64) ?? "",
  };
}

function safePower(power: Record<string, string> | null): Record<string, string> | null {
  if (!power) return null;
  const allowed = [
    "charge",
    "charge_level",
    "charging",
    "battery_current",
    "current_gauge",
    "current",
    "battery_voltage",
    "voltage_gauge",
    "voltage",
    "battery_temp",
    "temperature_gauge",
    "temperature",
    "battery_health",
    "health",
  ];
  return Object.fromEntries(
    allowed.flatMap((key) => {
      const value = bounded(power[key], 64);
      return value === null ? [] : [[key, value]];
    }),
  );
}

function bounded(value: unknown, maxLength: number): string | null {
  if (typeof value !== "string") return null;
  return Array.from(value, (character) => {
    const code = character.charCodeAt(0);
    return code <= 31 || code === 127 ? " " : character;
  })
    .join("")
    .trim()
    .slice(0, maxLength);
}

function finite(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function utf8Bytes(value: string): number {
  return new TextEncoder().encode(value).byteLength;
}
