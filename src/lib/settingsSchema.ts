/** Pure settings schema, migration, and validation helpers. */

export interface AppSettings {
  language: string;
  subghz: { excludedDirs: string[] };
  infrared: { excludedDirs: string[] };
  nfc: { excludedDirs: string[] };
  rfid: { excludedDirs: string[] };
  badusb: { excludedDirs: string[] };
  apps: { excludedDirs: string[]; extraDirs: string[] };
  tray: {
    enabled: boolean;
    hideDockIcon: boolean;
    monochromeIcon: boolean;
  };
  notifications: {
    libraryScansFinished: boolean;
    deviceDisconnected: boolean;
  };
  updates: {
    checkFrequency: UpdateCheckFrequency;
    lastCheckedAt: string | null;
    lastNotifiedVersion: string | null;
  };
  appearance: { appIcon: string; themeAccent: string };
  screenStream: { screenshotDir: string | null; gifDir: string | null };
  connection: {
    transport: "usb" | "ble";
    lastPort: string | null;
    lastBleId: string | null;
    lastBleName: string | null;
    autoReconnect: boolean;
    syncClockOnConnect: boolean;
  };
  fileBrowser: {
    inlineActions: { rename: boolean; download: boolean; delete: boolean };
  };
  libraries: { preScanReview: boolean };
}

export type UpdateCheckFrequency = "daily" | "startup" | "manual";

export type DeepPartial<T> = {
  [K in keyof T]?: T[K] extends readonly unknown[]
    ? T[K]
    : T[K] extends object
      ? DeepPartial<T[K]>
      : T[K];
};

export type SettingsPatch = DeepPartial<AppSettings>;

export const CURRENT_SETTINGS_VERSION = 1 as const;

export interface SettingsDocument {
  schemaVersion: typeof CURRENT_SETTINGS_VERSION;
  settings: AppSettings;
}

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
  libraries: { preScanReview: true },
};

type RecordValue = Record<string, unknown>;
type ValidationRule =
  | ((value: unknown) => boolean)
  | { readonly [key: string]: ValidationRule };

const isBoolean = (value: unknown) => typeof value === "boolean";
const isLanguage = (value: unknown) => value === "en";
const isTransport = (value: unknown) => value === "usb" || value === "ble";
const isUpdateFrequency = (value: unknown) =>
  value === "daily" || value === "startup" || value === "manual";
const isNullableTimestamp = (value: unknown) =>
  value === null ||
  (typeof value === "string" &&
    value.length <= 64 &&
    !Number.isNaN(Date.parse(value)));
const isNullableString = (maxLength: number) => (value: unknown) =>
  value === null ||
  (typeof value === "string" &&
    value.length <= maxLength &&
    !value.includes("\0"));
const isIdentifier = (value: unknown) =>
  typeof value === "string" && /^[A-Za-z0-9_-]{1,64}$/.test(value);
const isAccent = (value: unknown) =>
  typeof value === "string" && /^#[0-9a-fA-F]{6}$/.test(value);
const isDevicePath = (value: unknown) =>
  typeof value === "string" &&
  value.length <= 1024 &&
  /^\/(ext|int|any)(\/|$)/.test(value.trim()) &&
  !value.includes("\0");
const isDevicePathArray = (value: unknown) =>
  Array.isArray(value) && value.length <= 512 && value.every(isDevicePath);

