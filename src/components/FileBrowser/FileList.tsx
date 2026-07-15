import {
  useEffect,
  useRef,
  useState,
  useCallback,
  useMemo,
} from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import {
  Folder,
  File,
  Download,
  Trash2,
  Pencil,
  Check,
  X,
  Archive,
  ArrowUp,
  ArrowDown,
} from "lucide-react";
import { useFlipperStore } from "../../store/useFlipperStore";
import { useStorage } from "../../hooks/useStorage";
import { useExportDrag } from "../../hooks/useExportDrag";
import { storageTarExtract, storageTimestamp } from "../../lib/tauri";
import { loadSettings, subscribeSettings } from "../../lib/settings";
import { joinPath } from "../../lib/encoding";
import { Spinner } from "../ui/Spinner";
import { ContextMenu, type MenuItem } from "../ui/ContextMenu";
import { ConfirmDialog } from "../ui/ConfirmDialog";
import type { FileEntry } from "../../types/flipper";
import type {
  StorageDeleteFailure,
  StorageDeleteTarget,
  StorageDeleteUnattempted,
} from "../../hooks/useStorage";

const ROW_HEIGHT = 32; // px — fixed height for virtual scrolling

function formatSize(bytes: number): string {
  if (bytes === 0) return "—";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function isTarFile(name: string): boolean {
  const lower = name.toLowerCase();
  return lower.endsWith(".tar") || lower.endsWith(".tar.gz") || lower.endsWith(".tgz");
}

// ── Sort helpers ─────────────────────────────────────────────────────────────

type SortKey = "name" | "size" | "type";
type SortDir = "asc" | "desc";

function sortEntries(entries: FileEntry[], key: SortKey, dir: SortDir): FileEntry[] {
  const sorted = [...entries];
  sorted.sort((a, b) => {
    // Always keep dirs above files
    if (a.file_type !== b.file_type) return b.file_type - a.file_type;

    let cmp = 0;
    if (key === "name") cmp = a.name.localeCompare(b.name);
    else if (key === "size") cmp = a.size - b.size;
    else if (key === "type") {
      const extA = a.name.includes(".") ? a.name.split(".").pop()! : "";
      const extB = b.name.includes(".") ? b.name.split(".").pop()! : "";
      cmp = extA.localeCompare(extB) || a.name.localeCompare(b.name);
    }
    return dir === "asc" ? cmp : -cmp;
  });
  return sorted;
}

// ── File row ─────────────────────────────────────────────────────────────────

interface InlineActions {
  rename: boolean;
  download: boolean;
  delete: boolean;
}

interface FileRowProps {
  entry: FileEntry;
  isRenaming: boolean;
  isSelected: boolean;
  inlineActions: InlineActions;
  disabled: boolean;
  onStartRename: (name: string) => void;
  onDelete: (entry: FileEntry) => void;
  onContextMenu: (e: React.MouseEvent, entry: FileEntry) => void;
  onSelect: (name: string, e: React.MouseEvent) => void;
  style?: React.CSSProperties;
}

// Simple timestamp cache — shared across all FileRow instances
const timestampCache = new Map<string, string>();

function formatTimestamp(epoch: number): string {
  if (epoch === 0) return "—";
  const d = new Date(epoch * 1000);
  return d.toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function FileRow({
  entry,
  isRenaming,
  isSelected,
  inlineActions,
  disabled,
  onStartRename,
  onDelete,
  onContextMenu,
  onSelect,
  style,
}: FileRowProps) {
  const currentPath = useFlipperStore((s) => s.currentPath);
  const setCurrentPath = useFlipperStore((s) => s.setCurrentPath);
  const { refresh, download, rename } = useStorage();
  const isDir = entry.file_type === 1;

  const [renameValue, setRenameValue] = useState(entry.name);
  const [timestamp, setTimestamp] = useState<string | null>(null);
  const renameInputRef = useRef<HTMLInputElement>(null);

  // Fetch timestamp lazily on hover
  const handleMouseEnter = useCallback(() => {
    if (isDir || disabled) return;
    const fullPath = joinPath(currentPath, entry.name);
    const cached = timestampCache.get(fullPath);
    if (cached) {
      setTimestamp(cached);
      return;
    }
    storageTimestamp(fullPath)
      .then((epoch) => {
        const formatted = formatTimestamp(epoch);
        timestampCache.set(fullPath, formatted);
        setTimestamp(formatted);
      })
      .catch(() => {}); // ignore — timestamp is optional
  }, [isDir, disabled, currentPath, entry.name]);

  useEffect(() => {
    if (isRenaming) {
      setRenameValue(entry.name);
      setTimeout(() => {
        renameInputRef.current?.focus();
        renameInputRef.current?.select();
      }, 0);
    }
  }, [isRenaming, entry.name]);

  const commitRename = async () => {
    if (disabled) return;
    onStartRename("");
    await rename(entry.name, renameValue);
  };

  const cancelRename = () => {
    onStartRename("");
    setRenameValue(entry.name);
  };

  const handleClick = (e: React.MouseEvent) => {
    if (isRenaming || disabled) return;
    if (isDir && !e.metaKey && !e.ctrlKey && !e.shiftKey) {
      const newPath = joinPath(currentPath, entry.name);
      setCurrentPath(newPath);
      refresh(newPath);
      return;
    }
    onSelect(entry.name, e);
  };

  const exportDrag = useExportDrag(joinPath(currentPath, entry.name), entry.name);
  const handleDragStart = useCallback(
    (e: React.DragEvent) => {
      if (isDir || disabled) {
        e.preventDefault();
        return;
      }
      void exportDrag(e);
    },
    [isDir, disabled, exportDrag],
  );

  // Folder rows tag themselves with `data-drop-folder` so FileBrowser can
  // hit-test where a native drag landed and upload directly into that folder.
  const dropFolder = isDir ? joinPath(currentPath, entry.name) : undefined;

  return (
    <div
      style={style}
      data-drop-folder={dropFolder}
      draggable={!disabled && !isDir && !isRenaming}
      className={`flex items-center gap-2 px-3 border-b border-border-subtle/60 hover:bg-surface/40 group text-sm ${
        isDir && !isRenaming && !disabled ? "cursor-pointer" : ""
      } ${isSelected ? "bg-surface/60" : ""} ${disabled ? "opacity-60" : ""}`}
      onClick={handleClick}
      onMouseEnter={handleMouseEnter}
      onDragStart={handleDragStart}
      onContextMenu={(e) => {
        e.preventDefault();
        if (disabled) return;
        onContextMenu(e, entry);
      }}
      title={timestamp ?? undefined}
    >
      {isDir ? (
        <Folder size={15} className="text-accent shrink-0" />
      ) : (
        <File size={15} className="text-muted shrink-0" />
      )}

      {isRenaming ? (
        <div
          className="flex items-center gap-1 flex-1"
          onClick={(e) => e.stopPropagation()}
        >
          <input
            ref={renameInputRef}
            value={renameValue}
            onChange={(e) => setRenameValue(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") commitRename();
              if (e.key === "Escape") cancelRename();
            }}
            className="flex-1 px-1.5 py-0.5 text-sm bg-surface border border-accent/60 text-primary rounded outline-none focus:border-accent"
          />
          <button onClick={commitRename} className="p-0.5 text-success hover:text-success/80">
            <Check size={13} />
          </button>
          <button onClick={cancelRename} className="p-0.5 text-muted hover:text-primary">
            <X size={13} />
          </button>
        </div>
      ) : (
        <span className={`flex-1 truncate ${isDir ? "text-accent/80" : "text-primary"}`}>
          {entry.name}
        </span>
      )}

      {!isRenaming && (
        <span className="text-xs text-muted w-16 text-right shrink-0">
          {isDir ? "" : formatSize(entry.size)}
        </span>
      )}

      {!isRenaming && (inlineActions.rename || inlineActions.download || inlineActions.delete) && (
        <div
          className="flex items-center gap-1 opacity-0 group-hover:opacity-100 transition-opacity"
          onClick={(e) => e.stopPropagation()}
        >
          {inlineActions.rename && (
            <button
              disabled={disabled}
              onClick={() => onStartRename(entry.name)}
              className="p-1 text-secondary hover:text-accent disabled:opacity-40 rounded"
              title="Rename (F2)"
            >
              <Pencil size={13} />
            </button>
          )}
          {inlineActions.download && !isDir && (
            <button
              disabled={disabled}
              onClick={() => download(entry.name)}
              className="p-1 text-secondary hover:text-blue-400 disabled:opacity-40 rounded"
              title="Download"
            >
              <Download size={13} />
            </button>
          )}
          {inlineActions.delete && (
            <button
              disabled={disabled}
              onClick={() => onDelete(entry)}
              className="p-1 text-secondary hover:text-danger disabled:opacity-40 rounded"
              title="Delete"
            >
              <Trash2 size={13} />
            </button>
          )}
        </div>
      )}
    </div>
  );
}

// ── Sort header button ───────────────────────────────────────────────────────

function SortHeader({
  label,
  sortKey,
  currentKey,
  currentDir,
  onSort,
  className,
}: {
  label: string;
  sortKey: SortKey;
  currentKey: SortKey;
  currentDir: SortDir;
  onSort: (key: SortKey) => void;
  className?: string;
}) {
  const active = currentKey === sortKey;
  return (
    <button
      onClick={() => onSort(sortKey)}
      className={`flex items-center gap-0.5 hover:text-primary transition-colors ${
        active ? "text-primary" : ""
      } ${className ?? ""}`}
    >
      {label}
      {active &&
        (currentDir === "asc" ? <ArrowUp size={10} /> : <ArrowDown size={10} />)}
    </button>
  );
}

// ── FileList ─────────────────────────────────────────────────────────────────

export function FileList() {
  const entries = useFlipperStore((s) => s.entries);
  const isLoading = useFlipperStore((s) => s.isLoading);
  const currentPath = useFlipperStore((s) => s.currentPath);
  const setError = useFlipperStore((s) => s.setError);
  const isDeleting = useFlipperStore((s) => s.fileBrowserDeleting);
  const setIsDeleting = useFlipperStore((s) => s.setFileBrowserDeleting);
  const { download, downloadFolder, removeMany, refresh } = useStorage();

  const [renamingName, setRenamingName] = useState("");
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number; items: MenuItem[] } | null>(null);
  const [filter, setFilter] = useState("");
  const [sortKey, setSortKey] = useState<SortKey>("name");
  const [sortDir, setSortDir] = useState<SortDir>("asc");
  const [selectedNames, setSelectedNames] = useState<Set<string>>(new Set());
  const [pendingDelete, setPendingDelete] = useState<{
    targets: StorageDeleteTarget[];
    refreshPath: string;
  } | null>(null);
  const [inlineActions, setInlineActions] = useState<InlineActions>({ rename: true, download: true, delete: true });
  const scrollRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    loadSettings().then((s) => setInlineActions(s.fileBrowser.inlineActions));
    return subscribeSettings((s) => setInlineActions(s.fileBrowser.inlineActions));
  }, []);

  // Clear filter + selection + timestamp cache when directory changes
  useEffect(() => {
    setFilter("");
    setSelectedNames(new Set());
    setPendingDelete(null);
    timestampCache.clear();
  }, [currentPath]);

  const handleSort = useCallback(
    (key: SortKey) => {
      if (key === sortKey) {
        setSortDir((d) => (d === "asc" ? "desc" : "asc"));
      } else {
        setSortKey(key);
        setSortDir("asc");
      }
    },
    [sortKey],
  );

  const filteredSorted = useMemo(() => {
    let result = entries;
    if (filter) {
      const lower = filter.toLowerCase();
      result = result.filter((e) => e.name.toLowerCase().includes(lower));
    }
    return sortEntries(result, sortKey, sortDir);
  }, [entries, filter, sortKey, sortDir]);

  // Virtual scrolling
  const virtualizer = useVirtualizer({
    count: filteredSorted.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 10,
  });

  const handleSelect = useCallback(
    (name: string, e: React.MouseEvent) => {
      if (e.metaKey || e.ctrlKey) {
        setSelectedNames((prev) => {
          const next = new Set(prev);
          if (next.has(name)) next.delete(name);
          else next.add(name);
          return next;
        });
      } else if (e.shiftKey && selectedNames.size > 0) {
        // Range select
        const lastSelected = [...selectedNames].pop()!;
        const names = filteredSorted.map((e) => e.name);
        const startIdx = names.indexOf(lastSelected);
        const endIdx = names.indexOf(name);
        if (startIdx !== -1 && endIdx !== -1) {
          const [lo, hi] = startIdx < endIdx ? [startIdx, endIdx] : [endIdx, startIdx];
          const range = names.slice(lo, hi + 1);
          setSelectedNames((prev) => new Set([...prev, ...range]));
        }
      } else {
        setSelectedNames(new Set([name]));
      }
    },
    [selectedNames, filteredSorted],
  );

  const handleExtractTar = useCallback(
    async (entry: FileEntry) => {
      const tarPath = joinPath(currentPath, entry.name);
      try {
        await storageTarExtract(tarPath, currentPath);
        await refresh(currentPath);
      } catch (e: unknown) {
        setError(String(e));
      }
    },
    [currentPath, refresh, setError],
  );

  const requestDelete = useCallback(
    (deleteEntries: FileEntry[]) => {
      if (isDeleting || pendingDelete || deleteEntries.length === 0) return;

      setPendingDelete({
        refreshPath: currentPath,
        targets: deleteEntries.map((entry) => ({
          name: entry.name,
          path: joinPath(currentPath, entry.name),
          isDir: entry.file_type === 1,
        })),
      });
    },
    [currentPath, isDeleting, pendingDelete],
  );

  const confirmDelete = useCallback(async () => {
    const request = pendingDelete;
    if (!request || isDeleting) return;

    // Closing the prompt is the point at which the destructive operation is
    // authorized. Selection remains intact until the batch result is known.
    setPendingDelete(null);
    setRenamingName("");
    setContextMenu(null);
    setIsDeleting(true);

    try {
      const result = await removeMany(request.targets, request.refreshPath);
      const deletedNames = new Set(result.deleted.map((target) => target.name));
      const retryNames = new Set([
        ...result.failed.map((target) => target.name),
        ...result.unattempted.map((target) => target.name),
      ]);

      if (useFlipperStore.getState().currentPath === request.refreshPath) {
        setSelectedNames((previous) => {
          const next = new Set(previous);
          for (const name of deletedNames) next.delete(name);
          // Failed entries remain selected (or become selected for an inline
          // delete) so the user has a clear retry target.
          for (const name of retryNames) next.add(name);
          return next;
        });
      }

      if (result.failed.length > 0 || result.unattempted.length > 0) {
        setError(formatDeleteFailures(
          result.failed,
          result.unattempted,
          request.targets.length,
        ));
      }
    } finally {
      setIsDeleting(false);
    }
  }, [isDeleting, pendingDelete, removeMany, setError, setIsDeleting]);

  const handleContextMenu = useCallback(
    (e: React.MouseEvent, entry: FileEntry) => {
      e.preventDefault();
      if (isDeleting) return;
      const isDir = entry.file_type === 1;
      const items: MenuItem[] = [
        {
          label: "Rename",
          icon: <Pencil size={12} />,
          onClick: () => setRenamingName(entry.name),
        },
        {
          label: "Download",
          icon: <Download size={12} />,
          onClick: () => (isDir ? downloadFolder(entry.name) : download(entry.name)),
        },
      ];
      if (!isDir && isTarFile(entry.name)) {
        items.push({
          label: "Extract here",
          icon: <Archive size={12} />,
          onClick: () => handleExtractTar(entry),
        });
      }
      items.push({ type: "separator" });
      const selectedEntries = entries.filter((candidate) => selectedNames.has(candidate.name));
      const deleteEntries = selectedNames.has(entry.name) && selectedEntries.length > 1
        ? selectedEntries
        : [entry];
      items.push({
        label: deleteEntries.length > 1 ? `Delete ${deleteEntries.length} selected` : "Delete",
        icon: <Trash2 size={12} />,
        onClick: () => requestDelete(deleteEntries),
        danger: true,
      });
      setContextMenu({ x: e.clientX, y: e.clientY, items });
    },
    [download, downloadFolder, entries, handleExtractTar, isDeleting, requestDelete, selectedNames],
  );

  // Keyboard shortcuts — use refs so the listener is stable (registered once)
  const selectedNamesRef = useRef(selectedNames);
  selectedNamesRef.current = selectedNames;
  const entriesRef = useRef(entries);
  entriesRef.current = entries;
  const filteredSortedRef = useRef(filteredSorted);
  filteredSortedRef.current = filteredSorted;
  const requestDeleteRef = useRef(requestDelete);
  requestDeleteRef.current = requestDelete;
  const isDeletingRef = useRef(isDeleting);
  isDeletingRef.current = isDeleting;
  const pendingDeleteRef = useRef(pendingDelete);
  pendingDeleteRef.current = pendingDelete;

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      // Don't trigger if user is typing in an input
      const tag = (e.target as HTMLElement).tagName;
      if (tag === "INPUT" || tag === "TEXTAREA") return;

      // The confirmation dialog owns keyboard input while it is open. In
      // particular, Escape must cancel without clearing the pending selection.
      if (pendingDeleteRef.current) return;
      if (isDeletingRef.current) {
        if (e.key === "Delete" || e.key === "Backspace" || e.key === "F2") {
          e.preventDefault();
        }
        return;
      }

      if (e.key === "Delete" || e.key === "Backspace") {
        // Confirm and delete the selected snapshot as one controlled batch.
        if (
          selectedNamesRef.current.size > 0 &&
          !isDeletingRef.current &&
          !pendingDeleteRef.current
        ) {
          e.preventDefault();
          const selectedEntries = entriesRef.current.filter((entry) =>
            selectedNamesRef.current.has(entry.name),
          );
          requestDeleteRef.current(selectedEntries);
        }
      } else if (e.key === "F2") {
        // Rename first selected
        if (selectedNamesRef.current.size === 1) {
          e.preventDefault();
          setRenamingName([...selectedNamesRef.current][0]);
        }
      } else if ((e.metaKey || e.ctrlKey) && e.key === "a") {
        // Select all
        e.preventDefault();
        setSelectedNames(new Set(filteredSortedRef.current.map((en) => en.name)));
      } else if (e.key === "Escape") {
        setSelectedNames(new Set());
        setRenamingName("");
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, []);

  // Keep the destructive prompt mounted even if a background refresh swaps
  // the list into its loading or empty state.
  const deleteConfirmation = pendingDelete ? (
    <ConfirmDialog
      title={deleteDialogTitle(pendingDelete.targets)}
      message={deleteDialogMessage(pendingDelete.targets)}
      confirmLabel={pendingDelete.targets.length === 1 ? "Delete" : "Delete all"}
      destructive
      onConfirm={() => void confirmDelete()}
      onCancel={() => setPendingDelete(null)}
    />
  ) : null;

  if (isLoading) {
    return (
      <>
        <div className="flex items-center justify-center flex-1 gap-2 text-muted">
          <Spinner size={16} />
          <span className="text-sm">Loading…</span>
        </div>
        {deleteConfirmation}
      </>
    );
  }

  if (entries.length === 0) {
    return (
      <>
        <div className="flex items-center justify-center flex-1 text-dim text-sm">
          {currentPath === "/" ? "No files" : "Empty directory"}
        </div>
        {deleteConfirmation}
      </>
    );
  }

  return (
    <>
      <div className="flex flex-col flex-1 min-h-0">
        {/* Column header with search + sort */}
        <div className="flex items-center gap-2 px-3 py-1 border-b border-flipper bg-panel/60 text-xs text-muted shrink-0">
          <span className="w-4 shrink-0" />
          <SortHeader
            label="Name"
            sortKey="name"
            currentKey={sortKey}
            currentDir={sortDir}
            onSort={handleSort}
            className="flex-1"
          />
          <input
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            placeholder="Filter…"
            className="w-28 px-1.5 py-0.5 text-xs bg-surface border border-flipper text-primary rounded outline-none focus:border-accent/60 placeholder:text-dim"
          />
          <SortHeader
            label="Size"
            sortKey="size"
            currentKey={sortKey}
            currentDir={sortDir}
            onSort={handleSort}
            className="w-16 justify-end shrink-0"
          />
          <span className="w-14 shrink-0" />
        </div>

        {/* Virtualized file list */}
        <div ref={scrollRef} className="flex-1 overflow-y-auto">
          {filteredSorted.length === 0 && filter ? (
            <div className="flex items-center justify-center py-8 text-dim text-sm">
              No matches for &quot;{filter}&quot;
            </div>
          ) : (
            <div
              style={{
                height: virtualizer.getTotalSize(),
                width: "100%",
                position: "relative",
              }}
            >
              {virtualizer.getVirtualItems().map((virtualRow) => {
                const entry = filteredSorted[virtualRow.index];
                return (
                  <FileRow
                    key={entry.name}
                    entry={entry}
                    isRenaming={renamingName === entry.name}
                    isSelected={selectedNames.has(entry.name)}
                    inlineActions={inlineActions}
                    disabled={isDeleting}
                    onStartRename={setRenamingName}
                    onDelete={(deleteEntry) => requestDelete([deleteEntry])}
                    onContextMenu={handleContextMenu}
                    onSelect={handleSelect}
                    style={{
                      position: "absolute",
                      top: 0,
                      left: 0,
                      width: "100%",
                      height: ROW_HEIGHT,
                      transform: `translateY(${virtualRow.start}px)`,
                    }}
                  />
                );
              })}
            </div>
          )}
        </div>

        {/* File count footer */}
        <div className="px-3 py-1 text-xs text-dim border-t border-border-subtle/40 shrink-0">
          {filteredSorted.length} item{filteredSorted.length !== 1 ? "s" : ""}
          {selectedNames.size > 0 && ` · ${selectedNames.size} selected`}
          {isDeleting && " · Deleting…"}
        </div>
      </div>

      {contextMenu && (
        <ContextMenu
          {...contextMenu}
          onClose={() => setContextMenu(null)}
        />
      )}

      {deleteConfirmation}
    </>
  );
}

