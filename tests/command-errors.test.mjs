import assert from "node:assert/strict";
import test from "node:test";

import {
  isCommandCancelled,
  normalizeCommandError,
} from "../src/lib/commandError.ts";
import {
  availabilityFromCommandError,
  resolveFeatureAvailability,
} from "../src/lib/capabilities.ts";

test("structured command errors normalize without losing safe metadata", () => {
  const normalized = normalizeCommandError({
    code: "invalid_path",
    message: "Invalid device path",
    retryable: false,
    operation: "storage",
    commandId: 42,
    path: "/ext/example.txt",
    details: { status: "7", ignored: 3 },
  });

  assert.deepEqual(normalized, {
    code: "invalid_path",
    message: "Invalid device path",
    retryable: false,
    operation: "storage",
    commandId: 42,
    path: "/ext/example.txt",
    details: { status: "7" },
  });
});

test("malformed rejections normalize to a deterministic unknown envelope", () => {
  assert.deepEqual(normalizeCommandError({ message: "missing fields" }, "scan"), {
    code: "unknown",
    message: "The operation failed with an unrecognized error response",
    retryable: false,
    operation: "scan",
  });
});

test("cancellation decisions use the stable code and never display text", () => {
  assert.equal(
    isCommandCancelled({
      code: "cancelled",
      message: "Localized cancellation message",
      retryable: false,
      operation: "transfer",
    }),
    true,
  );
  assert.equal(isCommandCancelled("Transfer cancelled"), false);
  assert.equal(
    isCommandCancelled({
      code: "timeout",
      message: "Transfer cancelled",
      retryable: true,
      operation: "transfer",
    }),
    false,
  );
});

test("feature gating explains supported, unsupported, unknown, busy, and locked states", () => {
  assert.deepEqual(resolveFeatureAvailability({ state: "supported" }), {
    state: "supported",
    available: true,
    reason: "Supported",
  });
  assert.deepEqual(
    resolveFeatureAvailability({
      state: "unsupported",
      reason: "USB is required",
    }),
    { state: "unsupported", available: false, reason: "USB is required" },
  );
  assert.deepEqual(resolveFeatureAvailability(undefined), {
    state: "unknown",
    available: false,
    reason: "Support was not reported by the device handshake",
  });
  assert.deepEqual(
    resolveFeatureAvailability(
      { state: "supported" },
      { busy: "A transfer already owns the device" },
    ),
    {
      state: "busy",
      available: false,
      reason: "A transfer already owns the device",
    },
  );
  assert.deepEqual(
    resolveFeatureAvailability(
      { state: "supported" },
      { busy: true, locked: "Terminal mode owns the connection" },
    ),
    {
      state: "locked",
      available: false,
      reason: "Terminal mode owns the connection",
    },
  );
});

test("typed admission failures map to action-level availability explanations", () => {
  for (const [code, state] of [
    ["busy", "busy"],
    ["operation_locked", "locked"],
    ["unsupported", "unsupported"],
    ["future_code", "unknown"],
  ]) {
    const availability = availabilityFromCommandError({
      code,
      message: `${state} explanation`,
      retryable: state === "busy" || state === "locked",
      operation: "connection",
    });
    assert.equal(availability.state, state);
    assert.equal(availability.available, false);
    assert.equal(availability.reason, `${state} explanation`);
  }
});
