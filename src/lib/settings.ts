/**
 * Persisted application settings, backed by tauri-plugin-store.
 *
 * Settings live in a single key ("app") inside settings.json under the
 * platform's app-config dir (managed by the plugin). Reads are cached; writes
 * fan out to in-memory subscribers so React components can re-render without
 * re-hitting the store.
 */
import { LazyStore } from "@tauri-apps/plugin-store";

export interface AppSettings {
  /** ISO 639-1 code. Currently a stub — i18n strings aren't wired yet. */
  language: string;
  subghz: {
    /** Absolute Flipper paths excluded from the SubGhz library scan. */
    excludedDirs: string[];
  };
  infrared: {
    /** Absolute Flipper paths excluded from the Infrared library scan. */
    excludedDirs: string[];
  };
  nfc: {
    /** Absolute Flipper paths excluded from the NFC library scan. */
    excludedDirs: string[];
  };
  rfid: {
    /** Absolute Flipper paths excluded from the 125 kHz RFID library scan. */
    excludedDirs: string[];
  };
  badusb: {
    /** Absolute Flipper paths excluded from the BadUSB library scan. */
    excludedDirs: string[];
  };
  apps: {
    /** Absolute Flipper paths excluded from the App library scan. */
    excludedDirs: string[];
    /** Additional absolute Flipper paths scanned beyond the default /ext/apps. */
    extraDirs: string[];
  };
  tray: {
    /** When true, show the system-tray / menubar icon. */
    enabled: boolean;
    /** macOS only: when true and tray is enabled, hide the app from the Dock. */
    hideDockIcon: boolean;
    /** When true, render the tray icon as a flat monochrome glyph that adopts
     * the menubar's foreground color (template image on macOS). */
    monochromeIcon: boolean;
  };
  notifications: {
    /** Show an OS notification when a library scan (Sub-GHz / Infrared /
     * NFC / RFID / BadUSB / Apps) finishes. */
    libraryScansFinished: boolean;
    /** Show an OS notification when the Flipper disconnects unexpectedly.
     * Manual disconnects via the UI do not emit a notification. */
    deviceDisconnected: boolean;
  };
  updates: {
    /** How often FlipperUI checks GitHub Releases for a newer version. */
    checkFrequency: UpdateCheckFrequency;
    /** RFC 3339 timestamp of the last successful update check. */
    lastCheckedAt: string | null;
    /** Latest release for which an automatic toast was already shown. */
    lastNotifiedVersion: string | null;
  };
  appearance: {
    /** Selected app-icon variant id. Resolved server-side: unknown values
     * fall back to "default" without erroring. New variants can be added
     * without changing this type — the value is just a string. */
    appIcon: string;
    /** Theme accent color, applied to `--color-accent` (and derived
     * hover/dim variants) at runtime. Hex string `#rrggbb`. Default is
     * Flipper Zero orange. The FlipperUI logo orange in the splash and
     * app header is hardcoded and not affected by this setting. */
    themeAccent: string;
  };
  screenStream: {
    /** Default folder for `Save screenshot`. When null, the save dialog opens
     * at the OS default and only the filename is pre-filled. */
    screenshotDir: string | null;
    /** Default folder for the GIF recorder's save dialog. Same fallback rule. */
    gifDir: string | null;
  };
  connection: {
    /** Last-used transport. Restored on app launch. */
    transport: "usb" | "ble";
    /** Last-used USB serial port path. Restored on app launch when present. */
    lastPort: string | null;
    /** Last-connected BLE peripheral id. Used as the auto-reconnect target. */
    lastBleId: string | null;
    /** Display name for the last-connected BLE peripheral. */
    lastBleName: string | null;
    /** When true, the app auto-connects to a Flipper as soon as it shows up
     * (USB port detected, or a previously paired BLE peripheral) and
     * auto-reconnects after an unexpected drop. When false, the user must
     * click Connect manually. */
    autoReconnect: boolean;
    /** When true, sync the Flipper RTC to the host's local clock after
     * every successful USB or BLE connection. */
    syncClockOnConnect: boolean;
  };
  fileBrowser: {
    /** Which action icons appear inline on hover for each file row. */
    inlineActions: {
      rename: boolean;
      download: boolean;
      delete: boolean;
    };
  };
  libraries: {
    /** When true, run a pre-scan walk before Sub-GHz / Infrared / NFC / RFID /
     * BadUSB scans and prompt the user about directories with ≥254 entries or
     * containing files larger than 1 MiB. Checked rows are added to that
     * library's persistent `excludedDirs`. Apps is not affected. */
    preScanReview: boolean;
  };
}

