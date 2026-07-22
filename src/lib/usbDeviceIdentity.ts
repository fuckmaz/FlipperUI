import type { DeviceInfo, PortInfo } from "../types/flipper";

export const USB_IDENTITY_VERSION = 1 as const;

export interface UsbIdentityPreferences {
  version: typeof USB_IDENTITY_VERSION;
  preferredUid: string | null;
  portByUid: Record<string, string>;
  /** One-time bridge from the old raw `settings.connection.lastPort`. */
  legacyPort: string | null;
}

export interface PreferredUsbConnection {
  info: DeviceInfo;
  port: string;
  preferences: UsbIdentityPreferences;
}

export function normalizeUsbIdentityPreferences(
  raw: unknown,
  legacyPort: string | null,
): UsbIdentityPreferences {
  const value = isRecord(raw) ? raw : {};
  const preferredUid = safeUid(value.preferredUid);
  const portByUid: Record<string, string> = {};
  if (isRecord(value.portByUid)) {
    for (const [uid, port] of Object.entries(value.portByUid).slice(0, 32)) {
      const safeKey = safeUid(uid);
      const safePort = safePortName(port);
      if (safeKey && safePort) portByUid[safeKey] = safePort;
    }
  }
  return {
    version: USB_IDENTITY_VERSION,
    preferredUid,
    portByUid,
    legacyPort: safePortName(value.legacyPort) ?? safePortName(legacyPort),
  };
}

export function rememberUsbIdentity(
  current: UsbIdentityPreferences,
  uid: string | null,
  port: string,
): UsbIdentityPreferences {
  const safeKey = safeUid(uid);
  const safePort = safePortName(port);
  if (!safeKey || !safePort) return current;
  return {
    version: USB_IDENTITY_VERSION,
    preferredUid: safeKey,
    portByUid: { ...current.portByUid, [safeKey]: safePort },
    legacyPort: null,
  };
}

export function preferredUsbPort(preferences: UsbIdentityPreferences): string | null {
  return (
    (preferences.preferredUid
      ? preferences.portByUid[preferences.preferredUid]
      : null) ?? preferences.legacyPort
  );
}

export function orderUsbCandidates(
  ports: PortInfo[],
  preferences: UsbIdentityPreferences,
): string[] {
  const available = ports.filter((port) => port.is_flipper).map((port) => port.name);
  const availableSet = new Set(available);
  const ordered: string[] = [];
  const add = (port: string | null | undefined) => {
    if (port && availableSet.has(port) && !ordered.includes(port)) ordered.push(port);
  };
  add(preferredUsbPort(preferences));
  add(preferences.legacyPort);
  for (const port of available) add(port);
  return ordered;
}

/**
 * Probe only advertised Flipper ports until the persisted UID is found.
 * Mismatched devices are disconnected before the next candidate is touched.
 * With legacy raw-port state and no UID, the first successful handshake owns
 * the migration and becomes the persisted identity.
 */
export async function connectPreferredUsbIdentity(
  ports: PortInfo[],
  preferences: UsbIdentityPreferences,
  connect: (port: string) => Promise<DeviceInfo>,
  disconnect: () => Promise<void>,
): Promise<PreferredUsbConnection> {
  const candidates = orderUsbCandidates(ports, preferences);
  if (candidates.length === 0) throw new Error("No Flipper USB device is available.");

  let lastError: unknown = null;
  for (const port of candidates) {
    try {
      const info = await connect(port);
      const expected = preferences.preferredUid;
      if (expected && info.hardware_uid !== expected) {
        await disconnect().catch(() => {});
        lastError = new Error(`USB device on ${port} did not match the saved device identity.`);
        continue;
      }
      return {
        info,
        port,
        preferences: rememberUsbIdentity(preferences, info.hardware_uid, port),
      };
    } catch (error) {
      lastError = error;
    }
  }
  throw lastError instanceof Error
    ? lastError
    : new Error("The saved Flipper USB device could not be found.");
}

function safeUid(value: unknown): string | null {
  if (typeof value !== "string") return null;
  const trimmed = value.trim();
  return /^[A-Za-z0-9_-]{1,128}$/.test(trimmed) ? trimmed : null;
}

function safePortName(value: unknown): string | null {
  if (typeof value !== "string") return null;
  const trimmed = value.trim();
  return trimmed && trimmed.length <= 4096 && !trimmed.includes("\0") ? trimmed : null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
