import { useEffect, useRef, useState } from "react";
import { Upload, FolderPlus, RefreshCw, Check, X } from "lucide-react";
import { useFlipperStore } from "../../store/useFlipperStore";
import { useStorage } from "../../hooks/useStorage";

export function Toolbar() {
  const currentPath = useFlipperStore((s) => s.currentPath);
  const isLoading = useFlipperStore((s) => s.isLoading);
  const isDeleting = useFlipperStore((s) => s.fileBrowserDeleting);
  const { upload, mkdir, refresh } = useStorage();

  // Ctrl+U to upload
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === "u") {
        e.preventDefault();
        if (!isDeleting) void upload();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [isDeleting, upload]);

  const [creatingFolder, setCreatingFolder] = useState(false);
  const [folderName, setFolderName] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (isDeleting) {
      setCreatingFolder(false);
      setFolderName("");
    }
  }, [isDeleting]);

  const startCreate = () => {
    if (isDeleting) return;
    setFolderName("");
    setCreatingFolder(true);
    // Focus after render
    setTimeout(() => inputRef.current?.focus(), 0);
  };

  const confirmCreate = async () => {
    if (isDeleting) return;
    const name = folderName.trim();
    setCreatingFolder(false);
    setFolderName("");
    if (name) await mkdir(name);
  };

  const cancelCreate = () => {
    setCreatingFolder(false);
    setFolderName("");
  };

  return (
    <div className="flex items-center gap-1.5 px-3 py-1.5 border-b border-flipper/60 bg-panel/30 min-h-[34px]">
      <button
        onClick={() => void upload()}
        disabled={isDeleting}
        className="flex items-center gap-1.5 px-2.5 py-1 text-xs bg-surface hover:bg-elevated text-primary disabled:opacity-40 disabled:cursor-not-allowed rounded transition-colors shrink-0"
      >
        <Upload size={13} />
        Upload
      </button>

      {creatingFolder ? (
        /* Inline folder-name input */
        <div className="flex items-center gap-1">
          <input
            ref={inputRef}
            value={folderName}
            onChange={(e) => setFolderName(e.target.value)}
            disabled={isDeleting}
            onKeyDown={(e) => {
              if (e.key === "Enter") confirmCreate();
              if (e.key === "Escape") cancelCreate();
            }}
            placeholder="Folder name…"
            className="w-36 px-2 py-0.5 text-xs bg-surface border border-accent/60 text-primary rounded outline-none focus:border-accent"
          />
          <button
            onClick={confirmCreate}
            disabled={isDeleting}
            className="p-1 text-success hover:text-success/80 rounded"
            title="Create"
          >
            <Check size={13} />
          </button>
          <button
            onClick={cancelCreate}
            className="p-1 text-muted hover:text-primary rounded"
            title="Cancel"
          >
            <X size={13} />
          </button>
        </div>
      ) : (
        <button
          onClick={startCreate}
          disabled={isDeleting}
          className="flex items-center gap-1.5 px-2.5 py-1 text-xs bg-surface hover:bg-elevated text-primary disabled:opacity-40 disabled:cursor-not-allowed rounded transition-colors shrink-0"
        >
          <FolderPlus size={13} />
          New Folder
        </button>
      )}

      <div className="flex-1" />
      <span className="text-xs text-muted font-mono truncate max-w-xs">{currentPath}</span>
      <button
        onClick={() => refresh(currentPath)}
        disabled={isLoading || isDeleting}
        className="p-1 text-muted hover:text-primary disabled:opacity-40 rounded transition-colors shrink-0"
        title="Refresh"
      >
        <RefreshCw size={13} className={isLoading ? "animate-spin" : ""} />
      </button>
    </div>
  );
}
