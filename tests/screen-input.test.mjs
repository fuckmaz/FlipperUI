import assert from "node:assert/strict";
import test from "node:test";

import {
  createScreenInputController,
  INPUT_LONG,
  INPUT_PRESS,
  INPUT_RELEASE,
  INPUT_REPEAT,
  INPUT_SHORT,
  LONG_PRESS_MS,
  REPEAT_PRESS_MS,
} from "../src/components/ScreenViewer/screenInput.ts";

class FakeScheduler {
  nextId = 1;
  timeouts = new Map();
  intervals = new Map();

  setTimeout(callback, delayMs) {
    const id = this.nextId++;
    this.timeouts.set(id, { callback, delayMs });
    return id;
  }

  clearTimeout(id) {
    this.timeouts.delete(id);
  }

  setInterval(callback, delayMs) {
    const id = this.nextId++;
    this.intervals.set(id, { callback, delayMs });
    return id;
  }

  clearInterval(id) {
    this.intervals.delete(id);
  }

  fireLongTimer() {
    assert.equal(this.timeouts.size, 1);
    const [[id, timer]] = this.timeouts;
    assert.equal(timer.delayMs, LONG_PRESS_MS);
    this.timeouts.delete(id);
    timer.callback();
  }

  fireRepeatTimer() {
    assert.equal(this.intervals.size, 1);
    const [{ callback, delayMs }] = this.intervals.values();
    assert.equal(delayMs, REPEAT_PRESS_MS);
    callback();
  }
}

function harness() {
  const events = [];
  const scheduler = new FakeScheduler();
  const controller = createScreenInputController(
    (key, type) => events.push([key, type]),
    scheduler,
  );
  return { controller, events, scheduler };
}

test("a quick pointer or keyboard tap emits PRESS, SHORT, RELEASE", () => {
  const { controller, events, scheduler } = harness();
  assert.equal(controller.start("keyboard:Enter", 4), true);
  assert.equal(controller.finish("keyboard:Enter"), true);
  assert.deepEqual(events, [
    [4, INPUT_PRESS],
    [4, INPUT_SHORT],
    [4, INPUT_RELEASE],
  ]);
  assert.equal(scheduler.timeouts.size, 0);
  assert.equal(scheduler.intervals.size, 0);
});

test("a hold remains pressed through LONG and REPEAT until actual release", () => {
  const { controller, events, scheduler } = harness();
  controller.start("pointer:7", 5);
  scheduler.fireLongTimer();
  scheduler.fireRepeatTimer();
  scheduler.fireRepeatTimer();
  controller.finish("pointer:7");

  assert.deepEqual(events, [
    [5, INPUT_PRESS],
    [5, INPUT_LONG],
    [5, INPUT_REPEAT],
    [5, INPUT_REPEAT],
    [5, INPUT_RELEASE],
  ]);
  assert.equal(scheduler.intervals.size, 0);
});

test("all six controls share the same long-press lifecycle", () => {
  const { controller, events, scheduler } = harness();
  for (let key = 0; key < 6; key += 1) {
    controller.start(`key:${key}`, key);
    scheduler.fireLongTimer();
    controller.finish(`key:${key}`);
  }

  assert.deepEqual(
    events,
    Array.from({ length: 6 }, (_, key) => [
      [key, INPUT_PRESS],
      [key, INPUT_LONG],
      [key, INPUT_RELEASE],
    ]).flat(),
  );
});

test("cancellation releases held keys without inventing a short press", () => {
  const { controller, events, scheduler } = harness();
  controller.start("keyboard:Backspace", 5);
  controller.cancelAll();
  assert.deepEqual(events, [
    [5, INPUT_PRESS],
    [5, INPUT_RELEASE],
  ]);
  assert.equal(scheduler.timeouts.size, 0);
});

test("pointer and keyboard cannot own the same Flipper key simultaneously", () => {
  const { controller, events } = harness();
  assert.equal(controller.start("pointer:1", 4), true);
  assert.equal(controller.start("keyboard:Enter", 4), false);
  controller.cancel("pointer:1");
  assert.deepEqual(events, [
    [4, INPUT_PRESS],
    [4, INPUT_RELEASE],
  ]);
});
