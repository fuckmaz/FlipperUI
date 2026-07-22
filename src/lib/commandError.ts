export interface CommandError {
  code: string;
  message: string;
  retryable: boolean;
  operation: string;
  commandId?: number;
  path?: string;
  details?: Record<string, string>;
}

const FALLBACK_CODE = "unknown";

/** Normalize Tauri's rejected value without making decisions from display text. */
export function normalizeCommandError(
  error: unknown,
  fallbackOperation = "unknown",
): CommandError {
  if (isRecord(error)) {
    const code = stringField(error, "code");
    const message = stringField(error, "message");
    const operation = stringField(error, "operation");
    if (code && message && operation && typeof error.retryable === "boolean") {
      return {
        code,
        message,
        retryable: error.retryable,
        operation,
        commandId: numberField(error, "commandId"),
        path: stringField(error, "path"),
        details: stringRecord(error.details),
      };
    }
  }

  return {
    code: FALLBACK_CODE,
    message:
      error instanceof Error
        ? error.message
        : typeof error === "string" && error.length > 0
          ? error
          : "The operation failed with an unrecognized error response",
    retryable: false,
    operation: fallbackOperation,
  };
}

export function hasCommandErrorCode(error: unknown, code: string): boolean {
  return normalizeCommandError(error).code === code;
}

export function isCommandCancelled(error: unknown): boolean {
  return hasCommandErrorCode(error, "cancelled");
}

export function commandErrorMessage(error: unknown, operation = "unknown"): string {
  return normalizeCommandError(error, operation).message;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function stringField(record: Record<string, unknown>, key: string): string | undefined {
  const value = record[key];
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function numberField(record: Record<string, unknown>, key: string): number | undefined {
  const value = record[key];
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0
    ? value
    : undefined;
}

function stringRecord(value: unknown): Record<string, string> | undefined {
  if (!isRecord(value)) return undefined;
  const entries = Object.entries(value).filter(
    (entry): entry is [string, string] => typeof entry[1] === "string",
  );
  return entries.length > 0 ? Object.fromEntries(entries) : undefined;
}
