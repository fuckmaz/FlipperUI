/**
 * Typed wrappers over Tauri v2's invoke API.
 * IMPORTANT: In Tauri v2, invoke is imported from "@tauri-apps/api/core", not "@tauri-apps/api/tauri".
 */
import { Channel, invoke } from "@tauri-apps/api/core";
import type { DeviceInfo, FileEntry, PortInfo, StorageInfo } from "../types/flipper";
import type { SubGhzEntry } from "../types/subghz";
import type { IrEntry } from "../types/infrared";
import type { NfcEntry } from "../types/nfc";
import type { RfidEntry } from "../types/rfid";
import type { BadUsbEntry } from "../types/badusb";
import type { AppEntry } from "../types/apps";
import type {
  GpioMode,
  GpioPinName,
  GpioPull,
  GpioSnapshot,
} from "../types/gpio";
import { getCliCleanupPromise } from "../components/CliPanel/CliPanel";

// Helper to await any in-progress CLI cleanup before making RPC calls
async function awaitCliCleanup(): Promise<void> {
  const promise = getCliCleanupPromise();
  if (promise) {
    await promise;
  }
}

// ── Device commands ────────────────────────────────────────────────────────

export const listPorts = (): Promise<PortInfo[]> =>
  invoke<PortInfo[]>("list_ports");

export const connect = (port: string): Promise<DeviceInfo> =>
  invoke<DeviceInfo>("connect", { port });

export const disconnect = (): Promise<void> =>
  invoke<void>("disconnect");

// ── BLE device commands ────────────────────────────────────────────────────

export interface BleDevice {
  id: string;
  name: string;
  rssi: number | null;
  paired: boolean;
}

/** Discover Flipper devices over BLE. Runs a fixed ~10s scan. */
export const listBleDevices = (): Promise<BleDevice[]> =>
  invoke<BleDevice[]>("list_ble_devices");

/**
 * Start a live BLE discovery scan. Emits `ble-scan-device` Tauri events
 * (payload: {@link BleDevice}) as peripherals appear or update, and a
 * `ble-scan-stopped` event when the scan ends. Idempotent — calling while a
 * scan is already running has no effect.
 */
export const startBleScan = (): Promise<void> =>
  invoke<void>("start_ble_scan");

/** Stop a live scan started with {@link startBleScan}. Idempotent. */
export const stopBleScan = (): Promise<void> =>
  invoke<void>("stop_ble_scan");

/** Connect to a Flipper over BLE using an id from {@link listBleDevices}. */
export const connectBleDevice = (id: string, name?: string): Promise<DeviceInfo> =>
  invoke<DeviceInfo>("connect_ble_device", { id, name: name ?? null });

/** Which transport backs the active connection — `null` when disconnected. */
export const connectionKind = (): Promise<"serial" | "ble" | null> =>
  invoke<"serial" | "ble" | null>("connection_kind");

export interface DeviceDateTime {
  hour: number;
  minute: number;
  second: number;
  day: number;
  month: number;
  year: number;
  /** Monday = 1, Sunday = 7. */
  weekday: number;
}

export function currentDeviceDateTime(date = new Date()): DeviceDateTime {
  const jsWeekday = date.getDay(); // Sunday = 0, Monday = 1, ...
  return {
    hour: date.getHours(),
    minute: date.getMinutes(),
    second: date.getSeconds(),
    day: date.getDate(),
    month: date.getMonth() + 1,
    year: date.getFullYear(),
    weekday: jsWeekday === 0 ? 7 : jsWeekday,
  };
}

export const syncClock = (
  datetime: DeviceDateTime = currentDeviceDateTime(),
): Promise<void> => invoke<void>("sync_clock", { datetime });

// ── Storage commands ───────────────────────────────────────────────────────

export interface TransferProgress {
  operationId: number;
  completed: number;
  total: number;
  percent: number;
}

export interface ScanProgressPayload {
  operationId: number;
  scanned: number;
  total: number;
  current_path: string;
}

