import type { DeviceInfo, PortInfo } from "../types/flipper";
import { connect, disconnect, listPorts } from "./tauri";
import {
  connectPreferredUsbIdentity,
  normalizeUsbIdentityPreferences,
  rememberUsbIdentity,
} from "./usbDeviceIdentity";
import {
  loadUsbIdentityPreferences,
  saveUsbIdentityPreferences,
} from "./usbDevicePreferences";

let connectionAttempt: Promise<DeviceInfo> | null = null;

export function connectSelectedUsbDevice(port: string): Promise<DeviceInfo> {
  return singleFlight(async () => {
    const info = await connect(port);
    const current = await loadUsbIdentityPreferences(port);
    await saveUsbIdentityPreferences(rememberUsbIdentity(current, info.hardware_uid, port));
    return info;
  });
}

export function connectPreferredUsbDevice(
  ports: PortInfo[],
  legacyPort: string | null,
): Promise<DeviceInfo> {
  return singleFlight(async () => {
    const preferences = await loadUsbIdentityPreferences(legacyPort);
    const result = await connectPreferredUsbIdentity(
      ports,
      preferences,
      connect,
      disconnect,
    );
    await saveUsbIdentityPreferences(result.preferences);
    return result.info;
  });
}

export async function connectPreferredAvailableUsbDevice(
  legacyPort: string | null,
): Promise<DeviceInfo> {
  return connectPreferredUsbDevice(await listPorts(), legacyPort);
}

export async function migrateLegacyUsbPort(
  legacyPort: string | null,
): Promise<string | null> {
  const preferences = await loadUsbIdentityPreferences(legacyPort);
  const normalized = normalizeUsbIdentityPreferences(preferences, legacyPort);
  return (
    (normalized.preferredUid
      ? normalized.portByUid[normalized.preferredUid]
      : null) ?? normalized.legacyPort
  );
}

function singleFlight(operation: () => Promise<DeviceInfo>): Promise<DeviceInfo> {
  if (connectionAttempt) return connectionAttempt;
  const current = operation().finally(() => {
    if (connectionAttempt === current) connectionAttempt = null;
  });
  connectionAttempt = current;
  return current;
}