export type UpdateCheckFrequency = "daily" | "startup" | "manual";

export const DEFAULT_SETTINGS: AppSettings = {
  language: "en",
  subghz: { excludedDirs: [] },
  infrared: { excludedDirs: [] },
  nfc: { excludedDirs: [] },
  rfid: { excludedDirs: [] },
  badusb: { excludedDirs: [] },
  apps: { excludedDirs: [], extraDirs: [] },
  tray: { enabled: true, hideDockIcon: false, monochromeIcon: false },
  notifications: { libraryScansFinished: true, deviceDisconnected: true },
  updates: {
    checkFrequency: "daily",
    lastCheckedAt: null,
    lastNotifiedVersion: null,
  },
  appearance: { appIcon: "default", themeAccent: "#ff8300" },
  screenStream: { screenshotDir: null, gifDir: null },
  connection: {
    transport: "usb",
    lastPort: null,
    lastBleId: null,
    lastBleName: null,
    autoReconnect: false,
    syncClockOnConnect: true,
  },
  fileBrowser: {
    inlineActions: { rename: true, download: true, delete: true },
  },
  libraries: {
    preScanReview: true,
  },
};

export type SettingsPatch = {
  language?: string;
  subghz?: {
    excludedDirs?: string[];
  };
  infrared?: {
    excludedDirs?: string[];
  };
  nfc?: {
    excludedDirs?: string[];
  };
  rfid?: {
    excludedDirs?: string[];
  };
  badusb?: {
    excludedDirs?: string[];
  };
  apps?: {
    excludedDirs?: string[];
    extraDirs?: string[];
  };
  tray?: {
    enabled?: boolean;
    hideDockIcon?: boolean;
    monochromeIcon?: boolean;
  };
  notifications?: {
    libraryScansFinished?: boolean;
    deviceDisconnected?: boolean;
  };
  updates?: {
    checkFrequency?: UpdateCheckFrequency;
    lastCheckedAt?: string | null;
    lastNotifiedVersion?: string | null;
  };
  appearance?: {
    appIcon?: string;
    themeAccent?: string;
  };
  screenStream?: {
    screenshotDir?: string | null;
    gifDir?: string | null;
  };
  connection?: {
    transport?: "usb" | "ble";
    lastPort?: string | null;
    lastBleId?: string | null;
    lastBleName?: string | null;
    autoReconnect?: boolean;
    syncClockOnConnect?: boolean;
  };
  fileBrowser?: {
    inlineActions?: {
      rename?: boolean;
      download?: boolean;
      delete?: boolean;
    };
  };
  libraries?: {
    preScanReview?: boolean;
  };
};

const STORE_FILE = "settings.json";
const STORE_KEY = "app";

const store = new LazyStore(STORE_FILE, {
  defaults: { [STORE_KEY]: DEFAULT_SETTINGS as unknown as Record<string, unknown> },
  autoSave: true,
});

let cached: AppSettings | null = null;
const listeners = new Set<(s: AppSettings) => void>();
let writeQueue: Promise<AppSettings> = Promise.resolve(DEFAULT_SETTINGS);

export async function loadSettings(): Promise<AppSettings> {
  if (cached) return cached;
  const raw = await store.get<Partial<AppSettings>>(STORE_KEY);
  cached = mergeWithDefaults(raw ?? {});
  return cached;
}

export function updateSettings(patch: SettingsPatch): Promise<AppSettings> {
  const nextWrite = writeQueue.then(() => updateSettingsNow(patch));
  writeQueue = nextWrite.catch(() => cached ?? DEFAULT_SETTINGS);
  return nextWrite;
}