let activeTransferOperationId: number | null = null;

function transferChannel(
  onProgress?: (progress: TransferProgress) => void,
): { channel: Channel<TransferProgress>; operationId: () => number | null } {
  let operationId: number | null = null;
  const channel = new Channel<TransferProgress>((progress) => {
    operationId = progress.operationId;
    activeTransferOperationId = progress.operationId;
    onProgress?.(progress);
  });
  return { channel, operationId: () => operationId };
}

function clearTransferOperation(operationId: number | null): void {
  if (operationId !== null && activeTransferOperationId === operationId) {
    activeTransferOperationId = null;
  }
}

export const storageList = async (path: string): Promise<FileEntry[]> => {
  await awaitCliCleanup();
  return invoke<FileEntry[]>("storage_list", { path });
};

export const storageStat = async (path: string): Promise<FileEntry> => {
  await awaitCliCleanup();
  return invoke<FileEntry>("storage_stat", { path });
};

/**
 * Read a file from the Flipper. Returns base64-encoded bytes.
 * Decode with: Uint8Array.from(atob(result), c => c.charCodeAt(0))
 */
export const storageRead = async (
  path: string,
  onProgress?: (progress: TransferProgress) => void,
): Promise<string> => {
  await awaitCliCleanup();
  const progress = transferChannel(onProgress);
  try {
    return await invoke<string>("storage_read", {
      path,
      onProgress: progress.channel,
    });
  } finally {
    clearTransferOperation(progress.operationId());
  }
};

/**
 * Write a file to the Flipper. `data` must be base64-encoded.
 * Encode with: btoa(String.fromCharCode(...new Uint8Array(buffer)))
 */
export const storageWrite = async (
  path: string,
  data: string,
  onProgress?: (progress: TransferProgress) => void,
): Promise<void> => {
  await awaitCliCleanup();
  const progress = transferChannel(onProgress);
  try {
    return await invoke<void>("storage_write", {
      path,
      data,
      onProgress: progress.channel,
    });
  } finally {
    clearTransferOperation(progress.operationId());
  }
};

export const storageReadToLocal = async (
  path: string,
  localPath: string,
  onProgress?: (progress: TransferProgress) => void,
): Promise<void> => {
  await awaitCliCleanup();
  const progress = transferChannel(onProgress);
  try {
    return await invoke<void>("storage_read_to_local", {
      path,
      local_path: localPath,
      on_progress: progress.channel,
    });
  } finally {
    clearTransferOperation(progress.operationId());
  }
};

/**
 * Recursively download a Flipper directory into a local destination folder.
 * `localPath` is the full destination — directory contents land directly
 * inside it (caller appends the source folder's name). Emits cumulative
 * `download-progress` events (u32 0-100) across the whole tree.
 */
export const storageReadDirToLocal = async (
  path: string,
  localPath: string,
  onProgress?: (progress: TransferProgress) => void,
): Promise<void> => {
  await awaitCliCleanup();
  const progress = transferChannel(onProgress);
  try {
    return await invoke<void>("storage_read_dir_to_local", {
      path,
      local_path: localPath,
      on_progress: progress.channel,
    });
  } finally {
    clearTransferOperation(progress.operationId());
  }
};

export const storageWriteFromLocal = async (
  path: string,
  localPath: string,
  onProgress?: (progress: TransferProgress) => void,
): Promise<void> => {
  await awaitCliCleanup();
  const progress = transferChannel(onProgress);
  try {
    return await invoke<void>("storage_write_from_local", {
      path,
      local_path: localPath,
      on_progress: progress.channel,
    });
  } finally {
    clearTransferOperation(progress.operationId());
  }
};

export const storageMkdir = async (path: string): Promise<void> => {
  await awaitCliCleanup();
  return invoke<void>("storage_mkdir", { path });
};

export const storageDelete = async (path: string, recursive: boolean): Promise<void> => {
  await awaitCliCleanup();
  return invoke<void>("storage_delete", { path, recursive });
};

