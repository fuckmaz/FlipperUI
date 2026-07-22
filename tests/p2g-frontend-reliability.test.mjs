import assert from "node:assert/strict";
import test from "node:test";

import { DeviceTelemetryService } from "../src/lib/deviceTelemetryCore.ts";
import {
  connectPreferredUsbIdentity,
  normalizeUsbIdentityPreferences,
  orderUsbCandidates,
  rememberUsbIdentity,
} from "../src/lib/usbDeviceIdentity.ts";
import {
  applySubghzCacheMutation,
  migrateSubghzFavorites,
  subghzFavoriteIdentity,
} from "../src/lib/subghzFavorites.ts";
import {
  MAX_SUPPORT_BUNDLE_BYTES,
  MAX_SUPPORT_DIAGNOSTICS,
  buildSupportBundle,
  serializeSupportBundle,
} from "../src/lib/supportBundle.ts";
import { createDefaultSettings } from "../src/lib/settingsSchema.ts";

class FakeScheduler {
  nextId = 1;
  nowValue = 1_000;
  intervals = new Map();

  setInterval(callback, delayMs) {
    const id = this.nextId++;
    this.intervals.set(id, { callback, delayMs });
    return id;
  }

  clearInterval(id) {
    this.intervals.delete(id);
  }

  now() {
    return this.nowValue;
  }

  fire(delayMs) {
    for (const interval of this.intervals.values()) {
      if (interval.delayMs === delayMs) interval.callback();
    }
  }
}

class FakeVisibility {
  visible = true;
  listeners = new Set();

  isVisible() {
    return this.visible;
  }

  subscribe(callback) {
    this.listeners.add(callback);
    return () => this.listeners.delete(callback);
  }

  set(visible) {
    this.visible = visible;
    for (const listener of this.listeners) listener(visible);
  }
}

function telemetryHarness() {
  const calls = { power: 0, storage: 0, internal: 0, ping: 0, info: 0 };
  const readers = {
    powerInfo: async () => {
      calls.power += 1;
      return { charge: "75" };
    },
    storageInfo: async () => {
      calls.storage += 1;
      return { total_space: 100, free_space: 40 };
    },
    storageDu: async () => {
      calls.internal += 1;
      return 12;
    },
    ping: async () => {
      calls.ping += 1;
      return 8;
    },
    deviceInfoAll: async () => {
      calls.info += 1;
      return { hardware_name: "Flipper" };
    },
  };
  const scheduler = new FakeScheduler();
  const visibility = new FakeVisibility();
  const service = new DeviceTelemetryService(readers, scheduler, visibility, {
    slowIntervalMs: 30_000,
    pingIntervalMs: 4_000,
  });
  return { calls, scheduler, visibility, service };
}

test("telemetry has one bounded owner and stops all polling while hidden", async () => {
  const { calls, scheduler, visibility, service } = telemetryHarness();
  service.setConnection("uid:port");
  await service.refresh(true);
  assert.equal(scheduler.intervals.size, 2);
  assert.deepEqual(service.getSnapshot().power, { charge: "75" });
  assert.deepEqual(service.getSnapshot().deviceInfo, { hardware_name: "Flipper" });

  visibility.set(false);
  assert.equal(scheduler.intervals.size, 0);
  const hiddenCalls = { ...calls };
  await service.refresh(true);
  await service.refreshPing();
  assert.deepEqual(calls, hiddenCalls);

  visibility.set(true);
  await service.refresh(true);
  assert.equal(scheduler.intervals.size, 2);
  assert.ok(calls.power > hiddenCalls.power);
  service.dispose();
});

test("telemetry coalesces repeated requests instead of growing a work queue", async () => {
  const { calls, service } = telemetryHarness();
  service.setConnection("uid:port");
  await Promise.all([
    service.refresh(),
    service.refresh(),
    service.refresh(true),
    service.refreshPing(),
    service.refreshPing(),
  ]);
  assert.ok(calls.power <= 2, `unexpected refresh count ${calls.power}`);
  assert.ok(calls.ping <= 2, `unexpected ping count ${calls.ping}`);
  assert.equal(calls.info, 1);
  service.dispose();
});

const ports = [
  { name: "/dev/flipper-a", is_flipper: true, vid: 1, pid: 2, manufacturer: "Flipper" },
  { name: "/dev/flipper-b", is_flipper: true, vid: 1, pid: 2, manufacturer: "Flipper" },
];

function device(port, uid) {
  return {
    port,
    hardware_name: "Flipper",
    hardware_version: "1",
    hardware_uid: uid,
    firmware_version: "1.0",
    firmware_build_date: null,
    capabilities: {},
  };
}

test("legacy raw USB port migrates to a UID mapping after the first handshake", async () => {
  const legacy = normalizeUsbIdentityPreferences(null, "/dev/flipper-b");
  assert.deepEqual(orderUsbCandidates(ports, legacy), [
    "/dev/flipper-b",
    "/dev/flipper-a",
  ]);
  const result = await connectPreferredUsbIdentity(
    ports,
    legacy,
    async (port) => device(port, "UID-B"),
    async () => {},
  );
  assert.equal(result.port, "/dev/flipper-b");
  assert.equal(result.preferences.preferredUid, "UID-B");
  assert.equal(result.preferences.portByUid["UID-B"], "/dev/flipper-b");
  assert.equal(result.preferences.legacyPort, null);
});

