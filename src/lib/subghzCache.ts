/**
 * On-disk cache for the Sub-GHz library scan, keyed by device UID.
 *
 * Backed by tauri-plugin-store at `subghz-cache.json`. Each device's cache
 * holds the full list of parsed .sub entries (with mtime) from the last
 * successful scan. The library view loads the cache on mount for instant
 * render, and passes cached entries into the Rust scan so only files whose
 * mtime has moved get re-read over serial.
 *
 * The cache is intentionally *not* mirrored in-memory the way `settings.ts`
 * is — reads/writes are rare (view mount + scan completion) and we want the
 * disk to be the source of truth across reloads.
 */
import { LazyStore } from "@tauri-apps/plugin-store";
import type { SubGhzEntry } from "../types/subghz";
import {
  applySubghzCacheMutation,
  migrateSubghzFavorites,
  reconcileSubghzFavorites,
  type SubGhzCacheMutation,
  type SubGhzCacheSnapshot,
} from "./subghzFavorites";

const STORE_FILE = "subghz-cache.json";

/** Device cache with stable signal identities (legacy path values migrate on load). */
export type DeviceCache = SubGhzCacheSnapshot;

type CacheMap = Record<string, DeviceCache>;

const store = new LazyStore(STORE_FILE, {
  defaults: {},
  autoSave: true,
});

const ROOT_KEY = "cache";
let mutationQueue: Promise<unknown> = Promise.resolve();

async function readAll(): Promise<CacheMap> {
  return (await store.get<CacheMap>(ROOT_KEY)) ?? {};
}

/** Load the cached scan for a device UID, or `null` if never scanned. */
export async function loadSubghzCache(uid: string): Promise<DeviceCache | null> {
  const all = await readAll();
  const current = all[uid];
  if (!current) return null;
  const favorites = migrateSubghzFavorites(current.favorites, current.entries);
  if (!sameStrings(favorites, current.favorites ?? [])) {
    const migrated = { ...current, favorites };
    await mutate(async (latest) => {
      latest[uid] = migrated;
      return migrated;
    });
    return migrated;
  }
  return { ...current, favorites };
}

/** Persist scan results for the given device UID. Preserves favorites. */
export async function saveSubghzCache(
  uid: string,
  entries: SubGhzEntry[],
): Promise<DeviceCache> {
  return mutate(async (all) => {
    const prev = all[uid];
    const next: DeviceCache = {
      scannedAt: Date.now(),
      entries,
      favorites: reconcileSubghzFavorites(
        migrateSubghzFavorites(prev?.favorites, prev?.entries ?? entries),
        entries,
      ),
    };
    all[uid] = next;
    return next;
  });
}

/** Persist favorites for the given device UID. Preserves entries/scannedAt. */
export async function saveSubghzFavorites(
  uid: string,
  favorites: string[],
): Promise<void> {
  await mutate(async (all) => {
    const prev = all[uid];
    const entries = prev?.entries ?? [];
    all[uid] = {
      scannedAt: prev?.scannedAt ?? 0,
      entries,
      favorites: reconcileSubghzFavorites(favorites, entries),
    };
  });
}

export function mutateSubghzCacheEntry(
  uid: string,
  change: SubGhzCacheMutation,
): Promise<DeviceCache> {
  return mutate(async (all) => {
    const current: DeviceCache = all[uid] ?? {
      scannedAt: 0,
      entries: [],
      favorites: [],
    };
    const next = applySubghzCacheMutation(
      { ...current, favorites: current.favorites ?? [] },
      change,
    );
    all[uid] = next;
    return next;
  });
}

/** Drop the cache entry for a specific UID (or all if omitted). */
export async function clearSubghzCache(uid?: string): Promise<void> {
  if (!uid) {
    await mutate(async (all) => {
      for (const key of Object.keys(all)) delete all[key];
    });
    return;
  }
  await mutate(async (all) => {
    delete all[uid];
  });
}

function mutate<T>(operation: (all: CacheMap) => Promise<T> | T): Promise<T> {
  const next = mutationQueue.then(async () => {
    const all = await readAll();
    const result = await operation(all);
    await store.set(ROOT_KEY, all);
    return result;
  });
  mutationQueue = next.then(
    () => undefined,
    () => undefined,
  );
  return next;
}

function sameStrings(left: readonly string[], right: readonly string[]): boolean {
  return left.length === right.length && left.every((value, index) => value === right[index]);
}