export interface StorageDeleteManyTarget {
  path: string;
  recursive: boolean;
}

export interface StorageDeleteManyFailure extends StorageDeleteManyTarget {
  error: string;
  fatal: boolean;
}

export interface StorageDeleteManyResult {
  deleted: StorageDeleteManyTarget[];
  failed: StorageDeleteManyFailure[];
  unattempted: StorageDeleteManyTarget[];
  stopped_reason: string | null;
}

export const storageDeleteMany = async (
  targets: StorageDeleteManyTarget[],
): Promise<StorageDeleteManyResult> => {
  await awaitCliCleanup();
  return invoke<StorageDeleteManyResult>("storage_delete_many", { targets });
};

export const storageRename = async (oldPath: string, newPath: string): Promise<void> => {
  await awaitCliCleanup();
  return invoke<void>("storage_rename", { old_path: oldPath, new_path: newPath });
};

export const storageInfo = async (path: string): Promise<StorageInfo> => {
  await awaitCliCleanup();
  return invoke<StorageInfo>("storage_info", { path });
};

// Recursive sum of file sizes under `path`. Used for the `/int` namespace —
// modern firmware aliases `/int` onto the SD card, so `storage_info("/int")`
// reports the SD's numbers, not the internal namespace's actual footprint.
export const storageDu = async (path: string): Promise<number> => {
  await awaitCliCleanup();
  return invoke<number>("storage_du", { path });
};

export const storageTimestamp = async (path: string): Promise<number> => {
  await awaitCliCleanup();
  return invoke<number>("storage_timestamp", { path });
};

export const storageTarExtract = async (tarPath: string, outPath: string): Promise<void> => {
  await awaitCliCleanup();
  return invoke<void>("storage_tar_extract", { tar_path: tarPath, out_path: outPath });
};

/** Cancel an in-progress file transfer (upload or download). */
export const cancelTransfer = (): Promise<void> =>
  activeTransferOperationId === null
    ? Promise.resolve()
    : invoke<void>("cancel_transfer", {
        operationId: activeTransferOperationId,
      });

// ── Device extended commands ──────────────────────────────────────────────

export const powerInfo = (): Promise<Record<string, string>> =>
  invoke<Record<string, string>>("power_info");

/** Full key/value map from RPC system.device_info — much richer than DeviceInfo. */
export const deviceInfoAll = (): Promise<Record<string, string>> =>
  invoke<Record<string, string>>("device_info_all");

export const reboot = (mode: number): Promise<void> =>
  invoke<void>("reboot", { mode });

/** Round-trip ping latency in milliseconds. */
export const ping = (): Promise<number> => invoke<number>("ping");

// ── Screen streaming commands ───────────────────────────────────────────

/** Start streaming the Flipper's screen. Emits "screen-frame" events with base64 RGBA data. */
export const screenStreamStart = (): Promise<void> =>
  invoke<void>("screen_stream_start");

/** Stop streaming the Flipper's screen. */
export const screenStreamStop = (): Promise<void> =>
  invoke<void>("screen_stream_stop");

/**
 * Send a button input event to the Flipper.
 * key: 0=UP 1=DOWN 2=RIGHT 3=LEFT 4=OK 5=BACK
 * inputType: 0=PRESS 1=RELEASE 2=SHORT 3=LONG 4=REPEAT
 */
export const sendInputEvent = (key: number, inputType: number): Promise<void> =>
  invoke<void>("send_input_event", { key, input_type: inputType });

// ── CLI commands ──────────────────────────────────────────────────────

/** Enter CLI mode: stops RPC session and starts streaming serial output. */
export const cliStart = (): Promise<void> =>
  invoke<void>("cli_start");

/** Send a text command to the Flipper CLI. */
export const cliSend = (input: string): Promise<void> =>
  invoke<void>("cli_send", { input });

/** Send the terminal Ctrl+C / ETX byte to interrupt the running CLI command. */
export const cliInterrupt = (): Promise<void> =>
  invoke<void>("cli_interrupt");

