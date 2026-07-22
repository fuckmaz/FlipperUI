export type GpioSample = 0 | 1;

/**
 * Append one GPIO sample while retaining only the newest `limit` values.
 *
 * A new array is returned for every sample, including repeated values. This is
 * important for React consumers: a run such as 1, 1, 1 still advances the
 * sparkline even though the pin's current value has not changed.
 */
export function appendBoundedGpioSample(
  samples: ReadonlyArray<GpioSample>,
  value: GpioSample,
  limit: number,
): GpioSample[] {
  if (!Number.isInteger(limit) || limit <= 0) {
    return [];
  }

  const retainedStart = Math.max(0, samples.length - limit + 1);
  return [...samples.slice(retainedStart), value];
}
