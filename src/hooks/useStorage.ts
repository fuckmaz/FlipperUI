import { useCallback } from "react";
import { save, open } from "@tauri-apps/plugin-dialog";
import {
  storageList,
  storageReadToLocal,
  storageReadDirToLocal,
  storageWriteFromLocal,
  storageMkdir,
  storageDeleteMany,
  storageRename,
} from "../lib/tauri";
import { joinPath } from "../lib/encoding";
import { basename } from "../lib/path";
import { notify } from "../lib/notify";
import { useFlipperStore } from "../store/useFlipperStore";
import {
  commandErrorMessage,
  isCommandCancelled,
} from "../lib/commandError";

export interface StorageDeleteTarget {
  name: string;
  path: string;
  isDir: boolean;
}

export interface StorageDeleteFailure extends StorageDeleteTarget {
  error: string;
  fatal: boolean;
}

export interface StorageDeleteUnattempted extends StorageDeleteTarget {
  reason: string;
}

export interface StorageDeleteBatchResult {
  deleted: StorageDeleteTarget[];
  failed: StorageDeleteFailure[];
  unattempted: StorageDeleteUnattempted[];
}

export function useStorage() {
  const setEntries = useFlipperStore((s) => s.setEntries);
  const setLoading = useFlipperStore((s) => s.setLoading);
  const setError = useFlipperStore((s) => s.setError);
  const setTransferProgress = useFlipperStore((s) => s.setTransferProgress);

  const refresh = useCallback(async (path: string, allowWhileDeleting = false) => {
    if (
      useFlipperStore.getState().fileBrowserDeleting &&
      !allowWhileDeleting
    ) return;
    setLoading(true);
    setError(null);
    try {
      const entries = await storageList(path);
      entries.sort((a, b) => {
        if (a.file_type !== b.file_type) return b.file_type - a.file_type;
        return a.name.localeCompare(b.name);
      });
      setEntries(entries);
    } catch (e: unknown) {
      setError(commandErrorMessage(e, "storage_list"));
    } finally {
      setLoading(false);
    }
  }, [setEntries, setLoading, setError]);

  const download = useCallback(async (name: string) => {
    if (useFlipperStore.getState().fileBrowserDeleting) return;
    const currentPath = useFlipperStore.getState().currentPath;
    const remotePath = joinPath(currentPath, name);
    let failed = false;
    try {
      const savePath = await save({ defaultPath: name });
      if (!savePath) return;

      setTransferProgress(0);

      await storageReadToLocal(remotePath, savePath, (progress) => {
        setTransferProgress(progress.percent);
      });
      setTransferProgress(100);
      void notify("transfer", "Download complete", name);
    } catch (e: unknown) {
      failed = true;
      if (!isCommandCancelled(e)) {
        setError(commandErrorMessage(e, "storage_read_to_local"));
      }
    } finally {
      if (failed) {
        setTransferProgress(null);
      } else {
        setTimeout(() => setTransferProgress(null), 600);
      }
    }
  }, [setError, setTransferProgress]);

  // Recursive folder download: pick a parent directory, then mirror the
  // remote tree under `<picked>/<folderName>`. Reuses the same
  // `download-progress` event stream as single-file downloads.
  const downloadFolder = useCallback(async (name: string) => {
    if (useFlipperStore.getState().fileBrowserDeleting) return;
    const currentPath = useFlipperStore.getState().currentPath;
    const remotePath = joinPath(currentPath, name);
    let failed = false;
    try {
      const parentDir = await open({ directory: true, multiple: false });
      if (!parentDir || Array.isArray(parentDir)) return;
      const sep = parentDir.includes("\\") && !parentDir.includes("/") ? "\\" : "/";
      const localDest = parentDir.endsWith(sep)
        ? `${parentDir}${name}`
        : `${parentDir}${sep}${name}`;

      setTransferProgress(0);

      await storageReadDirToLocal(remotePath, localDest, (progress) => {
        setTransferProgress(progress.percent);
      });
      setTransferProgress(100);
      void notify("transfer", "Folder download complete", name);
    } catch (e: unknown) {
      failed = true;
      if (!isCommandCancelled(e)) {
        setError(commandErrorMessage(e, "storage_read_dir_to_local"));
      }
    } finally {
      if (failed) {
        setTransferProgress(null);
      } else {
        setTimeout(() => setTransferProgress(null), 600);
      }
    }
  }, [setError, setTransferProgress]);

  // `destDir` lets a drop target (folder row) upload into a path that isn't
  // the currently-shown directory. Defaults to currentPath for normal uploads.
  const uploadFile = useCallback(async (localPath: string, destDir?: string) => {
    if (useFlipperStore.getState().fileBrowserDeleting) return;
    const currentPath = useFlipperStore.getState().currentPath;
    const dir = destDir ?? currentPath;
    let failed = false;
    try {
      const fileName = basename(localPath) || "file";
      const remotePath = joinPath(dir, fileName);

      setTransferProgress(0);

      await storageWriteFromLocal(remotePath, localPath, (progress) => {
        setTransferProgress(progress.percent);
      });

      // Only refresh the visible listing when the upload landed there;
      // otherwise the user is still looking at currentPath and we'd flicker.
      if (dir === currentPath) await refresh(currentPath);
      void notify("transfer", "Upload complete", basename(localPath) || "file");
    } catch (e: unknown) {
      failed = true;
      if (!isCommandCancelled(e)) {
        setError(commandErrorMessage(e, "storage_write_from_local"));
      }
    } finally {
      if (failed) {
        setTransferProgress(null);
      } else {
        setTimeout(() => setTransferProgress(null), 600);
      }
    }
  }, [setError, setTransferProgress, refresh]);

  const upload = useCallback(async () => {
    if (useFlipperStore.getState().fileBrowserDeleting) return;
    const selected = await open({ multiple: true });
    if (!selected) return;
    const paths = Array.isArray(selected) ? selected : [selected];
    for (const path of paths) {
      await uploadFile(path);
    }
  }, [uploadFile]);

  const mkdir = useCallback(async (name: string) => {
    if (useFlipperStore.getState().fileBrowserDeleting) return;
    const currentPath = useFlipperStore.getState().currentPath;
    const path = joinPath(currentPath, name);
    try {
      await storageMkdir(path);
      await refresh(currentPath);
    } catch (e: unknown) {
      setError(commandErrorMessage(e, "storage_mkdir"));
    }
  }, [setError, refresh]);

  const rename = useCallback(async (oldName: string, newName: string) => {
    if (useFlipperStore.getState().fileBrowserDeleting) return;
    if (!newName.trim() || newName === oldName) return;
    const currentPath = useFlipperStore.getState().currentPath;
    const oldPath = joinPath(currentPath, oldName);
    const newPath = joinPath(currentPath, newName.trim());
    try {
      await storageRename(oldPath, newPath);
      await refresh(currentPath);
    } catch (e: unknown) {
      setError(commandErrorMessage(e, "storage_rename"));
    }
  }, [setError, refresh]);

  /**
   * Delete a confirmed snapshot of file-browser entries in a controlled,
   * sequential batch. Callers are responsible for obtaining confirmation
   * before invoking this helper.
   *
   * Rust validates the complete batch and holds one client lock while deleting
   * sequentially. A single refresh happens after the result returns.
   */
  const removeMany = useCallback(async (
    targets: StorageDeleteTarget[],
    refreshPath: string,
  ): Promise<StorageDeleteBatchResult> => {
    const byPath = new Map(targets.map((target) => [target.path, target]));
    let result: StorageDeleteBatchResult;

    try {
      const response = await storageDeleteMany(
        targets.map((target) => ({
          path: target.path,
          recursive: target.isDir,
        })),
      );
      result = {
        deleted: response.deleted.map((target) =>
          byPath.get(target.path) ?? {
            name: target.path,
            path: target.path,
            isDir: target.recursive,
          }),
        failed: response.failed.map((failure) => ({
          ...(byPath.get(failure.path) ?? {
            name: failure.path,
            path: failure.path,
            isDir: failure.recursive,
          }),
          error: failure.error,
          fatal: failure.fatal,
        })),
        unattempted: response.unattempted.map((target) => ({
          ...(byPath.get(target.path) ?? {
            name: target.path,
            path: target.path,
            isDir: target.recursive,
          }),
          reason: response.stopped_reason ?? "The delete batch stopped before this item",
        })),
      };
    } catch (e: unknown) {
      const reason = commandErrorMessage(e, "storage_delete_many");
      result = {
        deleted: [],
        failed: [],
        unattempted: targets.map((target) => ({ ...target, reason })),
      };
    }

    const reachedDevice = result.deleted.length > 0 || result.failed.length > 0;
    const connectionWasLost = result.failed.some((failure) => failure.fatal);

    // Refresh once only when the batch reached a still-connected device and
    // the user is still viewing that directory. A fatal disconnect already
    // clears the file list through the global disconnect handler.
    if (
      reachedDevice &&
      !connectionWasLost &&
      useFlipperStore.getState().isConnected &&
      useFlipperStore.getState().currentPath === refreshPath
    ) {
      await refresh(refreshPath, true);
    }

    return result;
  }, [refresh]);

  return {
    refresh,
    download,
    downloadFolder,
    upload,
    uploadFile,
    mkdir,
    rename,
    removeMany,
  };
}