/** Leave CLI mode and re-enter RPC mode. */
export const cliStop = (): Promise<void> =>
  invoke<void>("cli_stop");

// ── App control (launch/exit Flipper apps) ──────────────────────────────

/** Launch a Flipper app by name with optional CLI-style args. */
export const appStart = async (name: string, args: string): Promise<void> => {
  await awaitCliCleanup();
  return invoke<void>("app_start", { name, args });
};

/** Exit the currently running Flipper app. */
export const appExit = async (): Promise<void> => {
  await awaitCliCleanup();
  return invoke<void>("app_exit");
};

/**
 * Begin Sub-GHz replay via the full RPC flow (Start → LoadFile → ButtonPress).
 * TX continues until {@link subghzTxStop} is called. Mirrors the iOS app.
 */
export const subghzTxStart = async (path: string): Promise<void> => {
  await awaitCliCleanup();
  return invoke<void>("subghz_tx_start", { path });
};

/** Stop an in-progress Sub-GHz replay (ButtonRelease + AppExit). */
export const subghzTxStop = async (): Promise<void> => {
  await awaitCliCleanup();
  return invoke<void>("subghz_tx_stop");
};

// ── Sub-GHz library ──────────────────────────────────────────────────────

type ScanFamily =
  | "subghz"
  | "infrared"
  | "nfc"
  | "rfid"
  | "badusb"
  | "apps";

const activeScanOperationIds: Partial<Record<ScanFamily, number>> = {};
let activePrewalkOperationId: number | null = null;

function scanChannel(
  family: ScanFamily,
  onProgress?: (progress: ScanProgressPayload) => void,
): { channel: Channel<ScanProgressPayload>; operationId: () => number | null } {
  let operationId: number | null = null;
  const channel = new Channel<ScanProgressPayload>((progress) => {
    operationId = progress.operationId;
    activeScanOperationIds[family] = progress.operationId;
    onProgress?.(progress);
  });
  return { channel, operationId: () => operationId };
}

function clearScanOperation(family: ScanFamily, operationId: number | null): void {
  if (operationId !== null && activeScanOperationIds[family] === operationId) {
    delete activeScanOperationIds[family];
  }
}

function cancelScan(command: string, family: ScanFamily): Promise<void> {
  const operationId = activeScanOperationIds[family];
  if (operationId !== undefined) {
    return invoke<void>(command, { operationId });
  }
  return family === "apps" || activePrewalkOperationId === null
    ? Promise.resolve()
    : invoke<void>("cancel_library_prewalk", {
        operationId: activePrewalkOperationId,
      });
}

/**
 * Scan a directory recursively for .sub files, parse their headers, and
 * return the list. Emits "subghz-scan-progress" events as it works.
 *
 * `cached` — optional list of previously-parsed entries (from the on-disk
 * cache). When supplied, files whose mtime hasn't changed are reused from
 * cache instead of being re-read over serial.
 */
export const subghzScan = async (
  root: string,
  excludedDirs: string[],
  cached?: SubGhzEntry[],
  onProgress?: (progress: ScanProgressPayload) => void,
): Promise<SubGhzEntry[]> => {
  await awaitCliCleanup();
  const progress = scanChannel("subghz", onProgress);
  try {
    return await invoke<SubGhzEntry[]>("subghz_scan", {
      root,
      excluded_dirs: excludedDirs,
      cached: cached ?? null,
      on_progress: progress.channel,
    });
  } finally {
    clearScanOperation("subghz", progress.operationId());
  }
};

/** Abort an in-progress SubGhz library scan. */
export const subghzCancelScan = (): Promise<void> =>
  cancelScan("subghz_cancel_scan", "subghz");

// ── Infrared library ────────────────────────────────────────────────────

/**
 * Scan a directory recursively for .ir files, parse their signal blocks,
 * and return the list. Emits "infrared-scan-progress" events as it works.
 */
