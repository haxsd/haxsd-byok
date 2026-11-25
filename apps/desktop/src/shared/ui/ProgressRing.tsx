import { useEffect, useRef, useState, type ReactNode } from "react";
import styles from "./ProgressRing.module.scss";

/**
 * A single-value ring. Cache hit rate was drawn with a half-gauge in a canvas, which
 * meant its colours, stroke width and 100% state each had their own hardcoded values
 * that no theme could reach. This reads the theme through currentColor.
 */
export function ProgressRing({ value, size = 92, thickness = 8, tone = "ok", children, ariaLabel, animationKey }: {
  /** 0..1; values outside the range are clamped rather than wrapped. */
  value: number;
  size?: number;
  thickness?: number;
  tone?: "ok" | "warn" | "bad" | "accent" | "idle";
  children?: ReactNode;
  ariaLabel?: string;
  /** Change this to replay the sweep; a refresh should look like one. */
  animationKey?: number | string;
}) {
  const clamped = Math.max(0, Math.min(1, Number.isFinite(value) ? value : 0));
  const [shown, setShown] = useState(() => animationKey === undefined ? clamped : 0);
  const lastKey = useRef(animationKey);

  useEffect(() => {
    if (animationKey === undefined) {
      setShown(clamped);
      return;
    }
    if (lastKey.current === animationKey) {
      setShown(clamped);
      return;
    }
    lastKey.current = animationKey;
    setShown(0);
    const frame = window.requestAnimationFrame(() => setShown(clamped));
    return () => window.cancelAnimationFrame(frame);
  }, [animationKey, clamped]);

  const radius = (size - thickness) / 2;
  const circumference = 2 * Math.PI * radius;
  return <div className={styles.root} data-tone={tone} style={{ width: size, height: size }} role="img" aria-label={ariaLabel}>
    <svg width={size} height={size} aria-hidden="true">
      <circle className={styles.track} cx={size / 2} cy={size / 2} r={radius} strokeWidth={thickness} />
      {shown > 0 && <circle
        className={styles.value}
        cx={size / 2}
        cy={size / 2}
        r={radius}
        strokeWidth={thickness}
        strokeDasharray={`${circumference * shown} ${circumference}`}
        strokeDashoffset={0}
        transform={`rotate(-90 ${size / 2} ${size / 2})`}
      />}
    </svg>
    <div className={styles.content}>{children}</div>
  </div>;
}

/**
 * A proportion drawn as one bar. Used where a number needs a shape next to it —
 * token composition, storage use — and a second chart would be too much.
 */
export function Meter({ segments, total, height = 6, ariaLabel }: {
  segments: Array<{ value: number; color: string; label: string }>;
  /** 显式分母：单段条（如模型用量排行）传总用量或最大值，不传时按各段之和。 */
  total?: number;
  height?: number;
  ariaLabel?: string;
}) {
  const segmentSum = segments.reduce((sum, segment) => sum + Math.max(0, segment.value), 0);
  const denominator = Math.max(segmentSum, total ?? 0);
  return <div className={styles.meter} style={{ height }} role="img" aria-label={ariaLabel}>
    {denominator <= 0 || segmentSum <= 0
      ? <span className={styles.meterEmpty} />
      : segments.filter((segment) => segment.value > 0).map((segment) => <span
        key={segment.label}
        title={segment.label}
        style={{ width: `${(Math.max(0, segment.value) / denominator) * 100}%`, background: segment.color }}
      />)}
  </div>;
}
