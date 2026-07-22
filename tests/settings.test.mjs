import assert from "node:assert/strict";
import test from "node:test";

import {
  CURRENT_SETTINGS_VERSION,
  createDefaultSettings,
  createSettingsDocument,
  decodeStoredSettings,
  parseImportedSettings,
} from "../src/lib/settingsSchema.ts";
import {
  SettingsRepository,
  SETTINGS_BACKUP_KEY,
  SETTINGS_STORE_KEY,
} from "../src/lib/settingsRepository.ts";

class MemoryStorage {
  constructor(initial) {
    this.data = new Map(Object.entries(initial ?? {}));
  }

  saveCalls = 0;
  failSaveAt = null;
  failGetOnce = false;
  activeMutations = 0;
  maxActiveMutations = 0;
  mutationDelay = null;

  async get(key) {
    if (this.failGetOnce) {
      this.failGetOnce = false;
      throw new Error("corrupt store");
    }
    return structuredClone(this.data.get(key));
  }

  async set(key, value) {
    this.activeMutations += 1;
    this.maxActiveMutations = Math.max(
      this.maxActiveMutations,
      this.activeMutations,
    );
    if (this.mutationDelay) await this.mutationDelay();
    this.data.set(key, structuredClone(value));
    this.activeMutations -= 1;
  }

  async save() {
    this.saveCalls += 1;
    if (this.failSaveAt === this.saveCalls) {
      throw new Error("simulated save failure");
    }
  }
}

test("unversioned settings migrate in order and persist as schema v1", async () => {
  const storage = new MemoryStorage({
    [SETTINGS_STORE_KEY]: {
      language: "en",
      appearance: { appIcon: "dark", themeAccent: "#AABBCC" },
      connection: { transport: "ble", autoReconnect: true },
    },
  });
  const repository = new SettingsRepository(storage);

  const loaded = await repository.load();

  assert.equal(loaded.appearance.appIcon, "dark");
  assert.equal(loaded.appearance.themeAccent, "#aabbcc");
  assert.equal(loaded.connection.transport, "ble");
  assert.equal(loaded.connection.syncClockOnConnect, true);
  assert.equal(storage.data.get(SETTINGS_STORE_KEY).schemaVersion, 1);
  assert.deepEqual(storage.data.get(SETTINGS_STORE_KEY).settings, loaded);
});

test("explicit schema v0 documents use the same ordered legacy migration", () => {
  const decoded = decodeStoredSettings({
    schemaVersion: 0,
    settings: { tray: { enabled: false } },
  });

  assert.equal(decoded.document.schemaVersion, CURRENT_SETTINGS_VERSION);
  assert.equal(decoded.document.settings.tray.enabled, false);
  assert.equal(decoded.document.settings.tray.monochromeIcon, false);
  assert.equal(decoded.needsWrite, true);
});

test("stored malformed values are sanitized and unknown keys are discarded", () => {
  const decoded = decodeStoredSettings({
    language: "not-supported",
    mystery: "drop me",
    apps: {
      excludedDirs: [" /ext/good ", "relative", 42, "/ext/good"],
      extraDirs: "not-an-array",
    },
    tray: { enabled: "yes", unknown: true },
    appearance: { appIcon: "bad icon!", themeAccent: "orange" },
  });

  assert.equal(decoded.document.settings.language, "en");
  assert.deepEqual(decoded.document.settings.apps.excludedDirs, ["/ext/good"]);
  assert.deepEqual(decoded.document.settings.apps.extraDirs, []);
  assert.equal(decoded.document.settings.tray.enabled, true);
  assert.equal(decoded.document.settings.appearance.appIcon, "default");
  assert.equal("mystery" in decoded.document.settings, false);
});

