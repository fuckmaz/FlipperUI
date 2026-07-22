export const INPUT_PRESS = 0;
export const INPUT_RELEASE = 1;
export const INPUT_SHORT = 2;
export const INPUT_LONG = 3;
export const INPUT_REPEAT = 4;

export const LONG_PRESS_MS = 350;
export const REPEAT_PRESS_MS = 150;

type PressToken = string;

interface ScreenInputScheduler {
  setTimeout(callback: () => void, delayMs: number): number;
  clearTimeout(timer: number): void;
  setInterval(callback: () => void, delayMs: number): number;
  clearInterval(timer: number): void;
}

interface ActivePress {
  key: number;
  longTimer: number | null;
  repeatTimer: number | null;
  longSent: boolean;
}

export interface ScreenInputController {
  start(token: PressToken, key: number): boolean;
  finish(token: PressToken): boolean;
  cancel(token: PressToken): boolean;
  cancelAll(): void;
  isActive(token: PressToken): boolean;
}

const browserScheduler: ScreenInputScheduler = {
  setTimeout: (callback, delayMs) => window.setTimeout(callback, delayMs),
  clearTimeout: (timer) => window.clearTimeout(timer),
  setInterval: (callback, delayMs) => window.setInterval(callback, delayMs),
  clearInterval: (timer) => window.clearInterval(timer),
};

/**
 * Reproduce the Flipper's physical input lifecycle for every virtual key.
 * A hold stays pressed until the originating pointer/key is actually released;
 * LONG and REPEAT are events within that lifecycle, not self-contained taps.
 */
export function createScreenInputController(
  emit: (key: number, inputType: number) => void,
  scheduler: ScreenInputScheduler = browserScheduler,
): ScreenInputController {
  const activeByToken = new Map<PressToken, ActivePress>();
  const tokenByKey = new Map<number, PressToken>();

  const start = (token: PressToken, key: number): boolean => {
    // One physical Flipper key cannot have two simultaneous owners. This also
    // prevents a pointer press and its keyboard shortcut from double-pressing
    // the same key and corrupting the firmware's input sequence counter.
    if (activeByToken.has(token) || tokenByKey.has(key)) return false;

    const entry: ActivePress = {
      key,
      longTimer: null,
      repeatTimer: null,
      longSent: false,
    };
    activeByToken.set(token, entry);
    tokenByKey.set(key, token);
    emit(key, INPUT_PRESS);

    entry.longTimer = scheduler.setTimeout(() => {
      if (activeByToken.get(token) !== entry) return;
      entry.longTimer = null;
      entry.longSent = true;
      emit(key, INPUT_LONG);
      entry.repeatTimer = scheduler.setInterval(() => {
        if (activeByToken.get(token) === entry) emit(key, INPUT_REPEAT);
      }, REPEAT_PRESS_MS);
    }, LONG_PRESS_MS);
    return true;
  };

  const end = (token: PressToken, sendShort: boolean): boolean => {
    const entry = activeByToken.get(token);
    if (!entry) return false;

    activeByToken.delete(token);
    if (tokenByKey.get(entry.key) === token) tokenByKey.delete(entry.key);
    if (entry.longTimer != null) scheduler.clearTimeout(entry.longTimer);
    if (entry.repeatTimer != null) scheduler.clearInterval(entry.repeatTimer);

    if (sendShort && !entry.longSent) emit(entry.key, INPUT_SHORT);
    emit(entry.key, INPUT_RELEASE);
    return true;
  };

  return {
    start,
    finish: (token) => end(token, true),
    // Cancellation (blur, pointercancel, unmount) balances PRESS with RELEASE
    // but must not turn an interrupted tap into an action.
    cancel: (token) => end(token, false),
    cancelAll: () => {
      for (const token of [...activeByToken.keys()]) end(token, false);
    },
    isActive: (token) => activeByToken.has(token),
  };
}
