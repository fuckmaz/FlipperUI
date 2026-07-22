/**
 * Frontend bindings for the firmware-flash tool.
 *
 * The backend owns the modular firmware-source registry and the whole flash
 * pipeline; this module is just typed `invoke` wrappers plus its scoped
 * progress channel.
 */
import { Channel, invoke } from "@tauri-apps/api/core";

export interface FirmwareProvider {
  id: string;
  name: string;
  blurb: string;
}

export interface FirmwareVersion {
  version: string;
  changelog: string;
  /** Unix epoch seconds; 0 when the source omits it. */
  timestamp: number;
  /** Opaque backend binding to the exact catalog entry. */
  selection_token: string;
}

export interface FirmwareChannel {
  id: string;
  title: string;
  description: string;
  versions: FirmwareVersion[];
}

export interface FirmwareCatalog {
  provider_id: string;
  channels: FirmwareChannel[];
}

/** Where a flash pulls its bundle from. Keys are snake_case to match the
 * backend's `FlashSource` (Tauri commands here use snake_case argument keys). */
export type FlashSource =
  | {
      kind: "remote";
      provider_id: string;
      channel_id: string;
      version: string;
      timestamp: number;
      selection_token: string;
    }
  | { kind: "local"; local_path: string };

export interface FlashOptions {
  /** Wipe any existing bundle dir under /ext/update before uploading. */
  clean: boolean;
}

export type FlashStage =
  | "download"
  | "verify"
  | "prepare"
  | "upload"
  | "install"
  | "reboot"
  | "done"
  | "error";

export type FlashLevel = "info" | "ok" | "warn" | "error";

export interface FlashProgress {
  operationId: number;
  stage: FlashStage;
  /** Empty == pure progress tick (advance the bar, don't log a line). */
  message: string;
  pct: number | null;
  level: FlashLevel;
}

export type FirmwareCancelStatus =
  | "cancelled"
  | "too_late"
  | "stale_operation"
  | "no_active_operation";

export interface FirmwareCancelResponse {
  status: FirmwareCancelStatus;
  message: string;
}

/** The selectable firmware sources, in display order. */
export const firmwareProviders = (): Promise<FirmwareProvider[]> =>
  invoke<FirmwareProvider[]>("firmware_providers");

/** Fetch + normalize a provider's directory.json (f7 update bundles only). */
export const firmwareFetchDirectory = (
  providerId: string,
): Promise<FirmwareCatalog> =>
  invoke<FirmwareCatalog>("firmware_fetch_directory", { provider_id: providerId });

/**
 * Run the full self-update pipeline. Resolves when the device has been told to
 * reboot into its updater (or rejects on error/cancel). Subscribe with
 * {@link onFlashProgress} first to render the live console.
 */
export const firmwareFlash = (
  source: FlashSource,
  options: FlashOptions,
  onProgress: (progress: FlashProgress) => void,
): Promise<void> => {
  let operationId: number | null = null;
  const channel = new Channel<FlashProgress>((progress) => {
    operationId = progress.operationId;
    activeFirmwareOperationId = progress.operationId;
    onProgress(progress);
  });
  return invoke<void>("firmware_flash", {
    source,
    options,
    onProgress: channel,
  }).finally(() => {
    if (
      operationId !== null &&
      activeFirmwareOperationId === operationId
    ) {
      activeFirmwareOperationId = null;
    }
  });
};

let activeFirmwareOperationId: number | null = null;

/** Cancel the active firmware operation without affecting file transfers. */
export const cancelFirmwareFlash = (): Promise<FirmwareCancelResponse> =>
  activeFirmwareOperationId === null
    ? Promise.resolve({
        status: "no_active_operation",
        message: "No matching firmware flash is active",
      })
    : invoke<FirmwareCancelResponse>("cancel_firmware_flash", {
        operationId: activeFirmwareOperationId,
      });