async function updateSettingsNow(patch: SettingsPatch): Promise<AppSettings> {
  const current = await loadSettings();
  const next: AppSettings = {
    language: patch.language ?? current.language,
    subghz: {
      excludedDirs: patch.subghz?.excludedDirs ?? current.subghz.excludedDirs,
    },
    infrared: {
      excludedDirs: patch.infrared?.excludedDirs ?? current.infrared.excludedDirs,
    },
    nfc: {
      excludedDirs: patch.nfc?.excludedDirs ?? current.nfc.excludedDirs,
    },
    rfid: {
      excludedDirs: patch.rfid?.excludedDirs ?? current.rfid.excludedDirs,
    },
    badusb: {
      excludedDirs: patch.badusb?.excludedDirs ?? current.badusb.excludedDirs,
    },
    apps: {
      excludedDirs: patch.apps?.excludedDirs ?? current.apps.excludedDirs,
      extraDirs: patch.apps?.extraDirs ?? current.apps.extraDirs,
    },
    tray: {
      enabled: patch.tray?.enabled ?? current.tray.enabled,
      hideDockIcon: patch.tray?.hideDockIcon ?? current.tray.hideDockIcon,
      monochromeIcon:
        patch.tray?.monochromeIcon ?? current.tray.monochromeIcon,
    },
    notifications: {
      libraryScansFinished:
        patch.notifications?.libraryScansFinished ??
        current.notifications.libraryScansFinished,
      deviceDisconnected:
        patch.notifications?.deviceDisconnected ??
        current.notifications.deviceDisconnected,
    },
    updates: {
      checkFrequency:
        patch.updates?.checkFrequency ?? current.updates.checkFrequency,
      lastCheckedAt:
        patch.updates?.lastCheckedAt !== undefined
          ? patch.updates.lastCheckedAt
          : current.updates.lastCheckedAt,
      lastNotifiedVersion:
        patch.updates?.lastNotifiedVersion !== undefined
          ? patch.updates.lastNotifiedVersion
          : current.updates.lastNotifiedVersion,
    },
    appearance: {
      appIcon: patch.appearance?.appIcon ?? current.appearance.appIcon,
      themeAccent:
        patch.appearance?.themeAccent ?? current.appearance.themeAccent,
    },
    screenStream: {
      screenshotDir:
        patch.screenStream?.screenshotDir !== undefined
          ? patch.screenStream.screenshotDir
          : current.screenStream.screenshotDir,
      gifDir:
        patch.screenStream?.gifDir !== undefined
          ? patch.screenStream.gifDir
          : current.screenStream.gifDir,
    },
    connection: {
      transport: patch.connection?.transport ?? current.connection.transport,
      lastPort:
        patch.connection?.lastPort !== undefined
          ? patch.connection.lastPort
          : current.connection.lastPort,
      lastBleId:
        patch.connection?.lastBleId !== undefined
          ? patch.connection.lastBleId
          : current.connection.lastBleId,
      lastBleName:
        patch.connection?.lastBleName !== undefined
          ? patch.connection.lastBleName
          : current.connection.lastBleName,
      autoReconnect:
        patch.connection?.autoReconnect ?? current.connection.autoReconnect,
      syncClockOnConnect:
        patch.connection?.syncClockOnConnect ??
        current.connection.syncClockOnConnect,
    },
    fileBrowser: {
      inlineActions: {
        rename:
          patch.fileBrowser?.inlineActions?.rename ??
          current.fileBrowser.inlineActions.rename,
        download:
          patch.fileBrowser?.inlineActions?.download ??
          current.fileBrowser.inlineActions.download,
        delete:
          patch.fileBrowser?.inlineActions?.delete ??
          current.fileBrowser.inlineActions.delete,
      },
    },
    libraries: {
      preScanReview:
        patch.libraries?.preScanReview ?? current.libraries.preScanReview,
    },
  };
  await store.set(STORE_KEY, next);
  cached = next;
  listeners.forEach((cb) => cb(next));
  return next;
}

export function subscribeSettings(cb: (s: AppSettings) => void): () => void {
  listeners.add(cb);
  return () => {
    listeners.delete(cb);
  };
}

