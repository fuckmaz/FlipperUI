import type { SubGhzEntry } from "../types/subghz";

const FAVORITE_PREFIX = "signal:";

export interface SubGhzCacheSnapshot {
  scannedAt: number;
  entries: SubGhzEntry[];
  favorites: string[];
}

export type SubGhzCacheMutation =
  | { kind: "rename"; oldPath: string; entry: SubGhzEntry }
  | { kind: "delete"; path: string };

/** Stable signal identity: deliberately excludes path, filename, and mtime. */
export function subghzFavoriteIdentity(entry: SubGhzEntry): string {
  const canonical = JSON.stringify([
    entry.frequency,
    entry.preset,
    entry.protocol,
    entry.bit,
    entry.te,
    entry.key,
    entry.modulation,
    entry.has_raw,
    entry.coordinates?.lat ?? null,
    entry.coordinates?.lon ?? null,
  ]);
  return `${FAVORITE_PREFIX}${fnv1a(canonical)}${fnv1a([...canonical].reverse().join(""))}`;
}

/** Convert legacy path stars and discard paths that no longer resolve. */
export function migrateSubghzFavorites(
  favorites: readonly string[] | undefined,
  entries: readonly SubGhzEntry[],
): string[] {
  const byPath = new Map(entries.map((entry) => [entry.path, entry]));
  const available = new Set(entries.map(subghzFavoriteIdentity));
  const migrated = (favorites ?? []).flatMap((favorite) => {
    if (favorite.startsWith(FAVORITE_PREFIX)) {
      return available.has(favorite) ? [favorite] : [];
    }
    const entry = byPath.get(favorite);
    return entry ? [subghzFavoriteIdentity(entry)] : [];
  });
  return [...new Set(migrated)].sort();
}

export function reconcileSubghzFavorites(
  favorites: readonly string[],
  entries: readonly SubGhzEntry[],
): string[] {
  return migrateSubghzFavorites(favorites, entries);
}

/** Update entries and favorite identities as one pure cache transaction. */
export function applySubghzCacheMutation(
  current: SubGhzCacheSnapshot,
  mutation: SubGhzCacheMutation,
): SubGhzCacheSnapshot {
  const migrated = migrateSubghzFavorites(current.favorites, current.entries);
  const entries =
    mutation.kind === "rename"
      ? current.entries.map((entry) =>
          entry.path === mutation.oldPath ? mutation.entry : entry,
        )
      : current.entries.filter((entry) => entry.path !== mutation.path);
  return {
    ...current,
    entries,
    favorites: reconcileSubghzFavorites(migrated, entries),
  };
}

function fnv1a(value: string): string {
  let hash = 0x811c9dc5;
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193);
  }
  return (hash >>> 0).toString(16).padStart(8, "0");
}
