import { useCallback, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { confirm } from "@tauri-apps/plugin-dialog";
import {
  ArrowUp,
  ArrowDown,
  MapPin,
  Radio,
  Copy,
  Pencil,
  Trash2,
  Check,
  X,
  Star,
} from "lucide-react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { useFlipperStore } from "../../store/useFlipperStore";
import { useExportDrag } from "../../hooks/useExportDrag";
import { relativeDir, parentDir, nextDuplicateName } from "../../lib/path";
import { formatMtime } from "../../lib/format";
import { storageRead, storageRename, storageWrite, storageDelete } from "../../lib/tauri";
import { mutateSubghzCacheEntry, saveSubghzCache } from "../../lib/subghzCache";
import {
  applySubghzCacheMutation,
  subghzFavoriteIdentity,
  type SubGhzCacheMutation,
} from "../../lib/subghzFavorites";
import { ContextMenu, type MenuItem } from "../ui/ContextMenu";
import type { SubGhzEntry } from "../../types/subghz";

const ROW_HEIGHT = 46;
const SUBGHZ_ROOT = "/ext/subghz";

type SortKey = "name" | "frequency" | "protocol" | "modulation" | "mtime";
type SortDir = "asc" | "desc";

interface Props {
  entries: SubGhzEntry[];
  favorites: Set<string>;
  onToggleFavorite: (entry: SubGhzEntry) => void;
}

export function LibraryTable({ entries, favorites, onToggleFavorite }: Props) {
  const [sortKey, setSortKey] = useState<SortKey>("name");
  const [sortDir, setSortDir] = useState<SortDir>("asc");
  const [contextMenu, setContextMenu] = useState<
    { x: number; y: number; items: MenuItem[] } | null
  >(null);

  const sorted = useMemo(
    () => sortEntries(entries, sortKey, sortDir),
    [entries, sortKey, sortDir],
  );

  const scrollRef = useRef<HTMLDivElement>(null);
  const virtualizer = useVirtualizer({
    count: sorted.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 8,
  });

  const openMenu = useCallback((e: React.MouseEvent, items: MenuItem[]) => {
    e.preventDefault();
    setContextMenu({ x: e.clientX, y: e.clientY, items });
  }, []);

  const onHeaderClick = (key: SortKey) => {
    if (key === sortKey) {
      setSortDir((d) => (d === "asc" ? "desc" : "asc"));
    } else {
      setSortKey(key);
      setSortDir("asc");
    }
  };

  if (sorted.length === 0) {
    return (
      <div className="flex-1 flex items-center justify-center text-xs text-dim">
        No .sub files found.
      </div>
    );
  }

  return (
    <div className="flex-1 min-h-0 flex flex-col">
      <HeaderRow
        sortKey={sortKey}
        sortDir={sortDir}
        onHeaderClick={onHeaderClick}
      />
      <div ref={scrollRef} className="flex-1 min-h-0 overflow-y-auto">
        <div
          style={{
            height: `${virtualizer.getTotalSize()}px`,
            position: "relative",
          }}
        >
          {virtualizer.getVirtualItems().map((vi) => {
            const entry = sorted[vi.index];
            return (
              <div
                key={entry.path}
                style={{
                  position: "absolute",
                  top: 0,
                  left: 0,
                  right: 0,
                  height: `${vi.size}px`,
                  transform: `translateY(${vi.start}px)`,
                }}
              >
                <Row
                  entry={entry}
                  starred={favorites.has(subghzFavoriteIdentity(entry))}
                  onToggleFavorite={onToggleFavorite}
                  onContextMenu={openMenu}
                />
              </div>
            );
          })}
        </div>
      </div>
      {contextMenu && (
        <ContextMenu {...contextMenu} onClose={() => setContextMenu(null)} />
      )}
    </div>
  );
}

const GRID_COLS = "grid-cols-[1fr_110px_140px_120px_70px_100px_170px]";

function HeaderRow({
  sortKey,
  sortDir,
  onHeaderClick,
}: {
  sortKey: SortKey;
  sortDir: SortDir;
  onHeaderClick: (k: SortKey) => void;
}) {
  return (
    <div
      className={`grid ${GRID_COLS} gap-2 px-3 py-1.5 text-[10px] uppercase tracking-wide text-muted border-b border-border-subtle bg-panel/60 sticky top-0 z-10`}
    >
      <HeaderCell label="Name / Folder" col="name" sortKey={sortKey} sortDir={sortDir} onClick={onHeaderClick} />
      <HeaderCell label="Frequency" col="frequency" sortKey={sortKey} sortDir={sortDir} onClick={onHeaderClick} align="right" />
      <HeaderCell label="Protocol" col="protocol" sortKey={sortKey} sortDir={sortDir} onClick={onHeaderClick} />
      <span className="truncate">Preset</span>
      <HeaderCell label="Mod" col="modulation" sortKey={sortKey} sortDir={sortDir} onClick={onHeaderClick} />
      <HeaderCell label="Modified" col="mtime" sortKey={sortKey} sortDir={sortDir} onClick={onHeaderClick} align="right" />
      <span className="text-right">Actions</span>
    </div>
  );
}

function HeaderCell({
  label,
  col,
  sortKey,
  sortDir,
  onClick,
  align,
}: {
  label: string;
  col: SortKey;
  sortKey: SortKey;
  sortDir: SortDir;
  onClick: (k: SortKey) => void;
  align?: "right";
}) {
  const active = col === sortKey;
  return (
    <button
      onClick={() => onClick(col)}
      className={`flex items-center gap-1 hover:text-primary transition-colors ${
        align === "right" ? "justify-end" : ""
      } ${active ? "text-secondary" : ""}`}
    >
      <span>{label}</span>
      {active && (sortDir === "asc" ? <ArrowUp size={10} /> : <ArrowDown size={10} />)}
    </button>
  );
}

function Row({
  entry,
  starred,
  onToggleFavorite,
  onContextMenu,
}: {
  entry: SubGhzEntry;
  starred: boolean;
  onToggleFavorite: (entry: SubGhzEntry) => void;
  onContextMenu: (e: React.MouseEvent, items: MenuItem[]) => void;
}) {
  const setError = useFlipperStore((s) => s.setSubghzError);
  const setEntries = useFlipperStore((s) => s.setSubghzEntries);
  const setFavorites = useFlipperStore((s) => s.setSubghzFavorites);
  const deviceUid = useFlipperStore((s) => s.deviceInfo?.hardware_uid ?? null);

  const [renaming, setRenaming] = useState(false);
  const [renameValue, setRenameValue] = useState("");
  const [busy, setBusy] = useState<"rename" | "dup" | "delete" | null>(null);

  const relDir = relativeDir(entry.path, SUBGHZ_ROOT);
  const handleDragStart = useExportDrag(entry.path, entry.name);

  const persistList = async (next: SubGhzEntry[]) => {
    if (deviceUid) {
      const cache = await saveSubghzCache(deviceUid, next);
      setEntries(cache.entries);
      setFavorites(cache.favorites);
      return;
    }
    setEntries(next);
  };

  const persistMutation = async (change: SubGhzCacheMutation) => {
    if (deviceUid) {
      const cache = await mutateSubghzCacheEntry(deviceUid, change);
      setEntries(cache.entries);
      setFavorites(cache.favorites);
      return;
    }
    const state = useFlipperStore.getState();
    const next = applySubghzCacheMutation(
      {
        scannedAt: 0,
        entries: state.subghzEntries,
        favorites: state.subghzFavorites,
      },
      change,
    );
    setEntries(next.entries);
    setFavorites(next.favorites);
  };

  const onMaps = async () => {
    if (!entry.coordinates) return;
    const { lat, lon } = entry.coordinates;
    try {
      await openUrl(`https://www.google.com/maps/?q=${lat},${lon}`);
    } catch (e) {
      setError(`Could not open Maps: ${(e as Error).message}`);
    }
  };

  const startRename = () => {
    setRenameValue(entry.name);
    setRenaming(true);
  };

  const cancelRename = () => {
    setRenaming(false);
    setRenameValue("");
  };

  const commitRename = async () => {
    const newName = renameValue.trim();
    if (!newName || newName === entry.name) {
      cancelRename();
      return;
    }
    if (!newName.toLowerCase().endsWith(".sub")) {
      setError("Filename must end with .sub");
      return;
    }
    const parent = parentDir(entry.path);
    const newPath = `${parent}/${newName}`;
    const currentEntries = useFlipperStore.getState().subghzEntries;
    if (currentEntries.some((e) => e.path === newPath)) {
      setError(`A file named "${newName}" already exists here.`);
      return;
    }
    setBusy("rename");
    try {
      await storageRename(entry.path, newPath);
      await persistMutation({
        kind: "rename",
        oldPath: entry.path,
        entry: { ...entry, path: newPath, name: newName },
      });
      setRenaming(false);
    } catch (e) {
      setError(`Rename failed: ${(e as Error).message || String(e)}`);
    } finally {
      setBusy(null);
    }
  };

  const onDuplicate = async () => {
    setBusy("dup");
    try {
      const parent = parentDir(entry.path);
      const currentEntries = useFlipperStore.getState().subghzEntries;
      const existingNames = new Set(
        currentEntries.filter((e) => parentDir(e.path) === parent).map((e) => e.name),
      );
      const newName = nextDuplicateName(entry.name, existingNames);
      const newPath = `${parent}/${newName}`;
      // File content is identical — read the source, write to the new path,
      // and mirror the parsed metadata in-memory so we don't need a re-scan.
      const b64 = await storageRead(entry.path);
      await storageWrite(newPath, b64);
      const duplicate: SubGhzEntry = {
        ...entry,
        path: newPath,
        name: newName,
        mtime: null,
      };
      await persistList([...currentEntries, duplicate]);
    } catch (e) {
      setError(`Duplicate failed: ${(e as Error).message || String(e)}`);
    } finally {
      setBusy(null);
    }
  };

  const onDelete = async () => {
    const ok = await confirm(`Delete ${entry.name}?\n\nThis can't be undone.`, {
      title: "Delete .sub file",
      kind: "warning",
      okLabel: "Delete",
      cancelLabel: "Cancel",
    });
    if (!ok) return;
    setBusy("delete");
    try {
      await storageDelete(entry.path, false);
      await persistMutation({ kind: "delete", path: entry.path });
    } catch (e) {
      setError(`Delete failed: ${(e as Error).message || String(e)}`);
    } finally {
      setBusy(null);
    }
  };

  const handleContextMenu = (e: React.MouseEvent) => {
    if (renaming) return;
    const items: MenuItem[] = [
      {
        label: starred ? "Unstar" : "Star",
        icon: <Star size={12} className={starred ? "fill-accent text-accent" : ""} />,
        onClick: () => onToggleFavorite(entry),
      },
    ];
    if (entry.coordinates) {
      items.push({
        label: "Open in Maps",
        icon: <MapPin size={12} />,
        onClick: onMaps,
      });
    }
    items.push(
      { label: "Rename", icon: <Pencil size={12} />, onClick: startRename },
      { label: "Duplicate", icon: <Copy size={12} />, onClick: onDuplicate },
      { type: "separator" },
      {
        label: "Delete",
        icon: <Trash2 size={12} />,
        onClick: onDelete,
        danger: true,
      },
    );
    onContextMenu(e, items);
  };

  return (
    <div
      className={`group grid ${GRID_COLS} gap-2 px-3 h-full items-center text-xs border-b border-border-subtle/50 hover:bg-surface/40 transition-colors`}
      draggable={!renaming}
      onDragStart={handleDragStart}
      onContextMenu={handleContextMenu}
    >
      <div className="flex flex-col min-w-0">
        {renaming ? (
          <div className="flex items-center gap-1">
            <input
              autoFocus
              value={renameValue}
              onChange={(e) => setRenameValue(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") commitRename();
                if (e.key === "Escape") cancelRename();
              }}
              disabled={busy === "rename"}
              className="flex-1 min-w-0 bg-surface border border-accent/60 rounded px-1.5 py-0.5 text-xs text-primary outline-none"
            />
            <button
              onClick={commitRename}
              disabled={busy === "rename"}
              aria-label="Confirm rename"
              className="p-0.5 text-success hover:text-success/80"
              title="Save (Enter)"
            >
              <Check size={12} />
            </button>
            <button
              onClick={cancelRename}
              disabled={busy === "rename"}
              aria-label="Cancel rename"
              className="p-0.5 text-muted hover:text-primary"
              title="Cancel (Esc)"
            >
              <X size={12} />
            </button>
          </div>
        ) : (
          <span className="truncate text-primary" title={entry.path}>
            {entry.name}
          </span>
        )}
        <span
          className="truncate text-[10px] text-dim"
          title={relDir ? `${SUBGHZ_ROOT}/${relDir}` : SUBGHZ_ROOT}
        >
          {relDir || "/"}
        </span>
      </div>
      <span className="text-right text-secondary tabular-nums">
        {formatFreq(entry.frequency)}
      </span>
      <span className="text-secondary truncate" title={entry.protocol ?? ""}>
        {entry.protocol ?? "—"}
        {entry.has_raw && entry.protocol !== "RAW" && (
          <span className="ml-1 text-[10px] text-dim">(raw)</span>
        )}
      </span>
      <span className="text-dim text-[11px] truncate" title={entry.preset ?? ""}>
        {shortPreset(entry.preset)}
      </span>
      <span className="text-secondary">{entry.modulation ?? "—"}</span>
      <span
        className="text-right text-dim tabular-nums text-[11px]"
        title={entry.mtime ? new Date(entry.mtime * 1000).toLocaleString() : ""}
      >
        {formatMtime(entry.mtime)}
      </span>
      <div className="flex items-center justify-end gap-0.5">
        <button
          onClick={() => onToggleFavorite(entry)}
          title={starred ? "Unstar" : "Star"}
          aria-label={starred ? "Unstar" : "Star"}
          aria-pressed={starred}
          className={`p-1 rounded transition-all ${
            starred
              ? "text-accent opacity-100"
              : "text-muted opacity-0 group-hover:opacity-100 hover:text-accent focus:opacity-100"
          }`}
        >
          <Star size={13} className={starred ? "fill-accent" : ""} />
        </button>
        {entry.coordinates && (
          <button
            onClick={onMaps}
            title={`Open in Google Maps (${entry.coordinates.lat.toFixed(4)}, ${entry.coordinates.lon.toFixed(4)})`}
            className="p-1 text-muted hover:text-accent rounded transition-colors"
          >
            <MapPin size={13} />
          </button>
        )}
        <button
          onClick={startRename}
          disabled={busy !== null || renaming}
          title="Rename"
          aria-label="Rename"
          className="p-1 text-muted hover:text-primary rounded transition-colors disabled:opacity-30"
        >
          <Pencil size={13} />
        </button>
        <button
          onClick={onDuplicate}
          disabled={busy !== null || renaming}
          title="Duplicate"
          aria-label="Duplicate"
          className="p-1 text-muted hover:text-primary rounded transition-colors disabled:opacity-30"
        >
          <Copy size={13} />
        </button>
        <button
          onClick={onDelete}
          disabled={busy !== null || renaming}
          title="Delete"
          aria-label="Delete"
          className="p-1 text-muted hover:text-danger rounded transition-colors disabled:opacity-30"
        >
          <Trash2 size={13} />
        </button>
        <button
          disabled
          title="One-click TX is temporarily disabled — WIP, finishing the RPC flow"
          className="flex items-center gap-1 px-2 py-0.5 text-[11px] rounded border border-border-subtle text-dim opacity-40 cursor-not-allowed"
        >
          <Radio size={10} />
          TX
        </button>
      </div>
    </div>
  );
}

function formatFreq(hz: number | null): string {
  if (hz == null) return "—";
  return `${(hz / 1_000_000).toFixed(3)} MHz`;
}

function shortPreset(p: string | null): string {
  if (!p) return "—";
  // FuriHalSubGhzPresetOok650Async → Ook650Async
  return p.replace(/^FuriHalSubGhzPreset/i, "");
}



// Append " 1", " 2", … before the extension until the name is free. qFlipper
// uses " (1)"; spaces + parens work on Flipper's FAT32 store but trip some
// users up — simpler numeric suffix matches the rest of the app.

function sortEntries(
  entries: SubGhzEntry[],
  key: SortKey,
  dir: SortDir,
): SubGhzEntry[] {
  const out = [...entries];
  out.sort((a, b) => {
    if (key === "mtime") {
      if (a.mtime == null && b.mtime == null) return 0;
      if (a.mtime == null) return 1;
      if (b.mtime == null) return -1;
      const c = a.mtime - b.mtime;
      return dir === "asc" ? c : -c;
    }
    let cmp = 0;
    switch (key) {
      case "name":
        cmp = a.name.localeCompare(b.name);
        break;
      case "frequency":
        cmp = (a.frequency ?? 0) - (b.frequency ?? 0);
        break;
      case "protocol":
        cmp = (a.protocol ?? "").localeCompare(b.protocol ?? "");
        break;
      case "modulation":
        cmp = (a.modulation ?? "").localeCompare(b.modulation ?? "");
        break;
    }
    return dir === "asc" ? cmp : -cmp;
  });
  return out;
}