function mergeWithDefaults(raw: Partial<AppSettings>): AppSettings {
  return {
    language: raw.language ?? DEFAULT_SETTINGS.language,
    subghz: {
      excludedDirs:
        raw.subghz?.excludedDirs ?? DEFAULT_SETTINGS.subghz.excludedDirs,
    },
    infrared: {
      excludedDirs:
        raw.infrared?.excludedDirs ?? DEFAULT_SETTINGS.infrared.excludedDirs,
    },
    nfc: {
      excludedDirs:
        raw.nfc?.excludedDirs ?? DEFAULT_SETTINGS.nfc.excludedDirs,
    },
    rfid: {
      excludedDirs:
        raw.rfid?.excludedDirs ?? DEFAULT_SETTINGS.rfid.excludedDirs,
    },
    badusb: {
      excludedDirs:
        raw.badusb?.excludedDirs ?? DEFAULT_SETTINGS.badusb.excludedDirs,
    },
    apps: {
      excludedDirs:
        raw.apps?.excludedDirs ?? DEFAULT_SETTINGS.apps.excludedDirs,
      extraDirs: raw.apps?.extraDirs ?? DEFAULT_SETTINGS.apps.extraDirs,
    },
    tray: {
      enabled: raw.tray?.enabled ?? DEFAULT_SETTINGS.tray.enabled,
      hideDockIcon:
        raw.tray?.hideDockIcon ?? DEFAULT_SETTINGS.tray.hideDockIcon,
      monochromeIcon:
        raw.tray?.monochromeIcon ?? DEFAULT_SETTINGS.tray.monochromeIcon,
    },
    notifications: {
      libraryScansFinished:
        raw.notifications?.libraryScansFinished ??
        DEFAULT_SETTINGS.notifications.libraryScansFinished,
      deviceDisconnected:
        raw.notifications?.deviceDisconnected ??
        DEFAULT_SETTINGS.notifications.deviceDisconnected,
    },
    updates: {
      checkFrequency: isUpdateCheckFrequency(raw.updates?.checkFrequency)
        ? raw.updates.checkFrequency
        : DEFAULT_SETTINGS.updates.checkFrequency,
      lastCheckedAt:
        typeof raw.updates?.lastCheckedAt === "string"
          ? raw.updates.lastCheckedAt
          : DEFAULT_SETTINGS.updates.lastCheckedAt,
      lastNotifiedVersion:
        typeof raw.updates?.lastNotifiedVersion === "string"
          ? raw.updates.lastNotifiedVersion
          : DEFAULT_SETTINGS.updates.lastNotifiedVersion,
    },
    appearance: {
      appIcon:
        raw.appearance?.appIcon ?? DEFAULT_SETTINGS.appearance.appIcon,
      themeAccent:
        raw.appearance?.themeAccent ?? DEFAULT_SETTINGS.appearance.themeAccent,
    },
    screenStream: {
      screenshotDir:
        raw.screenStream?.screenshotDir ??
        DEFAULT_SETTINGS.screenStream.screenshotDir,
      gifDir:
        raw.screenStream?.gifDir ?? DEFAULT_SETTINGS.screenStream.gifDir,
    },
    connection: {
      transport:
        raw.connection?.transport ?? DEFAULT_SETTINGS.connection.transport,
      lastPort:
        raw.connection?.lastPort ?? DEFAULT_SETTINGS.connection.lastPort,
      lastBleId:
        raw.connection?.lastBleId ?? DEFAULT_SETTINGS.connection.lastBleId,
      lastBleName:
        raw.connection?.lastBleName ?? DEFAULT_SETTINGS.connection.lastBleName,
      autoReconnect:
        raw.connection?.autoReconnect ??
        DEFAULT_SETTINGS.connection.autoReconnect,
      syncClockOnConnect:
        raw.connection?.syncClockOnConnect ??
        DEFAULT_SETTINGS.connection.syncClockOnConnect,
    },
    fileBrowser: {
      inlineActions: {
        rename:
          raw.fileBrowser?.inlineActions?.rename ??
          DEFAULT_SETTINGS.fileBrowser.inlineActions.rename,
        download:
          raw.fileBrowser?.inlineActions?.download ??
          DEFAULT_SETTINGS.fileBrowser.inlineActions.download,
        delete:
          raw.fileBrowser?.inlineActions?.delete ??
          DEFAULT_SETTINGS.fileBrowser.inlineActions.delete,
      },
    },
    libraries: {
      preScanReview:
        raw.libraries?.preScanReview ??
        DEFAULT_SETTINGS.libraries.preScanReview,
    },
  };
}

function isUpdateCheckFrequency(
  value: unknown,
): value is UpdateCheckFrequency {
  return value === "daily" || value === "startup" || value === "manual";
}