export const infraredScan = async (
  root: string,
  excludedDirs: string[],
  cached?: IrEntry[],
  onProgress?: (progress: ScanProgressPayload) => void,
): Promise<IrEntry[]> => {
  await awaitCliCleanup();
  const progress = scanChannel("infrared", onProgress);
  try {
    return await invoke<IrEntry[]>("infrared_scan", {
      root,
      excluded_dirs: excludedDirs,
      cached: cached ?? null,
      on_progress: progress.channel,
    });
  } finally {
    clearScanOperation("infrared", progress.operationId());
  }
};

/** Abort an in-progress Infrared library scan. */
export const infraredCancelScan = (): Promise<void> =>
  cancelScan("infrared_cancel_scan", "infrared");

// ── NFC library ─────────────────────────────────────────────────────────

/**
 * Scan a directory recursively for `.nfc` files, parse their headers, and
 * return the list. Emits "nfc-scan-progress" events as it works.
 */
export const nfcScan = async (
  root: string,
  excludedDirs: string[],
  cached?: NfcEntry[],
  onProgress?: (progress: ScanProgressPayload) => void,
): Promise<NfcEntry[]> => {
  await awaitCliCleanup();
  const progress = scanChannel("nfc", onProgress);
  try {
    return await invoke<NfcEntry[]>("nfc_scan", {
      root,
      excluded_dirs: excludedDirs,
      cached: cached ?? null,
      on_progress: progress.channel,
    });
  } finally {
    clearScanOperation("nfc", progress.operationId());
  }
};

/** Abort an in-progress NFC library scan. */
export const nfcCancelScan = (): Promise<void> =>
  cancelScan("nfc_cancel_scan", "nfc");

/**
 * Parse the given `.nfc` paths only — no directory walk. Returns one
 * `NfcEntry` per readable path; non-`.nfc` and unreadable paths are dropped.
 * Used to merge freshly-uploaded files into the library without a full rescan.
 */
export const nfcParsePaths = async (paths: string[]): Promise<NfcEntry[]> => {
  await awaitCliCleanup();
  return invoke<NfcEntry[]>("nfc_parse_paths", { paths });
};

// ── RFID library ────────────────────────────────────────────────────────

/**
 * Scan a directory recursively for `.rfid` files, parse their headers, and
 * return the list. Emits "rfid-scan-progress" events as it works.
 */
export const rfidScan = async (
  root: string,
  excludedDirs: string[],
  cached?: RfidEntry[],
  onProgress?: (progress: ScanProgressPayload) => void,
): Promise<RfidEntry[]> => {
  await awaitCliCleanup();
  const progress = scanChannel("rfid", onProgress);
  try {
    return await invoke<RfidEntry[]>("rfid_scan", {
      root,
      excluded_dirs: excludedDirs,
      cached: cached ?? null,
      on_progress: progress.channel,
    });
  } finally {
    clearScanOperation("rfid", progress.operationId());
  }
};

/** Abort an in-progress RFID library scan. */
export const rfidCancelScan = (): Promise<void> =>
  cancelScan("rfid_cancel_scan", "rfid");

/** Parse only the given `.rfid` paths — no directory walk. */
export const rfidParsePaths = async (paths: string[]): Promise<RfidEntry[]> => {
  await awaitCliCleanup();
  return invoke<RfidEntry[]>("rfid_parse_paths", { paths });
};

// ── BadUSB library ──────────────────────────────────────────────────────

/**
 * Scan `/ext/badusb` and `/ext/badkb` recursively for `.txt` Duckyscript
 * files, parse line counts + leading comments, and return the combined list.
 * Emits "badusb-scan-progress" events as it works.
 */
export const badusbScan = async (
  usbRoot: string,
  kbRoot: string,
  excludedDirs: string[],
  cached?: BadUsbEntry[],
  onProgress?: (progress: ScanProgressPayload) => void,
): Promise<BadUsbEntry[]> => {
  await awaitCliCleanup();
  const progress = scanChannel("badusb", onProgress);
  try {
    return await invoke<BadUsbEntry[]>("badusb_scan", {
      usb_root: usbRoot,
      kb_root: kbRoot,
      excluded_dirs: excludedDirs,
      cached: cached ?? null,
      on_progress: progress.channel,
    });
  } finally {
    clearScanOperation("badusb", progress.operationId());
  }
};

