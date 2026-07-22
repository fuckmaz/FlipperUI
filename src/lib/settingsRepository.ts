import {
  applySettingsPatch,
  createDefaultSettings,
  createSettingsDocument,
  decodeStoredSettings,
  parseImportedSettings,
  serializeSettingsDocument,
  type AppSettings,
  type SettingsDocument,
  type SettingsPatch,
} from "./settingsSchema.ts";

export const SETTINGS_STORE_KEY = "app";
export const SETTINGS_BACKUP_KEY = "app.backup-before-import";

export interface SettingsStorage {
  get(key: string): Promise<unknown>;
  set(key: string, value: unknown): Promise<void>;
  save(): Promise<void>;
}

/**
 * Serialized repository for every settings mutation. The adapter is injected
 * so migrations and failure recovery can be exercised without a Tauri runtime.
 */
export class SettingsRepository {
  private readonly storage: SettingsStorage;
  private cached: AppSettings | null = null;
  private readonly listeners = new Set<(settings: AppSettings) => void>();
  private queue: Promise<void> = Promise.resolve();

  constructor(storage: SettingsStorage) {
    this.storage = storage;
  }

  load(): Promise<AppSettings> {
    return this.enqueue(() => this.loadNow());
  }

  update(patch: SettingsPatch): Promise<AppSettings> {
    return this.enqueue(async () => {
      const current = await this.loadNow();
      const next = applySettingsPatch(current, patch);
      await this.persist(createSettingsDocument(next));
      return this.publish(next);
    });
  }

  reset(): Promise<AppSettings> {
    return this.enqueue(async () => {
      const defaults = createDefaultSettings();
      await this.persist(createSettingsDocument(defaults));
      return this.publish(defaults);
    });
  }

  async importJson(json: string): Promise<AppSettings> {
    // Validate before joining the write queue so an invalid file can never
    // create a backup or mutate persistent state.
    const imported = parseImportedSettings(json);
    return this.enqueue(async () => {
      const current = await this.loadNow();
      const previousDocument = createSettingsDocument(current);

      await this.storage.set(SETTINGS_BACKUP_KEY, previousDocument);
      await this.storage.save();

      try {
        await this.persist(imported);
      } catch (importError) {
        try {
          await this.persist(previousDocument);
        } catch (rollbackError) {
          throw new Error(
            `Settings import failed (${errorMessage(importError)}) and rollback also failed: ${errorMessage(rollbackError)}`,
          );
        }
        throw importError;
      }

      return this.publish(imported.settings);
    });
  }

  async exportJson(): Promise<string> {
    const settings = await this.load();
    return serializeSettingsDocument(createSettingsDocument(settings));
  }

  subscribe(listener: (settings: AppSettings) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  private async loadNow(): Promise<AppSettings> {
    if (this.cached) return this.cached;

    let raw: unknown;
    try {
      raw = await this.storage.get(SETTINGS_STORE_KEY);
    } catch {
      const defaults = createDefaultSettings();
      // Keep the app usable when the underlying file is corrupt. Replacement
      // is best-effort because the same filesystem error may prevent saving.
      try {
        await this.persist(createSettingsDocument(defaults));
      } catch {
        // The next mutation will retry and surface its error to the caller.
      }
      this.cached = defaults;
      return defaults;
    }

    const decoded = decodeStoredSettings(raw);
    if (decoded.needsWrite) {
      try {
        await this.persist(decoded.document);
      } catch {
        // A sanitized in-memory recovery is safer than failing application
        // startup. Explicit updates still report persistence failures.
      }
    }
    this.cached = decoded.document.settings;
    return this.cached;
  }

  private async persist(document: SettingsDocument): Promise<void> {
    await this.storage.set(SETTINGS_STORE_KEY, document);
    await this.storage.save();
  }

  private publish(settings: AppSettings): AppSettings {
    this.cached = settings;
    for (const listener of this.listeners) {
      try {
        listener(settings);
      } catch {
        // A broken UI subscriber must not turn a completed durable write into
        // an apparent settings failure.
      }
    }
    return settings;
  }

  private enqueue<T>(operation: () => Promise<T>): Promise<T> {
    const result = this.queue.then(operation);
    this.queue = result.then(
      () => undefined,
      () => undefined,
    );
    return result;
  }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