test("UID reconnect rejects and disconnects a stale raw-port match", async () => {
  const saved = rememberUsbIdentity(
    normalizeUsbIdentityPreferences(null, null),
    "UID-B",
    "/dev/flipper-a",
  );
  const attempts = [];
  let disconnects = 0;
  const result = await connectPreferredUsbIdentity(
    ports,
    saved,
    async (port) => {
      attempts.push(port);
      return device(port, port.endsWith("a") ? "UID-A" : "UID-B");
    },
    async () => {
      disconnects += 1;
    },
  );
  assert.deepEqual(attempts, ["/dev/flipper-a", "/dev/flipper-b"]);
  assert.equal(disconnects, 1);
  assert.equal(result.info.hardware_uid, "UID-B");
  assert.equal(result.preferences.portByUid["UID-B"], "/dev/flipper-b");
});

function signal(path, key = "AA BB") {
  return {
    path,
    name: path.split("/").at(-1),
    frequency: 433_920_000,
    preset: "FuriHalSubGhzPresetOok650Async",
    protocol: "Princeton",
    bit: 24,
    te: 350,
    key,
    modulation: "OOK",
    coordinates: null,
    has_raw: false,
    mtime: 1,
  };
}

test("legacy path favorites become rename-stable signal identities", () => {
  const original = signal("/ext/subghz/gate.sub");
  const favorites = migrateSubghzFavorites([original.path, "/missing.sub"], [original]);
  assert.deepEqual(favorites, [subghzFavoriteIdentity(original)]);

  const renamed = { ...original, path: "/ext/subghz/front-gate.sub", name: "front-gate.sub" };
  const next = applySubghzCacheMutation(
    { scannedAt: 1, entries: [original], favorites },
    { kind: "rename", oldPath: original.path, entry: renamed },
  );
  assert.deepEqual(next.favorites, favorites);
  assert.equal(next.entries[0].path, renamed.path);
});

test("delete removes a favorite atomically only when its signal identity disappears", () => {
  const first = signal("/ext/subghz/one.sub");
  const duplicate = signal("/ext/subghz/two.sub");
  const favorite = subghzFavoriteIdentity(first);
  const oneLeft = applySubghzCacheMutation(
    { scannedAt: 1, entries: [first, duplicate], favorites: [favorite] },
    { kind: "delete", path: first.path },
  );
  assert.deepEqual(oneLeft.favorites, [favorite]);
  const noneLeft = applySubghzCacheMutation(oneLeft, {
    kind: "delete",
    path: duplicate.path,
  });
  assert.deepEqual(noneLeft.favorites, []);
  assert.deepEqual(noneLeft.entries, []);
});

test("support bundle is bounded and excludes identifiers, paths, details, payloads, and secrets", () => {
  const settings = createDefaultSettings();
  settings.connection.lastPort = "/dev/secret-port";
  settings.connection.lastBleId = "secret-ble-id";
  settings.screenStream.screenshotDir = "/Users/private/screens";
  settings.subghz.excludedDirs = ["/ext/subghz/private"];
  const diagnostics = Array.from({ length: 1_000 }, (_, index) => ({
    ts_ms: index,
    dir: "Tx",
    command_id: index,
    content_kind: "StorageReadResponse",
    has_next: false,
    payload_bytes: 1_000_000,
    command_status: 0,
    command_status_name: "OK",
    detail: `path=/ext/private/${index} token=SUPER_SECRET payload=AAAA`,
  }));
  const telemetry = {
    connectionKey: "UID-SECRET:/dev/secret-port",
    power: { charge: "80", api_token: "POWER_SECRET" },
    storage: { total_space: 100, free_space: 20 },
    internalBytes: 12,
    latency: 5,
    deviceInfo: { hardware_uid: "UID-SECRET" },
    refreshedAt: 1_000,
    loading: false,
    errors: {
      power: "token=TELEMETRY_SECRET path=/Users/private",
      storage: null,
      internal: null,
      latency: null,
      deviceInfo: null,
    },
  };
  const input = {
    generatedAt: new Date("2026-07-22T05:00:00Z"),
    appVersion: "1.2.3",
    platform: "test",
    userAgent: "agent",
    settings,
    device: device("/dev/secret-port", "UID-SECRET"),
    connectionKind: "serial",
    telemetry,
    diagnostics,
  };
  const bundle = buildSupportBundle(input);
  const serialized = serializeSupportBundle(input);
  assert.ok(bundle.diagnostics.entries.length <= MAX_SUPPORT_DIAGNOSTICS);
  assert.equal(bundle.diagnostics.truncated, true);
  assert.ok(new TextEncoder().encode(serialized).byteLength <= MAX_SUPPORT_BUNDLE_BYTES);
  for (const secret of [
    "UID-SECRET",
    "/dev/secret-port",
    "secret-ble-id",
    "/Users/private/screens",
    "/ext/private",
    "SUPER_SECRET",
    "POWER_SECRET",
    "TELEMETRY_SECRET",
    "payload=AAAA",
  ]) {
    assert.equal(serialized.includes(secret), false, `leaked ${secret}`);
  }
});
