import { useEffect, useRef, useState, useCallback } from "react";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { BreadcrumbBar } from "./BreadcrumbBar";
import { Toolbar } from "./Toolbar";
import { FileList } from "./FileList";
import { useFlipperStore } from "../../store/useFlipperStore";
import { useStorage } from "../../hooks/useStorage";
import { cancelTransfer } from "../../lib/tauri";
import { ProgressBar } from "../ui/ProgressBar";
import { Upload } from "lucide-react";

export function FileBrowser() {
  const currentPath = useFlipperStore((s) => s.currentPath);
  const transferProgress = useFlipperStore((s) => s.transferProgress);
  const isDeleting = useFlipperStore((s) => s.fileBrowserDeleting);
  const { refresh, uploadFile } = useStorage();
  const [isDragOver, setIsDragOver] = useState(false);
  // Folder under the cursor during a drag, so we can highlight it and route
  // the drop into it instead of the visible directory.
  const [hoveredFolder, setHoveredFolder] = useState<string | null>(null);
  // Tauri's drop event reports physical pixels; cache the latest devicePixelRatio
  // for the duration of a drag (it doesn't change mid-drag).
  const dprRef = useRef(window.devicePixelRatio || 1);

  const handleCancelTransfer = useCallback(() => {
    cancelTransfer().catch(() => {});
  }, []);

  // Load root directory on mount
  useEffect(() => {
    refresh(currentPath);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Apply (or clear) a highlight ring on the folder row currently under the
  // drag cursor. Uses a single CSS class swap so we don't have to re-render the
  // (virtualized) list on every "over" event.
  useEffect(() => {
    const HILITE = "ring-2";
    const HILITE2 = "ring-accent/70";
    document
      .querySelectorAll<HTMLElement>(`[data-drop-folder].${HILITE}`)
      .forEach((el) => el.classList.remove(HILITE, HILITE2));
    if (!hoveredFolder) return;
    document
      .querySelectorAll<HTMLElement>(
        `[data-drop-folder="${cssEscape(hoveredFolder)}"]`,
      )
      .forEach((el) => el.classList.add(HILITE, HILITE2));
  }, [hoveredFolder]);

  // Listen for native file drag-and-drop events
  useEffect(() => {
    if (isDeleting) {
      setIsDragOver(false);
      setHoveredFolder(null);
    }
  }, [isDeleting]);

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;

    const folderAt = (physX: number, physY: number): string | null => {
      const x = physX / dprRef.current;
      const y = physY / dprRef.current;
      const el = document.elementFromPoint(x, y);
      const row = el?.closest<HTMLElement>("[data-drop-folder]");
      return row?.dataset.dropFolder || null;
    };

    getCurrentWebviewWindow()
      .onDragDropEvent((event) => {
        if (cancelled) return;
        if (isDeleting) {
          setIsDragOver(false);
          setHoveredFolder(null);
          return;
        }
        if (event.payload.type === "enter") {
          dprRef.current = window.devicePixelRatio || 1;
          setIsDragOver(true);
          setHoveredFolder(folderAt(event.payload.position.x, event.payload.position.y));
        } else if (event.payload.type === "over") {
          setHoveredFolder(folderAt(event.payload.position.x, event.payload.position.y));
        } else if (event.payload.type === "leave") {
          setIsDragOver(false);
          setHoveredFolder(null);
        } else if (event.payload.type === "drop") {
          const dest = folderAt(event.payload.position.x, event.payload.position.y);
          setIsDragOver(false);
          setHoveredFolder(null);
          const paths = event.payload.paths;
          (async () => {
            for (const path of paths) {
              await uploadFile(path, dest ?? undefined);
            }
          })();
        }
      })
      .then((fn) => {
        if (cancelled) fn();
        else unlisten = fn;
      });

    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [isDeleting, uploadFile]);

  return (
    <div className="flex flex-col h-full relative">
      <BreadcrumbBar />
      <Toolbar />
      <FileList />
      {transferProgress !== null && (
        <ProgressBar value={transferProgress} onCancel={handleCancelTransfer} />
      )}

      {/* Drag-and-drop overlay. Stays semi-transparent so the user can still
          see (and aim at) the folder rows underneath. `pointer-events-none`
          keeps the OS drag flowing through to elementFromPoint hit-testing. */}
      {isDragOver && !isDeleting && (
        <div className="absolute inset-0 z-40 flex items-end justify-center pb-6 bg-app/30 pointer-events-none">
          <div className="flex flex-col items-center gap-1 px-5 py-3 border-2 border-dashed border-accent/60 bg-panel/90 rounded-xl shadow-xl">
            <div className="flex items-center gap-2 text-accent">
              <Upload size={18} />
              <span className="text-sm font-medium">
                {hoveredFolder ? "Drop into folder" : "Drop to upload here"}
              </span>
            </div>
            <span className="text-[11px] font-mono text-secondary truncate max-w-md">
              {hoveredFolder ?? currentPath}
            </span>
          </div>
        </div>
      )}
    </div>
  );
}

// CSS.escape isn't available in older Tauri webviews; fall back to a manual
// escape that handles the only special chars we'll see in paths (slashes,
// dots, spaces, etc.).
function cssEscape(s: string): string {
  if (typeof CSS !== "undefined" && typeof CSS.escape === "function") {
    return CSS.escape(s);
  }
  return s.replace(/["\\]/g, "\\$&");
}
