/** Persisted application settings backed by a versioned Tauri store. */
import { LazyStore } from "@tauri-apps/plugin-store";
import {
  SettingsRepository,
  SETTINGS_BACKUP_KEY,
  SETTINGS_STORE_KEY,
} from "./settingsRepository";
import {
  CURRENT_SETTINGS_VERSION,
  DEFAULT_SETTINGS,
  createSettingsDocument,
  type AppSettings,
  type SettingsPatch,
} from "./settingsSchema";

export {
  CURRENT_SETTINGS_VERSION,
  DEFAULT_SETTINGS,
  SETTINGS_BACKUP_KEY,
  SETTINGS_STORE_KEY,
};
export type {
  AppSettings,
  SettingsDocument,
  SettingsPatch,
  UpdateCheckFrequency,
} from "./settingsSchema";

const store = new LazyStore("settings.json", {
  defaults: {
    [SETTINGS_STORE_KEY]: createSettingsDocument(DEFAULT_SETTINGS),
  },
  // Repository operations call save explicitly and serialize every mutation.
  autoSave: false,
});

const repository = new SettingsRepository(store);

export function loadSettings(): Promise<AppSettings> {
  return repository.load();
}

export function updateSettings(patch: SettingsPatch): Promise<AppSettings> {
  return repository.update(patch);
}

export function resetSettings(): Promise<AppSettings> {
  return repository.reset();
}

export function importSettingsJson(json: string): Promise<AppSettings> {
  return repository.importJson(json);
}

export function exportSettingsJson(): Promise<string> {
  return repository.exportJson();
}

export function subscribeSettings(
  callback: (settings: AppSettings) => void,
): () => void {
  return repository.subscribe(callback);
}

// Exported for support diagnostics and tests without exposing the store itself.
export const SETTINGS_SCHEMA_VERSION = CURRENT_SETTINGS_VERSION;