test("an unreadable store recovers to defaults and replaces the corrupt value", async () => {
  const storage = new MemoryStorage({ [SETTINGS_STORE_KEY]: "corrupt" });
  storage.failGetOnce = true;
  const repository = new SettingsRepository(storage);

  assert.deepEqual(await repository.load(), createDefaultSettings());
  assert.deepEqual(
    storage.data.get(SETTINGS_STORE_KEY),
    createSettingsDocument(createDefaultSettings()),
  );
});

test("strict imports reject malformed values and unknown keys before backup", async () => {
  const current = createSettingsDocument(createDefaultSettings());
  const storage = new MemoryStorage({ [SETTINGS_STORE_KEY]: current });
  const repository = new SettingsRepository(storage);
  const malformed = structuredClone(current);
  malformed.settings.tray.enabled = "yes";

  await assert.rejects(
    repository.importJson(JSON.stringify(malformed)),
    /invalid value/,
  );
  const unknown = structuredClone(current);
  unknown.settings.unknown = true;
  await assert.rejects(
    repository.importJson(JSON.stringify(unknown)),
    /not a recognized setting/,
  );
  assert.equal(storage.data.has(SETTINGS_BACKUP_KEY), false);
  assert.deepEqual(storage.data.get(SETTINGS_STORE_KEY), current);
  assert.throws(
    () => parseImportedSettings("{not json"),
    /not valid JSON/,
  );
});

test("failed import restores the current document after writing its backup", async () => {
  const currentSettings = createDefaultSettings();
  currentSettings.appearance.themeAccent = "#123456";
  const current = createSettingsDocument(currentSettings);
  const importedSettings = createDefaultSettings();
  importedSettings.appearance.themeAccent = "#abcdef";
  const imported = createSettingsDocument(importedSettings);
  const storage = new MemoryStorage({ [SETTINGS_STORE_KEY]: current });
  // backup save succeeds, imported document save fails, rollback save succeeds
  storage.failSaveAt = 2;
  const repository = new SettingsRepository(storage);

  await assert.rejects(repository.importJson(JSON.stringify(imported)), /save failure/);

  assert.deepEqual(storage.data.get(SETTINGS_BACKUP_KEY), current);
  assert.deepEqual(storage.data.get(SETTINGS_STORE_KEY), current);
  assert.equal((await repository.load()).appearance.themeAccent, "#123456");
});

test("successful import preserves a backup and reset restores defaults", async () => {
  const currentSettings = createDefaultSettings();
  currentSettings.tray.enabled = false;
  const current = createSettingsDocument(currentSettings);
  const importedSettings = createDefaultSettings();
  importedSettings.connection.autoReconnect = true;
  const imported = createSettingsDocument(importedSettings);
  const storage = new MemoryStorage({ [SETTINGS_STORE_KEY]: current });
  const repository = new SettingsRepository(storage);

  const next = await repository.importJson(JSON.stringify(imported));
  assert.equal(next.connection.autoReconnect, true);
  assert.deepEqual(storage.data.get(SETTINGS_BACKUP_KEY), current);

  const reset = await repository.reset();
  assert.deepEqual(reset, createDefaultSettings());
  assert.deepEqual(
    storage.data.get(SETTINGS_STORE_KEY),
    createSettingsDocument(createDefaultSettings()),
  );
});

test("concurrent writes are serialized and build on the preceding result", async () => {
  const storage = new MemoryStorage({
    [SETTINGS_STORE_KEY]: createSettingsDocument(createDefaultSettings()),
  });
  storage.mutationDelay = () => new Promise((resolve) => setTimeout(resolve, 5));
  const repository = new SettingsRepository(storage);

  const [first, second] = await Promise.all([
    repository.update({ appearance: { themeAccent: "#112233" } }),
    repository.update({ tray: { enabled: false } }),
  ]);

  assert.equal(first.appearance.themeAccent, "#112233");
  assert.equal(second.appearance.themeAccent, "#112233");
  assert.equal(second.tray.enabled, false);
  assert.equal(storage.maxActiveMutations, 1);
  assert.deepEqual(storage.data.get(SETTINGS_STORE_KEY).settings, second);
});