/** Abort an in-progress BadUSB library scan. */
export const badusbCancelScan = (): Promise<void> =>
  cancelScan("badusb_cancel_scan", "badusb");

// ── Library prewalk ─────────────────────────────────────────────────────

export type PrewalkLibrary = "subghz" | "infrared" | "nfc" | "rfid" | "badusb";

export interface PrewalkLargestFile {
  name: string;
  size: number;
}

export interface PrewalkDirStat {
  path: string;
  entry_count: number;
  largest_file: PrewalkLargestFile | null;
}

interface PrewalkProgress {
  operationId: number;
  visited: number;
  current_path: string;
}

/**
 * Walk the given library roots and return directories that cross the
 * pre-scan thresholds (≥254 direct entries, or contain a >1 MiB file).
 * Emits `library-prewalk-progress` events while walking.
 */
export const libraryPrewalk = async (
  library: PrewalkLibrary,
  roots: string[],
  excludedDirs: string[],
): Promise<PrewalkDirStat[]> => {
  await awaitCliCleanup();
  let operationId: number | null = null;
  const onProgress = new Channel<PrewalkProgress>((progress) => {
    operationId = progress.operationId;
    activePrewalkOperationId = progress.operationId;
  });
  try {
    return await invoke<PrewalkDirStat[]>("library_prewalk", {
      library,
      roots,
      excluded_dirs: excludedDirs,
      on_progress: onProgress,
    });
  } finally {
    if (
      operationId !== null &&
      activePrewalkOperationId === operationId
    ) {
      activePrewalkOperationId = null;
    }
  }
};

/** parse only BadUSB / BadKB `.txt` paths */
export const badusbParsePaths = async (paths: string[]): Promise<BadUsbEntry[]> => {
  await awaitCliCleanup();
  return invoke<BadUsbEntry[]>("badusb_parse_paths", { paths });
};

// ── Apps library ────────────────────────────────────────────────────────

/**
 * Scan one or more roots recursively for `.fap` files and return a parsed
 * list. Emits "apps-scan-progress" events as it works.
 *
 * Pass previously-parsed entries as `cached` to skip re-reading files whose
 * mtime hasn't moved.
 */
export const appsScan = async (
  roots: string[],
  excludedDirs: string[],
  cached?: AppEntry[],
  onProgress?: (progress: ScanProgressPayload) => void,
): Promise<AppEntry[]> => {
  await awaitCliCleanup();
  const progress = scanChannel("apps", onProgress);
  try {
    return await invoke<AppEntry[]>("apps_scan", {
      roots,
      excluded_dirs: excludedDirs,
      cached: cached ?? null,
      on_progress: progress.channel,
    });
  } finally {
    clearScanOperation("apps", progress.operationId());
  }
};

/** Abort an in-progress App library scan. */
export const appsCancelScan = (): Promise<void> =>
  cancelScan("apps_cancel_scan", "apps");

/**
 * Parse the given `.fap` paths only — no directory walk. `roots` is the same
 * list of apps roots used for scans; the backend picks the longest matching
 * prefix to derive each entry's `category`. Used to merge freshly-installed
 * apps into the library without a full rescan.
 */
export const appsParsePaths = async (
  paths: string[],
  roots: string[],
): Promise<AppEntry[]> => {
  await awaitCliCleanup();
  return invoke<AppEntry[]>("apps_parse_paths", { paths, roots });
};

/**
 * Read a .fap and extract its embedded 10x10 icon. Returns base64-encoded
 * raw XBM bytes (32-byte slot; first 20 bytes are the bitmap), or null if
 * the file has no embedded icon.
 */
export const appsReadIcon = async (path: string): Promise<string | null> => {
  await awaitCliCleanup();
  return invoke<string | null>("apps_read_icon", { path });
};

