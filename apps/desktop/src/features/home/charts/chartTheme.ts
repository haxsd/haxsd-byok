/**
 * Reads the chart palette from CSS custom properties.
 *
 * A canvas chart cannot inherit CSS, so the colours are declared once in the
 * theme and read back here. That indirection is what lets a theme change reach
 * the charts; before this they carried hardcoded hex values and grey axis
 * labels and kept the old look under every theme.
 */

export type ChartPalette = {
  input: string;
  cacheRead: string;
  cacheWrite: string;
  output: string;
  line: string;
  axis: string;
  grid: string;
  series: string[];
  heat: string[];
};

const names = {
  input: "--oa-chart-input",
  cacheRead: "--oa-chart-cache-read",
  cacheWrite: "--oa-chart-cache-write",
  output: "--oa-chart-output",
  line: "--oa-chart-line",
  axis: "--oa-chart-axis",
  grid: "--oa-chart-grid",
} as const;

function read(variable: string, fallback: string): string {
  if (typeof window === "undefined") return fallback;
  const value = getComputedStyle(document.documentElement).getPropertyValue(variable).trim();
  return value || fallback;
}

/** Reads the palette for the theme currently applied to the document. */
export function chartPalette(): ChartPalette {
  return {
    input: read(names.input, "#0091ff"),
    cacheRead: read(names.cacheRead, "#2ecc8f"),
    cacheWrite: read(names.cacheWrite, "#6c7cf0"),
    output: read(names.output, "#e0a92c"),
    line: read(names.line, "#7c9cff"),
    axis: read(names.axis, "#9aa4bb"),
    grid: read(names.grid, "rgba(255,255,255,0.1)"),
    series: [1, 2, 3, 4, 5].map((index) => read(`--oa-chart-${index}`, "#7c9cff")),
    heat: [0, 1, 2, 3, 4].map((index) => read(`--oa-heat-${index}`, "#2f5da8")),
  };
}