function deleteDialogTitle(targets: StorageDeleteTarget[]): string {
  if (targets.length === 1) return `Delete ${targets[0].name}?`;
  return `Delete ${targets.length} selected items?`;
}

function deleteDialogMessage(targets: StorageDeleteTarget[]): string {
  const folderCount = targets.filter((target) => target.isDir).length;
  const fileCount = targets.length - folderCount;
  const counts = [
    fileCount > 0 ? `${fileCount} file${fileCount === 1 ? "" : "s"}` : "",
    folderCount > 0 ? `${folderCount} folder${folderCount === 1 ? "" : "s"}` : "",
  ].filter(Boolean).join(" and ");
  const shown = targets.slice(0, 4).map((target) => target.name).join(", ");
  const remainder = targets.length > 4 ? `, and ${targets.length - 4} more` : "";
  const recursiveWarning = folderCount > 0
    ? " Selected folders and everything inside them will be removed."
    : "";

  return `${counts} will be permanently deleted.${recursiveWarning} This cannot be undone. Items: ${shown}${remainder}.`;
}

function formatDeleteFailures(
  failures: StorageDeleteFailure[],
  unattempted: StorageDeleteUnattempted[],
  attemptedCount: number,
): string {
  const deletedCount = attemptedCount - failures.length - unattempted.length;
  const details = failures
    .slice(0, 3)
    .map((failure) => `${failure.name}: ${failure.error}`)
    .join("; ");
  const remainder = failures.length > 3
    ? `; ${failures.length - 3} additional failure${failures.length - 3 === 1 ? "" : "s"}`
    : "";

  const stopped = unattempted.length > 0
    ? ` ${unattempted.length} item${unattempted.length === 1 ? " was" : "s were"} not attempted after the batch stopped: ${unattempted[0].reason}.`
    : "";

  return `Deleted ${deletedCount} of ${attemptedCount} items. Failed or unattempted items remain selected; check the connection and retry them. ${details}${remainder}${stopped}`;
}