// ── GPIO ────────────────────────────────────────────────────────────────
//
// The 8 controllable pins are addressed by the proto enum names: PC0, PC1,
// PC3, PB2, PB3, PA4, PA6, PA7. The OTG bit toggles the +5 V rail on pin 1.
// Firmware does NOT expose `GetInputPull`, so pull state is tracked
// frontend-only after each `gpioSetPull` call.

export const gpioSnapshot = async (): Promise<GpioSnapshot> => {
  await awaitCliCleanup();
  return invoke<GpioSnapshot>("gpio_snapshot");
};

export const gpioSetMode = async (
  pin: GpioPinName,
  mode: GpioMode,
): Promise<void> => {
  await awaitCliCleanup();
  return invoke<void>("gpio_set_mode", { pin, mode });
};

export const gpioGetMode = async (pin: GpioPinName): Promise<GpioMode> => {
  await awaitCliCleanup();
  return invoke<GpioMode>("gpio_get_mode", { pin });
};

export const gpioSetPull = async (
  pin: GpioPinName,
  pull: GpioPull,
): Promise<void> => {
  await awaitCliCleanup();
  return invoke<void>("gpio_set_pull", { pin, pull });
};

export const gpioReadPin = async (pin: GpioPinName): Promise<0 | 1> => {
  await awaitCliCleanup();
  return invoke<0 | 1>("gpio_read_pin", { pin });
};

export const gpioWritePin = async (
  pin: GpioPinName,
  value: 0 | 1,
): Promise<void> => {
  await awaitCliCleanup();
  return invoke<void>("gpio_write_pin", { pin, value });
};

/**
 * Drive an output pin HIGH for a bounded duration, then LOW. The backend owns
 * the complete sequence so component teardown cannot skip the cleanup write.
 */
export const gpioPulsePin = async (
  pin: GpioPinName,
  durationMs: number,
): Promise<void> => {
  await awaitCliCleanup();
  return invoke<void>("gpio_pulse_pin", { pin, durationMs });
};

export const gpioGetOtg = async (): Promise<boolean> => {
  await awaitCliCleanup();
  return invoke<boolean>("gpio_get_otg");
};

export const gpioSetOtg = async (on: boolean): Promise<void> => {
  await awaitCliCleanup();
  return invoke<void>("gpio_set_otg", { on });
};

// ── Diagnostics ─────────────────────────────────────────────────────────

export interface DiagEntry {
  ts_ms: number;
  dir: "Tx" | "Rx" | "Event";
  command_id: number;
  command_status: number;
  command_status_name: string;
  has_next: boolean;
  content_kind: string;
  detail: string;
  payload_bytes: number;
}

export const diagEnable = (on: boolean): Promise<void> =>
  invoke<void>("diag_enable", { on });

export const diagEntries = (): Promise<DiagEntry[]> =>
  invoke<DiagEntry[]>("diag_entries");

export const diagClear = (): Promise<void> =>
  invoke<void>("diag_clear");

export const diagIsEnabled = (): Promise<boolean> =>
  invoke<boolean>("diag_is_enabled");

// ── App icon variants ───────────────────────────────────────────────────

export interface AppIconVariant {
  /** Stable id persisted in settings.json (e.g. "default", "dark"). */
  id: string;
  /** Short human-readable name shown next to the thumbnail. */
  label: string;
  /** Base64-encoded PNG bytes for the chooser thumbnail. */
  png_base64: string;
}

/** Catalogue of available app-icon variants for the chooser UI. */
export const appIconVariants = (): Promise<AppIconVariant[]> =>
  invoke<AppIconVariant[]>("app_icon_variants");

/**
 * Apply the named app-icon variant to all live windows and (on macOS) the
 * Dock. Returns the canonical id that was actually applied — the input
 * verbatim when valid, or `"default"` after a fallback.
 */
export const setAppIcon = (variant: string): Promise<string> =>
  invoke<string>("set_app_icon", { variant });
