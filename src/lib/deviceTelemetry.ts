import { useSyncExternalStore } from "react";

import {
  deviceInfoAll,
  ping,
  powerInfo,
  storageDu,
  storageInfo,
} from "./tauri";
import {
  DeviceTelemetryService,
  type TelemetryScheduler,
  type VisibilitySource,
} from "./deviceTelemetryCore";

const browserScheduler: TelemetryScheduler = {
  setInterval: (callback, delayMs) => window.setInterval(callback, delayMs),
  clearInterval: (timer) => window.clearInterval(timer as number),
  now: () => Date.now(),
};

const browserVisibility: VisibilitySource = {
  isVisible: () => document.visibilityState !== "hidden",
  subscribe: (callback) => {
    const onVisibilityChange = () => callback(document.visibilityState !== "hidden");
    document.addEventListener("visibilitychange", onVisibilityChange);
    return () => document.removeEventListener("visibilitychange", onVisibilityChange);
  },
};

export const deviceTelemetry = new DeviceTelemetryService(
  { powerInfo, storageInfo, storageDu, ping, deviceInfoAll },
  browserScheduler,
  browserVisibility,
);

export function useDeviceTelemetry() {
  return useSyncExternalStore(
    deviceTelemetry.subscribe,
    deviceTelemetry.getSnapshot,
    deviceTelemetry.getSnapshot,
  );
}
