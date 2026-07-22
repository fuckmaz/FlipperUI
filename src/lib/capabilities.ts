import type { Capability } from "../types/flipper";
import { normalizeCommandError } from "./commandError.ts";

export type FeatureAvailabilityState =
  | "supported"
  | "unsupported"
  | "busy"
  | "locked"
  | "unknown";

export interface FeatureAvailability {
  state: FeatureAvailabilityState;
  available: boolean;
  reason: string;
}

export function resolveFeatureAvailability(
  capability: Capability | null | undefined,
  runtime: { busy?: boolean | string; locked?: boolean | string } = {},
): FeatureAvailability {
  if (runtime.locked) {
    return unavailable(
      "locked",
      typeof runtime.locked === "string"
        ? runtime.locked
        : "Another connection mode currently owns the device",
    );
  }
  if (runtime.busy) {
    return unavailable(
      "busy",
      typeof runtime.busy === "string"
        ? runtime.busy
        : "The device is busy; try again shortly",
    );
  }
  if (!capability || capability.state === "unknown") {
    return unavailable(
      "unknown",
      capability?.reason ?? "Support was not reported by the device handshake",
    );
  }
  if (capability.state === "unsupported") {
    return unavailable(
      "unsupported",
      capability.reason ?? "This feature is not supported by the active connection",
    );
  }
  return { state: "supported", available: true, reason: "Supported" };
}

/** Convert typed command admission errors to the same UI availability model. */
export function availabilityFromCommandError(error: unknown): FeatureAvailability {
  const commandError = normalizeCommandError(error);
  switch (commandError.code) {
    case "busy":
      return unavailable("busy", commandError.message);
    case "operation_locked":
      return unavailable("locked", commandError.message);
    case "unsupported":
      return unavailable("unsupported", commandError.message);
    default:
      return unavailable("unknown", commandError.message);
  }
}

function unavailable(state: Exclude<FeatureAvailabilityState, "supported">, reason: string) {
  return { state, available: false, reason } as const;
}