const SETTINGS_RULES: { readonly [K in keyof AppSettings]: ValidationRule } = {
  language: isLanguage,
  subghz: { excludedDirs: isDevicePathArray },
  infrared: { excludedDirs: isDevicePathArray },
  nfc: { excludedDirs: isDevicePathArray },
  rfid: { excludedDirs: isDevicePathArray },
  badusb: { excludedDirs: isDevicePathArray },
  apps: { excludedDirs: isDevicePathArray, extraDirs: isDevicePathArray },
  tray: {
    enabled: isBoolean,
    hideDockIcon: isBoolean,
    monochromeIcon: isBoolean,
  },
  notifications: {
    libraryScansFinished: isBoolean,
    deviceDisconnected: isBoolean,
  },
  updates: {
    checkFrequency: isUpdateFrequency,
    lastCheckedAt: isNullableTimestamp,
    lastNotifiedVersion: isNullableString(64),
  },
  appearance: { appIcon: isIdentifier, themeAccent: isAccent },
  screenStream: {
    screenshotDir: isNullableString(4096),
    gifDir: isNullableString(4096),
  },
  connection: {
    transport: isTransport,
    lastPort: isNullableString(4096),
    lastBleId: isNullableString(512),
    lastBleName: isNullableString(512),
    autoReconnect: isBoolean,
    syncClockOnConnect: isBoolean,
  },
  fileBrowser: {
    inlineActions: {
      rename: isBoolean,
      download: isBoolean,
      delete: isBoolean,
    },
  },
  libraries: { preScanReview: isBoolean },
};

export interface DecodedSettings {
  document: SettingsDocument;
  needsWrite: boolean;
  recovered: boolean;
}

export function createDefaultSettings(): AppSettings {
  return clone(DEFAULT_SETTINGS);
}

export function createSettingsDocument(settings: AppSettings): SettingsDocument {
  return {
    schemaVersion: CURRENT_SETTINGS_VERSION,
    settings: clone(settings),
  };
}

/**
 * Decode existing persisted state. Legacy unversioned settings are version 0.
 * Malformed values are sanitized field-by-field; unreadable/future documents
 * recover to defaults instead of preventing application startup.
 */
export function decodeStoredSettings(raw: unknown): DecodedSettings {
  try {
    const migrated = migrateToCurrent(raw, false);
    const document = createSettingsDocument(sanitizeSettings(migrated.settings));
    return {
      document,
      needsWrite: !jsonEqual(raw, document),
      recovered: false,
    };
  } catch {
    return {
      document: createSettingsDocument(createDefaultSettings()),
      needsWrite: true,
      recovered: true,
    };
  }
}

/** Parse and strictly validate a user-supplied export before any store write. */
export function parseImportedSettings(json: string): SettingsDocument {
  let raw: unknown;
  try {
    raw = JSON.parse(json);
  } catch {
    throw new Error("The selected file is not valid JSON.");
  }

  const migrated = migrateToCurrent(raw, true);
  return createSettingsDocument(sanitizeSettings(migrated.settings));
}

export function serializeSettingsDocument(document: SettingsDocument): string {
  return `${JSON.stringify(document, null, 2)}\n`;
}

export function applySettingsPatch(
  current: AppSettings,
  patch: SettingsPatch,
): AppSettings {
  validateObject(patch, SETTINGS_RULES, "settings patch", false);
  return sanitizeSettings(deepMerge(current, patch) as RecordValue);
}

