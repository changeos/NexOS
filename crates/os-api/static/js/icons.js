/**
 * OS Web UI — SF Symbols / macOS-style line icon set.
 *
 * All icons are self-contained inline SVG strings using
 *   viewBox="0 0 24 24", fill="none", stroke="currentColor",
 *   stroke-width="1.5", round caps/joins — so they inherit CSS color.
 *
 * Health icons carry fixed semantic colors (green/yellow/red).
 *
 * Usage in other JS files:
 *   icon('storage')            // → '<svg ...>' (default 20px)
 *   icon('storage', 24)        // → explicit size
 *   ICONS.vm                   // → raw SVG string (unsized)
 */
(function (global) {
  'use strict';

  var ICONS = {
    dashboard:
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">' +
      '<rect x="3" y="3" width="7" height="7" rx="1"/>' +
      '<rect x="14" y="3" width="7" height="7" rx="1"/>' +
      '<rect x="3" y="14" width="7" height="7" rx="1"/>' +
      '<rect x="14" y="14" width="7" height="7" rx="1"/>' +
      '</svg>',

    storage:
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">' +
      '<ellipse cx="12" cy="5" rx="8" ry="2.5"/>' +
      '<path d="M4 5v6c0 1.38 3.58 2.5 8 2.5s8-1.12 8-2.5V5"/>' +
      '<path d="M4 11v6c0 1.38 3.58 2.5 8 2.5s8-1.12 8-2.5v-6"/>' +
      '</svg>',

    vm:
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">' +
      '<rect x="2.5" y="4" width="19" height="13" rx="2"/>' +
      '<path d="M8 21h8"/>' +
      '<path d="M12 17v4"/>' +
      '<path d="M7 9l-2 2 2 2"/>' +
      '<path d="M17 9l2 2-2 2"/>' +
      '<path d="M13.5 8l-3 6"/>' +
      '</svg>',

    share:
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">' +
      '<path d="M3 7a1 1 0 0 1 1-1h5l2 2h8a1 1 0 0 1 1 1v9a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1z"/>' +
      '<circle cx="16.5" cy="11.5" r="1.8"/>' +
      '<path d="M18 11.5V8.5a2 2 0 0 0-2-2h-2"/>' +
      '</svg>',

    users:
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">' +
      '<circle cx="9" cy="8" r="3.2"/>' +
      '<path d="M3.5 20a5.5 5.5 0 0 1 11 0"/>' +
      '<path d="M16 5.2a3 3 0 0 1 0 5.6"/>' +
      '<path d="M17.5 14.2a5.5 5.5 0 0 1 3 5.8"/>' +
      '</svg>',

    nodes:
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">' +
      '<circle cx="6" cy="6" r="2.2"/>' +
      '<circle cx="18" cy="6" r="2.2"/>' +
      '<circle cx="12" cy="18" r="2.2"/>' +
      '<path d="M7.6 7.6l3.2 8.4"/>' +
      '<path d="M16.4 7.6l-3.2 8.4"/>' +
      '<path d="M8.2 6h7.6"/>' +
      '</svg>',

    settings:
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">' +
      '<circle cx="12" cy="12" r="3"/>' +
      '<path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/>' +
      '</svg>',

    'health-ok':
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#22c55e" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">' +
      '<circle cx="12" cy="12" r="9"/>' +
      '<path d="M8 12.5l2.5 2.5L16 9"/>' +
      '</svg>',

    'health-warn':
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#eab308" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">' +
      '<path d="M10.3 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.7 3.86a2 2 0 0 0-3.42 0z"/>' +
      '<path d="M12 9v4"/>' +
      '<path d="M12 17h.01"/>' +
      '</svg>',

    'health-err':
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ef4444" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">' +
      '<circle cx="12" cy="12" r="9"/>' +
      '<path d="M15 9l-6 6"/>' +
      '<path d="M9 9l6 6"/>' +
      '</svg>',

    logo:
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">' +
      '<rect x="3" y="2.5" width="18" height="19" rx="1.5"/>' +
      '<path d="M3 7h18"/>' +
      '<path d="M3 12h18"/>' +
      '<path d="M3 17h18"/>' +
      '<circle cx="6.5" cy="4.75" r="0.6" fill="currentColor" stroke="none"/>' +
      '<circle cx="6.5" cy="9.75" r="0.6" fill="currentColor" stroke="none"/>' +
      '<circle cx="6.5" cy="14.75" r="0.6" fill="currentColor" stroke="none"/>' +
      '<path d="M11 4.75h6"/>' +
      '<path d="M11 9.75h6"/>' +
      '<path d="M11 14.75h6"/>' +
      '</svg>'
  };

  /**
   * Render a named icon sized to `size`×`size` px.
   * Returns '' for unknown names so callers can safely inject the result.
   *
   * The width/height attributes are injected just before `viewBox`, so the
   * underlying SVG string stays untouched for ICONS[name] consumers.
   */
  function icon(name, size) {
    size = size == null ? 20 : size;
    var svg = ICONS[name] || '';
    if (!svg) return '';
    return svg.replace(
      'viewBox',
      'width="' + size + '" height="' + size + '" viewBox'
    );
  }

  global.ICONS = ICONS;
  global.icon = icon;
})(typeof window !== 'undefined' ? window : this);
