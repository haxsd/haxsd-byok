import { useMemo } from "react";
import styles from "./Sparkline.module.scss";

/**
 * A trend line for a figure that already has a number next to it.
 *
 * It is an SVG path rather than a chart instance: at this size a canvas costs more
 * than it shows, and the four sparks on the overview would each have needed their
 * own resize observer.
 */
export function Sparkline({ values, tone = "accent", height = 30, fill = true, ariaLabel }: {
  values: number[];
  tone?: "accent" | "ok" | "warn" | "bad";
  height?: number;
  fill?: boolean;
  ariaLabel?: string;
}) {
  const path = useMemo(() => buildPath(values), [values]);
  if (!path) return <div className={styles.root} data-tone={tone} style={{ height }} aria-hidden="true" />;
  return <div className={styles.root} data-tone={tone} style={{ height }} role="img" aria-label={ariaLabel}>
    <svg viewBox="0 0 100 100" preserveAspectRatio="none" aria-hidden="true">
      {fill && <path className={styles.area} d={`${path.line} L100,100 L0,100 Z`} />}
      <path className={styles.line} d={path.line} vectorEffect="non-scaling-stroke" />
    </svg>
  </div>;
}

/**
 * Maps the series onto a 0..100 box. The baseline is the series minimum rather than
 * zero: a spike from 99 to 100 tokens is the shape people are looking for, and a
 * zero-based axis would draw it as a flat line.
 *
 * A constant series — including an all-zero one, which is what a range with no usage
 * looks like — returns null rather than a line along the bottom. Drawing it made an
 * empty range look like a measured flat trend.
 */
function buildPath(values: number[]) {
  const points = values.filter((value) => Number.isFinite(value));
  if (points.length < 2) return null;
  const maximum = Math.max(...points);
  const minimum = Math.min(...points);
  if (maximum === minimum) return null;
  const span = maximum - minimum;
  const step = 100 / (points.length - 1);
  const line = points
    .map((value, index) => {
      const x = index * step;
      const y = 96 - ((value - minimum) / span) * 88;
      return `${index === 0 ? "M" : "L"}${x.toFixed(2)},${y.toFixed(2)}`;
    })
    .join(" ");
  return { line };
}
