import { useRef, useState, useEffect, useMemo } from "react";
import { listen } from "@tauri-apps/api/event";
import { Terminal as TerminalIcon } from "lucide-react";
import { useFlipperStore } from "../../store/useFlipperStore";
import { cliStart, cliSend, cliInterrupt, cliStop } from "../../lib/tauri";

// Module-level promise tracking the in-flight CLI->RPC handover so RPC calls
// (file browser, etc.) wait for it to finish instead of racing the mode switch.
// Re-assigned on every unmount; cleared once the underlying cliStop resolves.
let cliCleanupPromise: Promise<void> | null = null;

/** Returns the current CLI cleanup promise, if any */
// eslint-disable-next-line react-refresh/only-export-components
export const getCliCleanupPromise = () => cliCleanupPromise;

export function CliPanel() {
  const cliHistory = useFlipperStore((s) => s.cliHistory);
  const cliConnected = useFlipperStore((s) => s.cliConnected);
  const addCliLine = useFlipperStore((s) => s.addCliLine);
  const clearCli = useFlipperStore((s) => s.clearCli);
  const setCliConnected = useFlipperStore((s) => s.setCliConnected);
  const [input, setInput] = useState("");
  const [cmdHistory, setCmdHistory] = useState<string[]>([]);
  const [historyIdx, setHistoryIdx] = useState(-1);
  const scrollRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  // Auto-scroll to bottom when history changes
  useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [cliHistory]);

  // Virtualize CLI output - only render last N lines for performance
  const VISIBLE_LINES = 200;
  const visibleHistory = useMemo(() => {
    if (cliHistory.length <= VISIBLE_LINES) return cliHistory;
    return cliHistory.slice(-VISIBLE_LINES);
  }, [cliHistory]);

  // Enter CLI mode on mount, exit on unmount
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let mounted = true;

    (async () => {
      try {
        unlisten = await listen<string>("cli-output", (event) => {
          if (!mounted) return;
          addCliLine({ type: "output", text: event.payload });
        });

        await cliStart();

        // Send empty command to trigger the CLI prompt
        await new Promise((r) => setTimeout(r, 100));
        await cliSend("").catch(() => {});

        if (mounted) {
          setCliConnected(true);
          setTimeout(() => {
            inputRef.current?.focus({ preventScroll: true });
          }, 10);
        }
      } catch (err) {
        console.warn("[CLI] cliStart failed:", err);
        if (mounted) {
          addCliLine({
            type: "error",
            text: `Failed to enter CLI: ${err}`,
          });
        }
      }
    })();

    return () => {
      mounted = false;
      setCliConnected(false);
      setTimeout(() => unlisten?.(), 0);
      // Hand the device back to RPC mode and publish the in-flight promise
      // so the next RPC call (awaitCliCleanup in lib/tauri.ts) blocks until
      // the mode switch is fully done.
      const p = cliStop()
        .catch(() => {})
        .finally(() => {
          if (cliCleanupPromise === p) cliCleanupPromise = null;
        });
      cliCleanupPromise = p;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Focus input when connected
  useEffect(() => {
    if (cliConnected) inputRef.current?.focus();
  }, [cliConnected]);

  const handleSubmit = async () => {
    const trimmed = input.trim();

    if (trimmed === "clear") {
      clearCli();
      setInput("");
      return;
    }

    if (!cliConnected) return;

    if (!trimmed) {
      await cliSend("").catch(() => {});
      setInput("");
      return;
    }

    setCmdHistory((prev) => [...prev, trimmed]);
    setHistoryIdx(-1);
    setInput("");

    try {
      await cliSend(trimmed);
    } catch (err) {
      addCliLine({ type: "error", text: String(err) });
    }
  };

  const handleKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.ctrlKey && !e.metaKey && e.key.toLowerCase() === "c") {
      const inputElement = e.currentTarget;
      const inputSelection =
        inputElement.selectionStart !== inputElement.selectionEnd;
      const documentSelection = Boolean(window.getSelection()?.toString());

      // Keep the platform's normal copy behavior when the user selected text.
      // With no selection, Ctrl+C has its terminal meaning: send ETX.
      if (inputSelection || documentSelection || !cliConnected) return;

      e.preventDefault();
      setInput("");
      setHistoryIdx(-1);
      void cliInterrupt().catch((err) => {
        addCliLine({ type: "error", text: `Failed to interrupt CLI: ${err}` });
      });
    } else if (e.key === "Enter") {
      handleSubmit();
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      if (cmdHistory.length === 0) return;
      const newIdx =
        historyIdx === -1
          ? cmdHistory.length - 1
          : Math.max(0, historyIdx - 1);
      setHistoryIdx(newIdx);
      setInput(cmdHistory[newIdx]);
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      if (historyIdx === -1) return;
      const newIdx = historyIdx + 1;
      if (newIdx >= cmdHistory.length) {
        setHistoryIdx(-1);
        setInput("");
      } else {
        setHistoryIdx(newIdx);
        setInput(cmdHistory[newIdx]);
      }
    }
  };

  return (
    <div className="flex-1 min-h-0 flex flex-col bg-app overflow-hidden">
      {/* Header */}
      <div className="flex items-center gap-2 px-3 py-1.5 border-b border-border-subtle bg-panel/50 shrink-0">
        <TerminalIcon size={13} className="text-accent" />
        <span className="text-xs text-secondary font-medium">
          Terminal
          {!cliConnected && (
            <span className="ml-2 text-dim">connecting...</span>
          )}
        </span>
      </div>

      {/* Output */}
      <div
        ref={scrollRef}
        className="terminal-font flex-1 overflow-y-auto px-3 py-1.5 text-xs leading-relaxed whitespace-pre-wrap break-all select-text cursor-text"
        onClick={() => inputRef.current?.focus()}
      >
        {visibleHistory.map((line) => (
          <span
            key={line.id}
            className={
              line.type === "error" ? "text-danger" : "text-primary/80"
            }
          >
            {line.text}
          </span>
        ))}
      </div>

      {/* Input */}
      <div className="flex items-center gap-1.5 px-3 py-1.5 border-t border-border-subtle">
        <span className="terminal-font text-xs text-accent shrink-0">&gt;</span>
        <input
          ref={inputRef}
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={handleKeyDown}
          className={`terminal-font flex-1 bg-transparent text-xs outline-none placeholder:text-dim transition-opacity ${
            cliConnected ? "text-primary" : "text-dim opacity-40"
          }`}
          placeholder={cliConnected ? "" : "connecting..."}
          spellCheck={false}
          autoComplete="off"
          aria-label="CLI input"
        />
      </div>
    </div>
  );
}
