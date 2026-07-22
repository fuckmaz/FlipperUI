import assert from "node:assert/strict";
import test from "node:test";

import { appendBoundedGpioSample } from "../src/components/Gpio/gpioHistory.ts";

test("repeated GPIO samples each advance immutable render history", () => {
  let history = [];

  for (const sample of [1, 1, 1]) {
    const previous = history;
    history = appendBoundedGpioSample(history, sample, 50);
    assert.notEqual(history, previous);
  }

  assert.deepEqual(history, [1, 1, 1]);
});

test("bounded GPIO history trims only the oldest samples", () => {
  let history = [];

  for (const sample of [0, 0, 1, 1, 0, 1]) {
    history = appendBoundedGpioSample(history, sample, 4);
  }

  assert.deepEqual(history, [1, 1, 0, 1]);
});

test("a non-positive or fractional history limit retains no samples", () => {
  assert.deepEqual(appendBoundedGpioSample([0, 1], 1, 0), []);
  assert.deepEqual(appendBoundedGpioSample([0, 1], 1, 2.5), []);
});
