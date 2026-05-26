/**
 * Tiny inline-SVG sparkline used by the GPIO live-read view.
 *
 * - No external dependency: pure SVG polyline.
 * - Renders up to `samples.length` points across the full width.
 * - Drawn with `vector-effect: non-scaling-stroke` so the line keeps its
 *   pixel weight even when `preserveAspectRatio` stretches the viewBox.
 */
interface SparklineProps {
  samples: ReadonlyArray<0 | 1>;
  width?: number;
  height?: number;
}

export function Sparkline({ samples, width = 200, height = 24 }: SparklineProps) {
  const w = width;
  const h = height;

  // No data yet — just show the midpoint reference line.
  const hasSamples = samples.length > 1;
  const stepX = hasSamples ? w / (samples.length - 1) : 0;
  const points = samples
    .map((v, i) => {
      const x = i * stepX;
      // Invert Y so 1 is at the top.
      const y = v === 1 ? 2 : h - 2;
      return `${x.toFixed(2)},${y.toFixed(2)}`;
    })
    .join(" ");

  return (
    <svg
      viewBox={`0 0 ${w} ${h}`}
      preserveAspectRatio="none"
      width="100%"
      height={h}
      className="block"
      aria-hidden
    >
      {/* Faint reference line at the midpoint. */}
      <line
        x1={0}
        x2={w}
        y1={h / 2}
        y2={h / 2}
        stroke="currentColor"
        strokeWidth={1}
        className="text-elevated"
      />
      {hasSamples && (
        <polyline
          points={points}
          fill="none"
          stroke="currentColor"
          strokeWidth={1.5}
          strokeLinejoin="miter"
          strokeLinecap="butt"
          className="text-accent"
          style={{ vectorEffect: "non-scaling-stroke" }}
        />
      )}
    </svg>
  );
}