export function sanitizeSettings(value: unknown): AppSettings {
  const raw = record(value);
  const subghz = record(raw.subghz);
  const infrared = record(raw.infrared);
  const nfc = record(raw.nfc);
  const rfid = record(raw.rfid);
  const badusb = record(raw.badusb);
  const apps = record(raw.apps);
  const tray = record(raw.tray);
  const notifications = record(raw.notifications);
  const updates = record(raw.updates);
  const appearance = record(raw.appearance);
  const screenStream = record(raw.screenStream);
  const connection = record(raw.connection);
  const fileBrowser = record(raw.fileBrowser);
  const inlineActions = record(fileBrowser.inlineActions);
  const libraries = record(raw.libraries);

  return {
    language: isLanguage(raw.language) ? raw.language : DEFAULT_SETTINGS.language,
    subghz: {
      excludedDirs: sanitizeDevicePaths(subghz.excludedDirs),
    },
    infrared: {
      excludedDirs: sanitizeDevicePaths(infrared.excludedDirs),
    },
    nfc: { excludedDirs: sanitizeDevicePaths(nfc.excludedDirs) },
    rfid: { excludedDirs: sanitizeDevicePaths(rfid.excludedDirs) },
    badusb: { excludedDirs: sanitizeDevicePaths(badusb.excludedDirs) },
    apps: {
      excludedDirs: sanitizeDevicePaths(apps.excludedDirs),
      extraDirs: sanitizeDevicePaths(apps.extraDirs),
    },
    tray: {
      enabled: booleanOr(tray.enabled, DEFAULT_SETTINGS.tray.enabled),
      hideDockIcon: booleanOr(
        tray.hideDockIcon,
        DEFAULT_SETTINGS.tray.hideDockIcon,
      ),
      monochromeIcon: booleanOr(
        tray.monochromeIcon,
        DEFAULT_SETTINGS.tray.monochromeIcon,
      ),
    },
    notifications: {
      libraryScansFinished: booleanOr(
        notifications.libraryScansFinished,
        DEFAULT_SETTINGS.notifications.libraryScansFinished,
      ),
      deviceDisconnected: booleanOr(
        notifications.deviceDisconnected,
        DEFAULT_SETTINGS.notifications.deviceDisconnected,
      ),
    },
    updates: {
      checkFrequency: isUpdateFrequency(updates.checkFrequency)
        ? updates.checkFrequency
        : DEFAULT_SETTINGS.updates.checkFrequency,
      lastCheckedAt: isNullableTimestamp(updates.lastCheckedAt)
        ? (updates.lastCheckedAt as string | null)
        : DEFAULT_SETTINGS.updates.lastCheckedAt,
      lastNotifiedVersion: isNullableString(64)(updates.lastNotifiedVersion)
        ? (updates.lastNotifiedVersion as string | null)
        : DEFAULT_SETTINGS.updates.lastNotifiedVersion,
    },
    appearance: {
      appIcon: isIdentifier(appearance.appIcon)
        ? (appearance.appIcon as string)
        : DEFAULT_SETTINGS.appearance.appIcon,
      themeAccent: isAccent(appearance.themeAccent)
        ? (appearance.themeAccent as string).toLowerCase()
        : DEFAULT_SETTINGS.appearance.themeAccent,
    },
    screenStream: {
      screenshotDir: nullableStringOr(
        screenStream.screenshotDir,
        4096,
        DEFAULT_SETTINGS.screenStream.screenshotDir,
      ),
      gifDir: nullableStringOr(
        screenStream.gifDir,
        4096,
        DEFAULT_SETTINGS.screenStream.gifDir,
      ),
    },
    connection: {
      transport: isTransport(connection.transport)
        ? connection.transport
        : DEFAULT_SETTINGS.connection.transport,
      lastPort: nullableStringOr(
        connection.lastPort,
        4096,
        DEFAULT_SETTINGS.connection.lastPort,
      ),
      lastBleId: nullableStringOr(
        connection.lastBleId,
        512,
        DEFAULT_SETTINGS.connection.lastBleId,
      ),
      lastBleName: nullableStringOr(
        connection.lastBleName,
        512,
        DEFAULT_SETTINGS.connection.lastBleName,
      ),
      autoReconnect: booleanOr(
        connection.autoReconnect,
        DEFAULT_SETTINGS.connection.autoReconnect,
      ),
      syncClockOnConnect: booleanOr(
        connection.syncClockOnConnect,
        DEFAULT_SETTINGS.connection.syncClockOnConnect,
      ),
    },
    fileBrowser: {
      inlineActions: {
        rename: booleanOr(
          inlineActions.rename,
          DEFAULT_SETTINGS.fileBrowser.inlineActions.rename,
        ),
        download: booleanOr(
          inlineActions.download,
          DEFAULT_SETTINGS.fileBrowser.inlineActions.download,
        ),
        delete: booleanOr(
          inlineActions.delete,
          DEFAULT_SETTINGS.fileBrowser.inlineActions.delete,
        ),
      },
    },
    libraries: {
      preScanReview: booleanOr(
        libraries.preScanReview,
        DEFAULT_SETTINGS.libraries.preScanReview,
      ),
    },
  };
}

