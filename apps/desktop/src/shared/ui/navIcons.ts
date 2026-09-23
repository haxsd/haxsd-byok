import type { IconifyIcon } from "@iconify/react";

/**
 * The navigation icon set for this product.
 *
 * Drawn here rather than taken from a collection on purpose. The previous set was
 * inherited from the code base this app was forked from, so both applications showed
 * the same colourful glyphs in the same order and looked like one product in two
 * themes. These are single-colour strokes at a uniform weight, which also lets them
 * take the accent colour of the active item.
 *
 * Every icon is a 24-box stroke drawing; nothing carries a baked-in colour.
 */
const icon = (body: string, width = 24, height = 24): IconifyIcon => ({ body, width, height });

const stroke = (body: string) =>
  icon(`<g fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">${body}</g>`);

/** Overview: a pulse, because everything this app reports is traffic over time. */
export const navOverviewIcon = stroke('<path d="M2.6 13.4h3.1l2.4-6.4 3.4 11.6 2.6-7.6 1.7 2.4h5.6"/>');

/** Calls: a log, three rows with a bullet each. */
export const navCallsIcon = stroke('<path d="M9 7h11M9 12h11M9 17h7"/><circle cx="4.7" cy="7" r="1.5" fill="currentColor" stroke="none"/><circle cx="4.7" cy="12" r="1.5" fill="currentColor" stroke="none"/><circle cx="4.7" cy="17" r="1.5" fill="currentColor" stroke="none"/>');

/** Devin: a hexagonal node, the module's mark. */
export const navDevinIcon = stroke('<path d="M12 2.9l7.7 4.5v9.2L12 21.1l-7.7-4.5V7.4z"/>');

/** Models: a stack, because the library is shared by both modules. */
export const navModelsIcon = stroke('<path d="M4.6 6.7c0-1.6 3.3-2.9 7.4-2.9s7.4 1.3 7.4 2.9-3.3 2.9-7.4 2.9-7.4-1.3-7.4-2.9z"/><path d="M4.6 6.7v10.6c0 1.6 3.3 2.9 7.4 2.9s7.4-1.3 7.4-2.9V6.7"/><path d="M4.6 12c0 1.6 3.3 2.9 7.4 2.9s7.4-1.3 7.4-2.9"/>');

/** Plugins: a chip with its pins. */
export const navPluginsIcon = stroke('<rect x="6.6" y="6.6" width="10.8" height="10.8" rx="2.4"/><path d="M9.6 3.1v3.5M14.4 3.1v3.5M9.6 17.4v3.5M14.4 17.4v3.5M3.1 9.6h3.5M3.1 14.4h3.5M17.4 9.6h3.5M17.4 14.4h3.5"/>');

/** Settings: two sliders, so it reads as configuration rather than machinery. */
export const navSettingsIcon = stroke('<path d="M4.2 8h5.6M14.2 8h5.6M4.2 16h9.6M18 16h1.8"/><circle cx="12" cy="8" r="2.2"/><circle cx="16" cy="16" r="2.2"/>');

/** Tutorial: an information mark. */
export const navTutorialIcon = stroke('<circle cx="12" cy="12" r="8.8"/><path d="M12 11.2v5.1"/><circle cx="12" cy="7.9" r="1.05" fill="currentColor" stroke="none"/>');
