import { LazyStore } from "@tauri-apps/plugin-store";

import {
  normalizeUsbIdentityPreferences,
  type UsbIdentityPreferences,
} from "./usbDeviceIdentity";

const store = new LazyStore("usb-device-preferences.json", {
  defaults: {},
  autoSave: true,
});
const ROOT_KEY = "identity";
let mutation: Promise<unknown> = Promise.resolve();

export async function loadUsbIdentityPreferences(
  legacyPort: string | null,
): Promise<UsbIdentityPreferences> {
  const raw = await store.get<unknown>(ROOT_KEY);
  const normalized = normalizeUsbIdentityPreferences(raw, legacyPort);
  if (JSON.stringify(raw) !== JSON.stringify(normalized)) {
    await enqueue(async () => store.set(ROOT_KEY, normalized));
  }
  return normalized;
}

export function saveUsbIdentityPreferences(
  preferences: UsbIdentityPreferences,
): Promise<void> {
  return enqueue(async () => store.set(ROOT_KEY, preferences));
}

function enqueue<T>(operation: () => Promise<T>): Promise<T> {
  const next = mutation.then(operation, operation);
  mutation = next.then(
    () => undefined,
    () => undefined,
  );
  return next;
}