function migrateToCurrent(
  raw: unknown,
  strict: boolean,
): { version: number; settings: unknown } {
  let version: number;
  let settings: unknown;

  if (isRecord(raw) && hasOwn(raw, "schemaVersion")) {
    if (!Number.isInteger(raw.schemaVersion)) {
      throw new Error("Settings schemaVersion must be an integer.");
    }
    version = raw.schemaVersion as number;
    if (version < 0 || version > CURRENT_SETTINGS_VERSION) {
      throw new Error(`Unsupported settings schema version: ${version}.`);
    }
    if (strict) assertKeys(raw, ["schemaVersion", "settings"], "document", true);
    settings = raw.settings;
  } else {
    version = 0;
    settings = raw;
  }

  while (version < CURRENT_SETTINGS_VERSION) {
    if (version === 0) {
      if (strict) validateObject(settings, SETTINGS_RULES, "settings", false);
      settings = sanitizeSettings(settings);
      version = 1;
      continue;
    }
    throw new Error(`No migration exists for settings schema version ${version}.`);
  }

  if (strict) validateObject(settings, SETTINGS_RULES, "settings", true);
  return { version, settings };
}

function validateObject(
  value: unknown,
  rules: { readonly [key: string]: ValidationRule },
  path: string,
  requireAll: boolean,
): void {
  if (!isRecord(value)) throw new Error(`${path} must be an object.`);
  assertKeys(value, Object.keys(rules), path, requireAll);

  for (const [key, child] of Object.entries(value)) {
    const rule = rules[key];
    const childPath = `${path}.${key}`;
    if (typeof rule === "function") {
      if (!rule(child)) throw new Error(`${childPath} has an invalid value.`);
    } else {
      validateObject(child, rule, childPath, requireAll);
    }
  }
}

function assertKeys(
  value: RecordValue,
  allowed: string[],
  path: string,
  requireAll: boolean,
): void {
  const allowedSet = new Set(allowed);
  const unknown = Object.keys(value).find((key) => !allowedSet.has(key));
  if (unknown) throw new Error(`${path}.${unknown} is not a recognized setting.`);
  if (!requireAll) return;
  const missing = allowed.find((key) => !hasOwn(value, key));
  if (missing) throw new Error(`${path}.${missing} is required.`);
}

function sanitizeDevicePaths(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  const paths = value
    .filter(isDevicePath)
    .map((path) => (path as string).trim().replace(/\/{2,}/g, "/"));
  return [...new Set(paths)].slice(0, 512);
}

function booleanOr(value: unknown, fallback: boolean): boolean {
  return typeof value === "boolean" ? value : fallback;
}

function nullableStringOr(
  value: unknown,
  maxLength: number,
  fallback: string | null,
): string | null {
  return isNullableString(maxLength)(value) ? (value as string | null) : fallback;
}

function record(value: unknown): RecordValue {
  return isRecord(value) ? value : {};
}

function isRecord(value: unknown): value is RecordValue {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function hasOwn(value: RecordValue, key: string): boolean {
  return Object.prototype.hasOwnProperty.call(value, key);
}

function deepMerge(current: unknown, patch: unknown): unknown {
  if (!isRecord(current) || !isRecord(patch)) return clone(patch);
  const next: RecordValue = { ...current };
  for (const [key, value] of Object.entries(patch)) {
    next[key] = isRecord(value) && isRecord(current[key])
      ? deepMerge(current[key], value)
      : clone(value);
  }
  return next;
}

function jsonEqual(left: unknown, right: unknown): boolean {
  try {
    return JSON.stringify(left) === JSON.stringify(right);
  } catch {
    return false;
  }
}

function clone<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}
