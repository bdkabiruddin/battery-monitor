// Wires up navigation, theme toggling, and live data polling for the
// Tauri build. No framework/build step -- plain DOM + fetch-style Tauri
// `invoke()` calls, mirroring network-monitor's tauri/frontend/js/app.js.

const PAGE_META = {
  dashboard: ['Dashboard', 'Overview of battery status and live telemetry'],
  telemetry: ['Live Telemetry', 'Capacity and power over time'],
  power: ['Power Usage', 'Per-process CPU share and PowerTop impact scan'],
  health: ['Health', 'Capacity, cycle count, and degradation'],
  charging: ['Charging', 'Thresholds and power profile'],
  alerts: ['Alerts', 'Low/critical battery notifications'],
  history: ['History', 'Long-term battery history and reports'],
  devices: ['Devices', 'Battery-powered peripherals'],
  settings: ['Settings', 'App preferences'],
};

let dashboardPollMs = 1000; // overridden at boot from the persisted "Refresh interval" setting
const PROCESS_POLL_MS = 2000;
const HISTORY_POLL_MS = 15000;
const TELEMETRY_POLL_MS = 3000;
const HEALTH_TREND_POLL_MS = 30000;
const CHARGING_POLL_MS = 5000;
const ALERTS_POLL_MS = 3000;
const DEVICES_POLL_MS = 5000;
const REPORTS_POLL_MS = 5000;
const SPARK_SAMPLES = 40;

let currentPage = 'dashboard';
let dashboardTimer = null;
let processTimer = null;
let historyTimer = null;
let telemetryTimer = null;
let healthTimer = null;
let chargingTimer = null;
let alertsTimer = null;
let devicesTimer = null;
let reportsTimer = null;
let selectedReportPeriod = '24h';
let capacityUnit = 'Wh';
let selectedRangeMinutes = 15;
let selectedTelemetryHours = 24;
let selectedAlertFilter = 'all';
let lastAlerts = [];

// Latest chart series, kept around so each chart's hover handler (set up
// once in init()) always reads current data without redoing the network
// fetch on every mousemove -- mirrors network-monitor's `trafficChartState`.
let h24ChartState = null;
let liveChartState = null;
let healthTrendState = null;

const sparkBuffers = { voltage: [], current: [], power: [], temp: [] };

// ── Connection health ────────────────────────────────────────────────
//
// Every invoke() goes through here, so a real backend/IPC outage (poll
// loop crashed, IPC channel broken) shows up as several rejections in a
// row -- a single one-off failure (a cancelled pkexec prompt, one bad
// tick) isn't enough to trip this, since the very next successful poll
// (1-5s later, depending on the page) resets the streak to 0 before it
// reaches the threshold. Previously every failure just went to
// console.error with zero user-visible indication anything was wrong --
// the "Live" dot kept glowing and the timestamp just silently froze.
const CONNECTION_FAILURE_THRESHOLD = 3;
let connectionFailureStreak = 0;
let connectionIsDown = false;

function setLiveStatus(online) {
  const row = $('live-status-row');
  const text = $('live-status-text');
  if (!row || !text) return;
  row.classList.toggle('offline', !online);
  text.textContent = online ? 'Live' : 'Offline';
}

function showConnectionToast() {
  const toast = $('connection-toast');
  if (toast) toast.classList.remove('hidden');
  setLiveStatus(false);
}

function hideConnectionToast() {
  const toast = $('connection-toast');
  if (toast) toast.classList.add('hidden');
  setLiveStatus(true);
}

// Escapes real system strings (device names, process names, ...) before
// they go into innerHTML — this is live data from the machine (in some
// cases reported by the hardware/peripheral itself), not trusted markup.
function escapeHtml(s) {
  const div = document.createElement('div');
  div.textContent = s == null ? '' : String(s);
  return div.innerHTML.replaceAll('"', '&quot;').replaceAll("'", '&#39;');
}

function invoke(cmd, args) {
  if (!window.__TAURI__) return Promise.reject(new Error('not running inside Tauri'));
  return window.__TAURI__.core.invoke(cmd, args).then(
    (result) => {
      connectionFailureStreak = 0;
      if (connectionIsDown) {
        connectionIsDown = false;
        hideConnectionToast();
      }
      return result;
    },
    (err) => {
      connectionFailureStreak += 1;
      if (!connectionIsDown && connectionFailureStreak >= CONNECTION_FAILURE_THRESHOLD) {
        connectionIsDown = true;
        showConnectionToast();
      }
      throw err;
    },
  );
}

function $(id) {
  return document.getElementById(id);
}

// ── Sortable tables (Power Usage processes, Devices, Report History) ──
//
// None of the tables in this app had clickable column sorting -- always
// whatever order the backend happened to return (CPU-descending for
// processes, disk-scan order for devices, newest-first for reports).
// This is a small shared helper rather than three copy-pasted sort
// implementations, since all three tables follow the same pattern:
// click a <th class="sortable">, toggle asc/desc, re-render from the
// already-fetched rows (no re-fetch needed).

// `ps -o etime` values ("MM:SS", "HH:MM:SS", or "D-HH:MM:SS") aren't
// lexicographically sortable, so Runtime needs real parsing rather than
// a plain string compare.
function parseEtimeToSeconds(s) {
  if (!s) return 0;
  const [dayPart, rest] = s.includes('-') ? s.split('-') : [null, s];
  const parts = rest.split(':').map(Number);
  let seconds = 0;
  if (parts.length === 3) seconds = parts[0] * 3600 + parts[1] * 60 + parts[2];
  else if (parts.length === 2) seconds = parts[0] * 60 + parts[1];
  else seconds = parts[0] || 0;
  if (dayPart) seconds += parseInt(dayPart, 10) * 86400;
  return seconds;
}

// column `type`: 'string' (default), 'numeric' (parses a leading number,
// e.g. "12.3%" or a raw number), or 'etime'.
function sortRows(rows, key, dir, columnTypes) {
  const type = (columnTypes && columnTypes[key]) || 'string';
  const sign = dir === 'asc' ? 1 : -1;
  const valueOf = (row) => {
    const v = row[key];
    if (v == null) return type === 'string' ? '' : -Infinity;
    if (type === 'numeric') return typeof v === 'number' ? v : parseFloat(v) || 0;
    if (type === 'etime') return parseEtimeToSeconds(v);
    return String(v).toLowerCase();
  };
  return [...rows].sort((a, b) => {
    const av = valueOf(a);
    const bv = valueOf(b);
    if (av < bv) return -1 * sign;
    if (av > bv) return 1 * sign;
    return 0;
  });
}

// Wires click handlers onto a table's `<thead>` and keeps its arrow/
// active-column indicators in sync; `onChange` re-renders from cached
// rows using the new `state.key`/`state.dir`.
function initSortableTable(theadEl, state, onChange) {
  theadEl.querySelectorAll('th.sortable').forEach((th) => {
    th.addEventListener('click', () => {
      if (state.key === th.dataset.sort) {
        state.dir = state.dir === 'asc' ? 'desc' : 'asc';
      } else {
        state.key = th.dataset.sort;
        state.dir = 'asc';
      }
      theadEl.querySelectorAll('th.sortable').forEach((h) => {
        const active = h === th;
        h.classList.toggle('sort-active', active);
        h.querySelector('.sort-arrow').textContent = active ? (state.dir === 'asc' ? '▲' : '▼') : '';
      });
      onChange();
    });
  });
}

// ── Range slider fill ────────────────────────────────────────────────

// Native <input type="range"> has no built-in "filled up to the thumb"
// look -- paints a JS-driven gradient onto the input's own background
// (the CSS hides the WebKit track pseudo-element's background so this
// shows through) rather than leaving a flat, un-progressed track.
function updateRangeFill(input) {
  const min = parseFloat(input.min || 0);
  const max = parseFloat(input.max || 100);
  const pct = max > min ? ((parseFloat(input.value) - min) / (max - min)) * 100 : 0;
  input.style.background = `linear-gradient(to right, var(--color-accent) ${pct}%, var(--color-divider) ${pct}%)`;
}

function initRangeFills() {
  document.querySelectorAll('input[type="range"]').forEach(updateRangeFill);
  document.addEventListener('input', (ev) => {
    if (ev.target.matches('input[type="range"]')) updateRangeFill(ev.target);
  });
}

// ── Theme ────────────────────────────────────────────────────────────

function applyTheme(theme) {
  document.documentElement.setAttribute('data-theme', theme);
  localStorage.setItem('battery-monitor-theme', theme);
  const isDark = theme === 'dark';
  $('theme-icon-moon').classList.toggle('hidden', !isDark);
  $('theme-icon-sun').classList.toggle('hidden', isDark);
}

function initTheme() {
  const saved = localStorage.getItem('battery-monitor-theme');
  const theme = saved || (window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light');
  applyTheme(theme);
  $('theme-toggle').addEventListener('click', () => {
    const isDark = document.documentElement.getAttribute('data-theme') === 'dark';
    applyTheme(isDark ? 'light' : 'dark');
  });
}

// ── Navigation ───────────────────────────────────────────────────────

function showPage(page) {
  currentPage = page;
  for (const btn of document.querySelectorAll('.nav-item')) {
    btn.classList.toggle('active', btn.dataset.page === page);
  }
  for (const section of document.querySelectorAll('.page')) {
    section.classList.toggle('active', section.id === `page-${page}`);
  }
  const [title, subtitle] = PAGE_META[page] || [page, ''];
  $('page-title').textContent = title;
  $('page-subtitle').textContent = subtitle;

  clearInterval(processTimer);
  if (page === 'power') {
    pollProcesses();
    processTimer = setInterval(pollProcesses, PROCESS_POLL_MS);
  }

  clearInterval(telemetryTimer);
  if (page === 'telemetry') {
    pollTelemetry();
    telemetryTimer = setInterval(pollTelemetry, TELEMETRY_POLL_MS);
  }

  clearInterval(healthTimer);
  if (page === 'health') {
    pollHealthTrend();
    healthTimer = setInterval(pollHealthTrend, HEALTH_TREND_POLL_MS);
  }

  clearInterval(chargingTimer);
  if (page === 'charging') {
    pollCharging();
    chargingTimer = setInterval(pollCharging, CHARGING_POLL_MS);
  }

  clearInterval(alertsTimer);
  if (page === 'alerts') {
    pollAlertThresholds();
    pollAlerts();
    alertsTimer = setInterval(pollAlerts, ALERTS_POLL_MS);
  }

  clearInterval(devicesTimer);
  if (page === 'devices') {
    pollDevices();
    devicesTimer = setInterval(pollDevices, DEVICES_POLL_MS);
  }

  clearInterval(reportsTimer);
  if (page === 'history') {
    pollReports();
    reportsTimer = setInterval(pollReports, REPORTS_POLL_MS);
  }

  if (page === 'settings') {
    pollSettingsPage();
  }
}

function initNav() {
  for (const btn of document.querySelectorAll('.nav-item')) {
    btn.addEventListener('click', () => showPage(btn.dataset.page));
  }
}

// ── Shared chrome: refresh, quick actions, command palette, shortcuts ──

// Re-runs whichever poll(s) back the currently-visible page, so "Refresh"
// is a real immediate re-fetch, not just a spinner animation.
const PAGE_REFRESH = {
  dashboard: () => { pollDashboard(); pollHistory(); },
  telemetry: () => pollTelemetry(),
  power: () => pollProcesses(),
  health: () => pollHealthTrend(),
  charging: () => pollCharging(),
  alerts: () => { pollAlerts(); pollAlertThresholds(); },
  history: () => pollReports(),
  devices: () => pollDevices(),
  settings: () => pollSettingsPage(),
};

function refreshCurrentPage() {
  (PAGE_REFRESH[currentPage] || (() => {}))();
}

function initRefreshButton() {
  $('refresh-btn').addEventListener('click', refreshCurrentPage);
}

// Every action here does something real when run -- no palette/menu entry
// exists for a feature that isn't actually wired up (Calibration, "Lock
// Charge at Current %", etc. from the design reference have no real
// backing yet, so they're left out rather than faked).
const ACTIONS = [
  {
    id: 'toggle-battery-saver',
    label: 'Toggle Battery Saver',
    run: async () => {
      const state = await invoke('get_battery_saver_state');
      if (state.enabled != null) await invoke('set_battery_saver', { enabled: !state.enabled });
    },
  },
  { id: 'generate-report', label: 'Generate Report (24h)', run: () => invoke('generate_report', { period: '24h' }) },
  { id: 'mark-all-read', label: 'Mark All Alerts Read', run: () => invoke('mark_all_alerts_read') },
  { id: 'refresh-now', label: 'Refresh Now', run: () => refreshCurrentPage() },
];

function runAction(id) {
  const action = ACTIONS.find((a) => a.id === id);
  if (!action) return;
  Promise.resolve(action.run()).catch((err) => console.error(`action "${id}" failed`, err));
}

// ── Quick actions dropdown ──────────────────────────────────────────

function closeQuickActions() {
  $('quick-actions-menu').classList.add('hidden');
}

function initQuickActions() {
  $('quick-actions-btn').addEventListener('click', (ev) => {
    ev.stopPropagation();
    $('quick-actions-menu').classList.toggle('hidden');
  });
  $('quick-actions-menu').addEventListener('click', (ev) => {
    const btn = ev.target.closest('[data-action]');
    if (!btn) return;
    runAction(btn.dataset.action);
    closeQuickActions();
  });
  document.addEventListener('click', (ev) => {
    if (!$('quick-actions-menu').contains(ev.target) && ev.target !== $('quick-actions-btn')) closeQuickActions();
  });
}

// ── Command palette ──────────────────────────────────────────────────

let paletteActiveIndex = 0;
let paletteMatches = [];

function paletteAllEntries() {
  const pages = Object.entries(PAGE_META).map(([page, [label]]) => ({ kind: 'Page', label, run: () => showPage(page) }));
  const actions = ACTIONS.map((a) => ({ kind: 'Action', label: a.label, run: a.run }));
  return [...pages, ...actions];
}

function openPalette() {
  $('palette-backdrop').classList.remove('hidden');
  const input = $('palette-input');
  input.value = '';
  renderPaletteResults('');
  input.focus();
}

function closePalette() {
  $('palette-backdrop').classList.add('hidden');
}

function renderPaletteResults(query) {
  const needle = query.trim().toLowerCase();
  const all = paletteAllEntries();
  paletteMatches = needle ? all.filter((e) => e.label.toLowerCase().includes(needle)) : all;
  paletteActiveIndex = 0;
  const container = $('palette-results');
  if (paletteMatches.length === 0) {
    container.innerHTML = `<div class="palette-item text-muted">No matches</div>`;
    return;
  }
  container.innerHTML = paletteMatches
    .map((e, i) => `<div class="palette-item${i === 0 ? ' active' : ''}" data-index="${i}"><span>${e.label}</span><span class="kind">${e.kind}</span></div>`)
    .join('');
  container.querySelectorAll('.palette-item').forEach((el) => {
    el.addEventListener('click', () => runPaletteMatch(parseInt(el.dataset.index, 10)));
  });
}

function runPaletteMatch(index) {
  const entry = paletteMatches[index];
  if (entry) entry.run();
  closePalette();
}

function movePaletteSelection(delta) {
  if (paletteMatches.length === 0) return;
  paletteActiveIndex = (paletteActiveIndex + delta + paletteMatches.length) % paletteMatches.length;
  $('palette-results').querySelectorAll('.palette-item').forEach((el, i) => el.classList.toggle('active', i === paletteActiveIndex));
}

function initPalette() {
  $('palette-btn').addEventListener('click', openPalette);
  $('palette-backdrop').addEventListener('click', (ev) => {
    if (ev.target === $('palette-backdrop')) closePalette();
  });
  $('palette-input').addEventListener('input', (ev) => renderPaletteResults(ev.target.value));
  $('palette-input').addEventListener('keydown', (ev) => {
    if (ev.key === 'ArrowDown') { ev.preventDefault(); movePaletteSelection(1); }
    else if (ev.key === 'ArrowUp') { ev.preventDefault(); movePaletteSelection(-1); }
    else if (ev.key === 'Enter') { ev.preventDefault(); runPaletteMatch(paletteActiveIndex); }
    else if (ev.key === 'Escape') closePalette();
  });
}

// ── Keyboard shortcuts dialog ────────────────────────────────────────

const SHORTCUTS = [
  { action: 'Open command palette', keys: '⌘K / Ctrl+K' },
  { action: 'Show shortcuts', keys: '?' },
  { action: 'Close dialog', keys: 'Esc' },
  { action: 'Refresh current page', keys: 'R' },
];

function openShortcuts() {
  $('shortcuts-list').innerHTML = SHORTCUTS.map((s) => `<div class="shortcut-row"><span>${s.action}</span><span class="shortcut-key">${s.keys}</span></div>`).join('');
  $('shortcuts-backdrop').classList.remove('hidden');
}

function closeShortcuts() {
  $('shortcuts-backdrop').classList.add('hidden');
}

function initShortcutsDialog() {
  $('shortcuts-btn').addEventListener('click', openShortcuts);
  $('shortcuts-backdrop').addEventListener('click', (ev) => {
    if (ev.target === $('shortcuts-backdrop')) closeShortcuts();
  });
}

function initGlobalKeyboardShortcuts() {
  document.addEventListener('keydown', (ev) => {
    const typing = ev.target.tagName === 'INPUT' || ev.target.tagName === 'TEXTAREA';
    if ((ev.metaKey || ev.ctrlKey) && ev.key.toLowerCase() === 'k') {
      ev.preventDefault();
      openPalette();
      return;
    }
    if (ev.key === 'Escape') {
      closePalette();
      closeShortcuts();
      closeQuickActions();
      return;
    }
    if (typing) return; // don't hijack '?'/'r' while the user is typing elsewhere
    if (ev.key === '?') {
      ev.preventDefault();
      openShortcuts();
    } else if (ev.key.toLowerCase() === 'r') {
      refreshCurrentPage();
    }
  });
}

// ── Chart axis scales ────────────────────────────────────────────────

// 5 evenly-spaced horizontal gridlines + a matching Y-axis label column
// (100%/75%/50%/25%/0%, top to bottom), aligned to the exact same
// top/bottom padding the chart's own path-building function uses (see
// buildAreaPath/buildDualLine/buildHealthTrendPath) so the gridlines
// actually line up with the curve instead of just approximating it.
function renderPercentAxis(gridId, yaxisId, height, topPad, bottomPad) {
  const grid = $(gridId);
  const yaxis = $(yaxisId);
  if (!grid && !yaxis) return;
  const usable = height - topPad - bottomPad;
  const rows = [];
  const labels = [];
  for (let i = 0; i <= 4; i++) {
    const value = 100 - (i * 100) / 4;
    const y = height - bottomPad - (value / 100) * usable;
    if (grid) rows.push(`<line x1="0" y1="${y.toFixed(1)}" x2="1000" y2="${y.toFixed(1)}" stroke="var(--color-divider)" stroke-width="1"></line>`);
    if (yaxis) labels.push(`<span>${value.toFixed(0)}%</span>`);
  }
  if (grid) grid.innerHTML = rows.join('');
  if (yaxis) yaxis.innerHTML = labels.join('');
}

// Same 5-tick scheme as renderPercentAxis, but for a dynamically-scaled
// series (e.g. watts) sharing the chart's Y-axis-right column instead of
// a fixed 0-100 range.
function renderValueAxis(yaxisId, maxValue, unit, decimals) {
  const yaxis = $(yaxisId);
  if (!yaxis) return;
  const labels = [];
  for (let i = 0; i <= 4; i++) {
    const value = (maxValue * (4 - i)) / 4;
    labels.push(`<span>${value.toFixed(decimals)}${unit}</span>`);
  }
  yaxis.innerHTML = labels.join('');
}

// Evenly-spaced 12-hour-format time labels from a series of *real*
// per-point timestamps (unix seconds) rather than an assumed fixed
// interval -- samples aren't perfectly evenly spaced (a missed poll, a
// bucket's actual width), so this reads the actual point timestamps,
// mirroring network-monitor's `updateTimestampLabelsFromRows`.
function renderTimeLabels(containerId, timestampsSeconds) {
  const container = $(containerId);
  if (!container) return;
  if (!timestampsSeconds || timestampsSeconds.length === 0) {
    container.innerHTML = '';
    return;
  }
  const labelCount = Math.min(6, timestampsSeconds.length);
  const labels = [];
  for (let i = 0; i < labelCount; i++) {
    const idx = labelCount <= 1 ? 0 : Math.round((i / (labelCount - 1)) * (timestampsSeconds.length - 1));
    labels.push(new Date(timestampsSeconds[idx] * 1000).toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit', hour12: true }));
  }
  container.innerHTML = labels.map((l) => `<span>${l}</span>`).join('');
}

// ── Chart hover tooltips ─────────────────────────────────────────────

// Attaches a mousemove/mouseleave hover crosshair + tooltip to a chart
// SVG (viewBox "0 0 1000 <height>"), matching network-monitor's
// identical 24H Traffic hover pattern. `getPointCount()` is read fresh
// on every move (not captured once) so a chart that's since re-polled
// with a different point count doesn't desync from the crosshair.
// `buildTooltipHtml(idx)` returns the tooltip's inner HTML for that
// index, or null/undefined to hide it (e.g. no data at that point).
function attachChartHover(svgId, crosshairId, tooltipId, height, getPointCount, buildTooltipHtml) {
  const svg = $(svgId);
  const crosshair = $(crosshairId);
  const tooltip = $(tooltipId);
  if (!svg || !crosshair || !tooltip) return;

  function hide() {
    tooltip.classList.add('hidden');
    crosshair.classList.add('hidden');
  }

  function update(clientX) {
    const n = getPointCount();
    if (n < 2) {
      hide();
      return;
    }
    const rect = svg.getBoundingClientRect();
    const frac = Math.min(1, Math.max(0, (clientX - rect.left) / rect.width));
    const idx = Math.round(frac * (n - 1));
    const html = buildTooltipHtml(idx);
    if (html == null) {
      hide();
      return;
    }

    const xSvg = (idx / (n - 1)) * 1000;
    crosshair.setAttribute('x1', xSvg);
    crosshair.setAttribute('x2', xSvg);
    crosshair.setAttribute('y2', height);
    crosshair.classList.remove('hidden');

    tooltip.innerHTML = html;
    tooltip.classList.remove('hidden');

    const wrapperRect = svg.parentElement.getBoundingClientRect();
    const xPixel = (xSvg / 1000) * wrapperRect.width;
    const tooltipW = tooltip.offsetWidth || 150;
    const left = xPixel + 12 + tooltipW > wrapperRect.width ? xPixel - 12 - tooltipW : xPixel + 12;
    tooltip.style.left = `${Math.max(0, left)}px`;
  }

  svg.addEventListener('mousemove', (e) => update(e.clientX));
  svg.addEventListener('mouseleave', hide);
}

function tooltipRow(label, value, color) {
  const swatch = color ? `<span class="swatch" style="background:${color}"></span>` : '';
  const style = color ? `style="color:${color}"` : '';
  return `<div class="tt-row" ${style}>${swatch}${label}: ${value}</div>`;
}

// ── Sparklines ───────────────────────────────────────────────────────

// Normalizes a series of numbers into an SVG <polyline> `points` string
// (viewBox 0 0 100 30). A flat/empty series draws a flat mid-line rather
// than dividing by a zero range.
function sparklinePoints(values) {
  if (!values || values.length === 0) return '';
  const min = Math.min(...values);
  const max = Math.max(...values);
  const range = max - min || 1;
  const stepX = values.length > 1 ? 100 / (values.length - 1) : 0;
  return values
    .map((v, i) => {
      const x = (i * stepX).toFixed(1);
      const y = (28 - ((v - min) / range) * 26).toFixed(1);
      return `${x},${y}`;
    })
    .join(' ');
}

function pushSample(buf, value) {
  if (value == null) return;
  buf.push(value);
  if (buf.length > SPARK_SAMPLES) buf.shift();
}

// ── Dashboard ────────────────────────────────────────────────────────

function setText(id, text) {
  const el = $(id);
  if (el) el.textContent = text;
}

function renderDashboard(snap) {
  setText('overview-status', snap.status);
  setText('overview-health', snap.healthPercent);
  setText('overview-note', snap.timeRemaining);
  setText('ov-model', snap.model);
  setText('ov-manufacturer', snap.manufacturer);
  setText('ov-technology', snap.technology);
  $('device-tag').textContent = snap.hasBattery ? 'Internal Battery' : 'No Battery Detected';

  const pct = snap.capacityPercent;
  const circumference = 2 * Math.PI * 40;
  if (pct != null) {
    const filled = (pct / 100) * circumference;
    $('ring-progress').setAttribute('stroke-dasharray', `${filled.toFixed(1)} ${circumference.toFixed(1)}`);
    $('ring-percent').textContent = `${Math.round(pct)}%`;
  } else {
    $('ring-progress').setAttribute('stroke-dasharray', `0 ${circumference.toFixed(1)}`);
    $('ring-percent').textContent = '—';
  }

  setText('m-voltage', snap.voltage);
  setText('m-current', snap.currentA);
  setText('m-power', snap.powerW);
  setText('m-temp', snap.temperatureC);

  const capVal = (kind) => snap[`${kind}Capacity${capacityUnit}`];

  setText('bi-health', `${snap.healthPercent} · ${snap.healthStatus}`);
  setText('bi-design-voltage', snap.designVoltage);
  setText('bi-charge-threshold', snap.chargeLimit);
  setText('bi-design-capacity', capVal('design'));
  setText('bi-full-capacity', capVal('full'));
  setText('bi-current-capacity', capVal('current'));
  setText('bi-cycle-count', snap.cycleCount);

  setText('tile-status', snap.status);
  setText('tile-health', snap.healthStatus);
  setText('tile-temp', snap.temperatureC);
  setText('tile-power', snap.powerW);

  setText('last-updated', `Updated ${snap.lastUpdated || '—'}`);
  setText('footer-status', `${pct != null ? Math.round(pct) : '—'}% · ${snap.status}`);

  // Live Telemetry's Session card shares these fields with the Dashboard
  // snapshot -- cheap to keep in sync on every 1s tick regardless of
  // which page is visible, rather than a second poll for the same data.
  setText('sess-current-draw', snap.powerW);
  setText('sess-runtime', snap.timeRemaining);
  setText('sess-health', `${snap.healthPercent} · ${snap.healthStatus}`);
  setText('sess-cycles', snap.cycleCount);

  // Health page's tiles share the same Dashboard-poll fields -- cheap to
  // keep in sync every tick rather than a second poll for the same data.
  setText('h-health-percent', snap.healthPercent);
  $('h-health-bar').style.width = pct != null ? `${Math.max(0, Math.min(100, pct))}%` : '0%';
  setText('h-health-label', snap.healthStatus);
  setText('h-cap-design', capVal('design'));
  setText('h-cap-full', capVal('full'));
  setText('h-cap-current', capVal('current'));
  setText('h-cycle-count', snap.cycleCount);

  const parseNum = (s) => {
    const n = parseFloat(s);
    return Number.isFinite(n) ? n : null;
  };
  pushSample(sparkBuffers.voltage, parseNum(snap.voltage));
  pushSample(sparkBuffers.current, parseNum(snap.currentA));
  pushSample(sparkBuffers.power, parseNum(snap.powerW));
  pushSample(sparkBuffers.temp, parseNum(snap.temperatureC));
  $('spark-voltage').setAttribute('points', sparklinePoints(sparkBuffers.voltage));
  $('spark-current').setAttribute('points', sparklinePoints(sparkBuffers.current));
  $('spark-power').setAttribute('points', sparklinePoints(sparkBuffers.power));
  $('spark-temp').setAttribute('points', sparklinePoints(sparkBuffers.temp));
}

async function pollDashboard() {
  try {
    const snap = await invoke('get_dashboard');
    renderDashboard(snap);
  } catch (e) {
    console.error('get_dashboard failed', e);
  }
}

// ── 24H Average chart ───────────────────────────────────────────────

// Builds a filled-area + line SVG path pair for a "0 0 1000 <height>"
// viewBox from a capacity series (0-100%), with 10% headroom like the
// egui build's area charts. An all-empty series (fresh install, no
// history yet) renders as a flat baseline, not a fabricated curve.
function buildAreaPath(values, height = 200) {
  if (!values || values.length === 0) return { area: '', line: '' };
  const stepX = values.length > 1 ? 1000 / (values.length - 1) : 0;
  const top = height * 0.08;
  const usable = height - top - 4;
  const points = values.map((v, i) => {
    const x = (i * stepX).toFixed(1);
    const y = (height - 4 - (v / 100) * usable).toFixed(1);
    return [x, y];
  });
  const line = 'M ' + points.map((p) => p.join(',')).join(' L ');
  const area = `${line} L ${points[points.length - 1][0]},${height} L ${points[0][0]},${height} Z`;
  return { area, line };
}

async function pollHistory() {
  try {
    const bins = await invoke('get_interval_capacity', { intervalMinutes: selectedRangeMinutes });
    const values = bins.map((b) => b.capacity);
    const { area, line } = buildAreaPath(values);
    $('h24-area').setAttribute('d', area);
    $('h24-line').setAttribute('d', line);
    const timestamps = bins.map((b) => b.timestamp);
    renderPercentAxis('h24-grid', 'h24-yaxis', 200, 200 * 0.08, 4);
    renderTimeLabels('h24-labels', timestamps);
    h24ChartState = { timestamps, capacity: values };
  } catch (e) {
    console.error('get_interval_capacity failed', e);
  }
}

function buildH24TooltipHtml(idx) {
  if (!h24ChartState || idx >= h24ChartState.timestamps.length) return null;
  const dt = new Date(h24ChartState.timestamps[idx] * 1000);
  const timeLabel = dt.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit', hour12: true });
  const dateLabel = dt.toLocaleDateString(undefined, { day: 'numeric', month: 'short' });
  return (
    `<div class="tt-row" style="justify-content:space-between;gap:12px"><span>${timeLabel}</span><span class="text-muted">${dateLabel}</span></div>` +
    tooltipRow('Capacity', `${h24ChartState.capacity[idx].toFixed(0)}%`, 'var(--color-accent-600)')
  );
}

function initRangeSeg() {
  const seg = $('range-seg');
  seg.addEventListener('click', (ev) => {
    const opt = ev.target.closest('.seg-opt');
    if (!opt) return;
    for (const o of seg.querySelectorAll('.seg-opt')) o.classList.remove('active');
    opt.classList.add('active');
    opt.querySelector('input').checked = true;
    selectedRangeMinutes = parseInt(opt.dataset.range, 10);
    pollHistory();
  });
}

// ── Live Telemetry ───────────────────────────────────────────────────

// Two independent value scales (percent for capacity, watts for power)
// mapped onto the same "0 0 1000 <height>" pixel range, since the two
// series don't share a unit. An all-empty series draws nothing rather
// than a fabricated flat line.
function buildDualLine(values, height, maxValue) {
  if (!values || values.length === 0) return '';
  const stepX = values.length > 1 ? 1000 / (values.length - 1) : 0;
  const top = height * 0.06;
  const usable = height - top - 4;
  const scale = maxValue > 0 ? maxValue : 1;
  const points = values.map((v, i) => {
    const x = (i * stepX).toFixed(1);
    const y = (height - 4 - (v / scale) * usable).toFixed(1);
    return `${x},${y}`;
  });
  return 'M ' + points.join(' L ');
}

function renderHeatmap(hourly) {
  const row = $('heatmap-row');
  row.innerHTML = '';
  const known = hourly.filter((v) => v != null);
  const max = known.length ? Math.max(...known) : 0;
  for (let hour = 0; hour < 24; hour++) {
    const v = hourly[hour];
    const div = document.createElement('div');
    div.style.flex = '1';
    div.style.height = '28px';
    div.style.background = 'var(--color-accent-600)';
    // No readings for this hour yet -- shown as a faint, clearly-inert
    // block rather than guessing an intensity.
    div.style.opacity = v == null ? 0.05 : String(0.15 + (max > 0 ? (v / max) * 0.85 : 0));
    div.title = v == null ? `${hour}:00 — no data yet` : `${hour}:00 — ${v.toFixed(1)} W avg discharge`;
    row.appendChild(div);
  }
}

async function pollTelemetry() {
  try {
    const seconds = selectedTelemetryHours * 3600;
    const [chart, avgPower, hourly] = await Promise.all([
      invoke('get_graph_data', { seconds, maxPoints: 200 }),
      invoke('get_avg_power_24h'),
      invoke('get_hourly_drain_intensity'),
    ]);
    const maxPower = Math.max(1, ...chart.power) * 1.15;
    $('live-capacity-line').setAttribute('d', buildDualLine(chart.capacity, 300, 100));
    $('live-power-line').setAttribute('d', buildDualLine(chart.power, 300, maxPower));
    setText('sess-avg-draw', avgPower);
    renderHeatmap(hourly);
    // Two independent scales sharing one plot: capacity (left axis,
    // fixed 0-100%) and power (right axis, dynamic 0-maxPower W) --
    // same padding constants as buildDualLine so gridlines/labels line
    // up with both curves, not just one.
    renderPercentAxis('live-grid', 'live-yaxis-l', 300, 300 * 0.06, 4);
    renderValueAxis('live-yaxis-r', maxPower, ' W', 1);
    renderTimeLabels('live-labels', chart.timestamps);
    liveChartState = { timestamps: chart.timestamps, capacity: chart.capacity, power: chart.power };
  } catch (e) {
    console.error('telemetry poll failed', e);
  }
}

function buildLiveTooltipHtml(idx) {
  if (!liveChartState || idx >= liveChartState.timestamps.length) return null;
  const dt = new Date(liveChartState.timestamps[idx] * 1000);
  const timeLabel = dt.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit', second: '2-digit', hour12: true });
  return (
    `<div class="tt-row">${timeLabel}</div>` +
    tooltipRow('Capacity', `${liveChartState.capacity[idx].toFixed(0)}%`, 'var(--color-accent-600)') +
    tooltipRow('Power', `${liveChartState.power[idx].toFixed(1)} W`, 'var(--color-accent-300)')
  );
}

function initTelemetryRangeSeg() {
  const seg = $('telemetry-range-seg');
  seg.addEventListener('click', (ev) => {
    const opt = ev.target.closest('.seg-opt');
    if (!opt) return;
    for (const o of seg.querySelectorAll('.seg-opt')) o.classList.remove('active');
    opt.classList.add('active');
    opt.querySelector('input').checked = true;
    selectedTelemetryHours = parseInt(opt.dataset.hours, 10);
    pollTelemetry();
  });
}

// ── Health ───────────────────────────────────────────────────────────

// Builds a possibly-multi-segment path from monthly health points, with a
// real gap (no line drawn) across any month with no snapshot yet, instead
// of interpolating across missing history.
function buildHealthTrendPath(points, height = 160) {
  const stepX = points.length > 1 ? 1000 / (points.length - 1) : 0;
  const top = height * 0.08;
  const usable = height - top - 4;
  let d = '';
  let drawing = false;
  points.forEach((p, i) => {
    if (p.healthPercent == null) {
      drawing = false;
      return;
    }
    const x = (i * stepX).toFixed(1);
    const y = (height - 4 - (p.healthPercent / 100) * usable).toFixed(1);
    d += drawing ? ` L ${x},${y}` : `${d ? ' ' : ''}M ${x},${y}`;
    drawing = true;
  });
  return d;
}

async function pollHealthTrend() {
  try {
    const points = await invoke('get_monthly_health_trend', { months: 12 });
    $('health-trend-line').setAttribute('d', buildHealthTrendPath(points));
    $('health-trend-labels').innerHTML = points.map((p) => `<span>${p.month}</span>`).join('');
    renderPercentAxis('health-trend-grid', 'health-trend-yaxis', 160, 160 * 0.08, 4);
    const known = points.filter((p) => p.healthPercent != null).length;
    $('health-trend-note').textContent =
      known < points.length
        ? `${known} of ${points.length} months have real data so far — health is snapshotted monthly starting from this version, no history predates it.`
        : '';
    healthTrendState = points;
  } catch (e) {
    console.error('get_monthly_health_trend failed', e);
  }
}

function buildHealthTrendTooltipHtml(idx) {
  if (!healthTrendState || idx >= healthTrendState.length) return null;
  const p = healthTrendState[idx];
  const value = p.healthPercent != null ? `${p.healthPercent.toFixed(0)}%` : 'no data yet';
  return `<div class="tt-row" style="justify-content:space-between;gap:12px"><span>${p.month}</span><span class="text-muted">${value}</span></div>`;
}

// ── Settings ─────────────────────────────────────────────────────────

async function saveSettingsPatch(patch) {
  // A single atomic invoke() -- the read-merge-write happens on the Rust
  // side under a lock, so two rapid patches (e.g. a toggle flip right
  // after a slider drag) can't interleave and silently discard one
  // another the way a separate get_settings()+save_settings() round trip
  // could.
  await invoke('save_settings_patch', { patch });
}

async function pollSettingsPage() {
  try {
    const [settings, autostartOn, version, historyStats] = await Promise.all([
      invoke('get_settings'),
      invoke('get_autostart'),
      invoke('get_app_version'),
      invoke('get_history_stats'),
    ]);
    for (const opt of $('settings-units-seg').querySelectorAll('.seg-opt')) {
      const active = opt.dataset.unit === settings.capacityUnit;
      opt.classList.toggle('active', active);
      opt.querySelector('input').checked = active;
    }
    $('settings-refresh').value = settings.refreshIntervalSecs;
    $('settings-refresh-val').textContent = `${settings.refreshIntervalSecs}s`;
    updateRangeFill($('settings-refresh'));
    $('settings-autostart').checked = autostartOn;
    $('settings-notif-desktop').checked = settings.notifDesktop;
    $('settings-tray').checked = settings.notifTray;
    $('settings-version').textContent = version;
    $('settings-retention').value = settings.historyRetentionDays;
    $('settings-retention-val').textContent = `${settings.historyRetentionDays} days`;
    updateRangeFill($('settings-retention'));
    $('settings-db-path').textContent = historyStats.dbPath;
    $('settings-row-count').textContent = historyStats.rowCount.toLocaleString();
  } catch (e) {
    console.error('settings poll failed', e);
  }
}

function initSettingsControls() {
  $('settings-units-seg').addEventListener('click', async (ev) => {
    const opt = ev.target.closest('.seg-opt');
    if (!opt) return;
    for (const o of $('settings-units-seg').querySelectorAll('.seg-opt')) o.classList.remove('active');
    opt.classList.add('active');
    opt.querySelector('input').checked = true;
    capacityUnit = opt.dataset.unit;
    await saveSettingsPatch({ capacityUnit });
    pollDashboard(); // re-render capacity fields in the newly selected unit immediately
  });

  const refresh = $('settings-refresh');
  refresh.addEventListener('input', () => {
    $('settings-refresh-val').textContent = `${refresh.value}s`;
  });
  refresh.addEventListener('change', async () => {
    const secs = parseInt(refresh.value, 10);
    dashboardPollMs = secs * 1000;
    startDashboardPolling();
    await saveSettingsPatch({ refreshIntervalSecs: secs });
  });

  $('settings-autostart').addEventListener('change', async (ev) => {
    try {
      await invoke('set_autostart', { enabled: ev.target.checked });
    } catch (err) {
      console.error('set_autostart failed', err);
      ev.target.checked = !ev.target.checked;
    }
  });

  $('settings-notif-desktop').addEventListener('change', async (ev) => {
    await saveSettingsPatch({ notifDesktop: ev.target.checked });
  });

  $('settings-tray').addEventListener('change', async (ev) => {
    try {
      await invoke('set_tray_enabled', { enabled: ev.target.checked });
      await saveSettingsPatch({ notifTray: ev.target.checked });
    } catch (err) {
      console.error('set_tray_enabled failed', err);
      ev.target.checked = !ev.target.checked;
    }
  });

  const retention = $('settings-retention');
  retention.addEventListener('input', () => {
    $('settings-retention-val').textContent = `${retention.value} days`;
  });
  retention.addEventListener('change', async () => {
    await saveSettingsPatch({ historyRetentionDays: parseInt(retention.value, 10) });
    // The backend just re-pruned against the new value -- refresh the
    // sample count so a lowered retention is visibly reflected right away.
    pollSettingsPage();
  });

  $('settings-clear-btn').addEventListener('click', () => {
    $('settings-clear-normal').classList.add('hidden');
    $('settings-clear-confirm').classList.remove('hidden');
  });
  $('settings-clear-cancel-btn').addEventListener('click', () => {
    $('settings-clear-confirm').classList.add('hidden');
    $('settings-clear-normal').classList.remove('hidden');
  });
  $('settings-clear-confirm-btn').addEventListener('click', async () => {
    await invoke('clear_history');
    $('settings-clear-confirm').classList.add('hidden');
    $('settings-clear-normal').classList.remove('hidden');
    pollSettingsPage();
  });
}

// ── History ──────────────────────────────────────────────────────────

function formatBytes(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

const REPORT_COLUMN_TYPES = { sizeBytes: 'numeric' };
const reportSortState = { key: 'modified', dir: 'desc' };
let lastReportRows = [];
// Tracked outside renderReportRows() rather than as a per-button
// dataset flag -- this page re-polls every REPORTS_POLL_MS (5s), which
// rebuilds the whole table and would otherwise silently wipe an
// "armed" button's state out from under the user before their confirm
// click ever lands (confirmed live: a 2s-poll build of this same
// pattern reproduced the race reliably). This module-level variable
// survives the re-render, so the row stays showing "Confirm?" across
// polls until the click or the timeout.
let armedReportPath = null;
let armedReportTimeout = null;

function disarmReportDelete() {
  armedReportPath = null;
  clearTimeout(armedReportTimeout);
  armedReportTimeout = null;
}

// Delete used to fire on a single click with no confirmation at all --
// inconsistent with Settings' Clear History, which got a confirm/cancel
// step. A table row doesn't have room for that same two-button layout
// per row, so this uses a lighter "click to arm, click again within 3s
// to actually delete" pattern instead: the button itself becomes the
// confirmation, and it silently disarms if left alone.
function renderReportRows(reports) {
  const tbody = $('report-rows');
  $('report-total').textContent = `${reports.length} report${reports.length === 1 ? '' : 's'} · ${formatBytes(reports.reduce((sum, r) => sum + r.sizeBytes, 0))}`;
  if (reports.length === 0) {
    tbody.innerHTML = `<tr><td colspan="4" class="text-muted">No reports generated yet.</td></tr>`;
    return;
  }
  // The armed report may have just been deleted (or no longer exists
  // for any other reason) -- don't leave a dangling armed-state
  // pointing at nothing.
  if (armedReportPath && !reports.some((r) => r.path === armedReportPath)) disarmReportDelete();
  const sorted = sortRows(reports, reportSortState.key, reportSortState.dir, REPORT_COLUMN_TYPES);
  tbody.innerHTML = sorted
    .map((r) => {
      const armed = r.path === armedReportPath;
      return `<tr><td>${escapeHtml(r.name)}</td><td>${escapeHtml(r.modified)}</td><td>${formatBytes(r.sizeBytes)}</td><td><button type="button" class="btn btn-ghost" data-delete-report="${escapeHtml(r.path)}" style="font-size:12px">${armed ? 'Confirm?' : 'Delete'}</button></td></tr>`;
    })
    .join('');
  tbody.querySelectorAll('[data-delete-report]').forEach((btn) => {
    btn.addEventListener('click', async () => {
      const path = btn.dataset.deleteReport;
      if (armedReportPath !== path) {
        armedReportPath = path;
        clearTimeout(armedReportTimeout);
        armedReportTimeout = setTimeout(() => {
          armedReportPath = null;
          renderReportRows(lastReportRows);
        }, 3000);
        renderReportRows(lastReportRows);
        return;
      }
      disarmReportDelete();
      await invoke('remove_report', { path });
      pollReports();
    });
  });
}

async function pollReports() {
  try {
    lastReportRows = await invoke('get_reports');
    renderReportRows(lastReportRows);
  } catch (e) {
    console.error('get_reports failed', e);
  }
}

function initReportSort() {
  initSortableTable($('report-rows').closest('table').querySelector('thead'), reportSortState, () => renderReportRows(lastReportRows));
}

function initReportDeleteAll() {
  const btn = $('report-delete-all-btn');
  btn.addEventListener('click', async () => {
    if (btn.dataset.armed !== 'true') {
      btn.dataset.armed = 'true';
      btn.textContent = 'Delete all — confirm?';
      setTimeout(() => {
        btn.dataset.armed = 'false';
        btn.textContent = 'Delete All';
      }, 3000);
      return;
    }
    btn.dataset.armed = 'false';
    btn.textContent = 'Delete All';
    const status = $('report-status');
    let failed = 0;
    for (const r of lastReportRows) {
      try {
        await invoke('remove_report', { path: r.path });
      } catch (err) {
        failed += 1;
        console.error('remove_report failed', r.path, err);
      }
    }
    if (failed > 0) status.textContent = `Failed to delete ${failed} report${failed === 1 ? '' : 's'}.`;
    pollReports();
  });
}

function initReportPeriodSeg() {
  const seg = $('report-period-seg');
  seg.addEventListener('click', (ev) => {
    const opt = ev.target.closest('.seg-opt');
    if (!opt) return;
    for (const o of seg.querySelectorAll('.seg-opt')) o.classList.remove('active');
    opt.classList.add('active');
    opt.querySelector('input').checked = true;
    selectedReportPeriod = opt.dataset.period;
  });
}

function initGenerateReport() {
  $('generate-report-btn').addEventListener('click', async () => {
    const status = $('report-status');
    status.textContent = 'Generating...';
    try {
      await invoke('generate_report', { period: selectedReportPeriod });
      status.textContent = 'Report generated.';
      pollReports();
    } catch (err) {
      status.textContent = `Failed: ${err}`;
    }
  });
}

// ── Devices ──────────────────────────────────────────────────────────

const DEVICE_COLUMN_TYPES = { capacityPercent: 'numeric' };
const deviceSortState = { key: null, dir: 'asc' };
let lastDeviceRows = [];

function renderDeviceRows(devices) {
  const tbody = $('device-rows');
  if (devices.length === 0) {
    tbody.innerHTML = `<tr><td colspan="4" class="text-muted">No battery-powered devices detected.</td></tr>`;
    return;
  }
  const sorted = deviceSortState.key ? sortRows(devices, deviceSortState.key, deviceSortState.dir, DEVICE_COLUMN_TYPES) : devices;
  tbody.innerHTML = sorted
    .map((d) => {
      const bar =
        d.capacityPercent != null
          ? `<div style="display:flex;align-items:center;gap:8px"><div style="flex:1;height:6px;background:var(--color-divider)"><div style="height:6px;background:var(--color-accent-600);width:${Math.max(0, Math.min(100, d.capacityPercent))}%"></div></div><span style="font-size:12px">${Math.round(d.capacityPercent)}%</span></div>`
          : '<span class="text-muted" style="font-size:12px">—</span>';
      return `<tr><td>${escapeHtml(d.name)}</td><td>${escapeHtml(d.manufacturer)}</td><td><span class="tag tag-neutral">${escapeHtml(d.status)}</span></td><td>${bar}</td></tr>`;
    })
    .join('');
}

async function pollDevices() {
  try {
    lastDeviceRows = await invoke('get_devices');
    renderDeviceRows(lastDeviceRows);
  } catch (e) {
    console.error('get_devices failed', e);
  }
}

function initDeviceSort() {
  initSortableTable($('device-rows').closest('table').querySelector('thead'), deviceSortState, () => renderDeviceRows(lastDeviceRows));
}

// ── Alerts ───────────────────────────────────────────────────────────

function renderAlerts() {
  const container = $('alert-rows');
  const filtered = lastAlerts.filter((a) => {
    if (selectedAlertFilter === 'all') return true;
    if (selectedAlertFilter === 'unread') return !a.read;
    return a.severity === selectedAlertFilter;
  });
  if (filtered.length === 0) {
    container.innerHTML = `<div class="text-muted" style="font-size:13px;padding:var(--space-4) 0">No alerts${selectedAlertFilter === 'all' ? ' yet.' : ' match this filter.'}</div>`;
    return;
  }
  container.innerHTML = filtered
    .map((a) => {
      const dotColor = a.severity === 'Critical' ? 'var(--color-accent-800)' : 'var(--color-accent-600)';
      const tagClass = a.severity === 'Critical' ? 'tag-accent' : 'tag-outline';
      const readBtn = a.read
        ? ''
        : `<button type="button" class="btn btn-ghost" data-mark-read="${a.id}" style="font-size:11px;padding:2px 8px">Mark read</button>`;
      return `<div style="display:flex;align-items:center;gap:10px;padding:9px 0;border-bottom:1px solid var(--color-divider);opacity:${a.read ? 0.6 : 1}">
        <span style="width:7px;height:7px;border-radius:50%;background:${dotColor};flex:none"></span>
        <span class="tag ${tagClass}" style="flex:none">${a.severity}</span>
        <span style="flex:1;min-width:0">${a.message}</span>
        <span class="text-muted" style="font-size:12px;flex:none">${a.time}</span>
        ${readBtn}
      </div>`;
    })
    .join('');
  container.querySelectorAll('[data-mark-read]').forEach((btn) => {
    btn.addEventListener('click', async () => {
      await invoke('mark_alert_read', { id: parseInt(btn.dataset.markRead, 10) });
      pollAlerts();
    });
  });
}

async function pollAlerts() {
  try {
    lastAlerts = await invoke('get_alerts');
    renderAlerts();
  } catch (e) {
    console.error('get_alerts failed', e);
  }
}

function initAlertFilterSeg() {
  const seg = $('alert-filter-seg');
  seg.addEventListener('click', (ev) => {
    const opt = ev.target.closest('.seg-opt');
    if (!opt) return;
    for (const o of seg.querySelectorAll('.seg-opt')) o.classList.remove('active');
    opt.classList.add('active');
    opt.querySelector('input').checked = true;
    selectedAlertFilter = opt.dataset.filter;
    renderAlerts();
  });
}

function initMarkAllRead() {
  $('mark-all-read').addEventListener('click', async () => {
    await invoke('mark_all_alerts_read');
    pollAlerts();
  });
}

async function pollAlertThresholds() {
  try {
    const t = await invoke('get_alert_thresholds');
    $('al-low').value = t.low;
    $('al-critical').value = t.critical;
    $('al-low-val').textContent = `${t.low}%`;
    $('al-critical-val').textContent = `${t.critical}%`;
    // Setting .value via JS doesn't fire 'input', so the gradient fill
    // needs an explicit refresh here too, not just on user drag.
    updateRangeFill($('al-low'));
    updateRangeFill($('al-critical'));
  } catch (e) {
    console.error('get_alert_thresholds failed', e);
  }
}

function initAlertThresholdSliders() {
  const low = $('al-low');
  const critical = $('al-critical');
  const note = $('al-threshold-note');
  const apply = async () => {
    const l = parseInt(low.value, 10);
    const c = parseInt(critical.value, 10);
    $('al-low-val').textContent = `${l}%`;
    $('al-critical-val').textContent = `${c}%`;
    if (c >= l) return; // let the user keep dragging without fighting them mid-drag
    try {
      await invoke('set_alert_thresholds', { low: l, critical: c });
    } catch (err) {
      note.textContent = `Failed to apply: ${err}`;
    }
  };
  low.addEventListener('input', apply);
  critical.addEventListener('input', apply);
}

// ── Charging ─────────────────────────────────────────────────────────

function renderChargeThreshold(t) {
  const body = $('charge-threshold-body');
  // Don't clobber an in-progress drag on the periodic re-poll.
  if (body.contains(document.activeElement)) return;
  if (!t.supported) {
    body.innerHTML = `<div class="text-muted" style="font-size:12px">Not supported on this hardware — the kernel driver for this battery doesn't expose charge_control_start_threshold/charge_control_end_threshold (common on ThinkPads and similar; not present on most desktops).</div>`;
    return;
  }
  const start = t.start != null ? Math.round(t.start) : 40;
  const end = t.end != null ? Math.round(t.end) : 80;
  body.innerHTML = `
    <div style="display:flex;flex-direction:column;gap:10px;font-size:13px">
      <div>
        <div style="display:flex;justify-content:space-between"><span class="text-muted">Start</span><span id="ct-start-val">${start}%</span></div>
        <input type="range" min="0" max="100" value="${start}" id="ct-start" style="width:100%">
      </div>
      <div>
        <div style="display:flex;justify-content:space-between"><span class="text-muted">Stop</span><span id="ct-end-val">${end}%</span></div>
        <input type="range" min="0" max="100" value="${end}" id="ct-end" style="width:100%">
      </div>
      <div style="display:flex;align-items:center;gap:10px">
        <button type="button" class="btn btn-primary blueprint" id="ct-apply"><i class="corner tl"></i><i class="corner tr"></i><i class="corner bl"></i><i class="corner br"></i>Apply (requires password)</button>
        <span class="text-muted" style="font-size:12px" id="ct-status"></span>
      </div>
    </div>`;
  const startInput = $('ct-start');
  const endInput = $('ct-end');
  updateRangeFill(startInput);
  updateRangeFill(endInput);
  startInput.addEventListener('input', () => { $('ct-start-val').textContent = `${startInput.value}%`; });
  endInput.addEventListener('input', () => { $('ct-end-val').textContent = `${endInput.value}%`; });
  $('ct-apply').addEventListener('click', async () => {
    const status = $('ct-status');
    const s = parseInt(startInput.value, 10);
    const e = parseInt(endInput.value, 10);
    if (s >= e) {
      status.textContent = 'Start must be less than stop.';
      return;
    }
    status.textContent = 'Applying...';
    try {
      await invoke('set_charge_thresholds', { start: s, end: e });
      status.textContent = 'Applied.';
      pollCharging();
    } catch (err) {
      status.textContent = `Failed: ${err}`;
    }
  });
}

function renderPowerProfileSeg(state) {
  const seg = $('power-profile-seg');
  seg.innerHTML = (state.profiles || [])
    .map((p) => `<label class="seg-opt${p === state.active ? ' active' : ''}" data-profile="${p}"><input type="radio" name="powerProfile"${p === state.active ? ' checked' : ''}>${p}</label>`)
    .join('');
  $('power-profile-note').textContent =
    state.profiles.length === 0
      ? 'power-profiles-daemon not available on this system.'
      : `Current runtime estimate: ${$('sess-runtime') ? $('sess-runtime').textContent : '—'} (measured, not predicted per-profile).`;
  seg.querySelectorAll('.seg-opt').forEach((opt) => {
    opt.addEventListener('click', async () => {
      const profile = opt.dataset.profile;
      try {
        await invoke('set_power_profile', { name: profile });
        pollCharging();
      } catch (err) {
        console.error('set_power_profile failed', err);
      }
    });
  });
}

let batterySaverWired = false;

function renderBatterySaver(state) {
  const toggle = $('battery-saver-toggle');
  toggle.checked = !!state.enabled;
  toggle.disabled = state.enabled == null;
  const low = state.lowPercent != null ? `${state.lowPercent}%` : '—';
  const critical = state.criticalPercent != null ? `${state.criticalPercent}%` : '—';
  $('battery-saver-note').textContent =
    state.enabled == null
      ? 'GNOME Settings Daemon not available — this setting only exists on GNOME desktops.'
      : `Switches to the power-saver profile automatically at low battery. UPower is configured with Low: ${low}, Critical: ${critical} (read-only here).`;
  if (!batterySaverWired) {
    batterySaverWired = true;
    toggle.addEventListener('change', async () => {
      try {
        await invoke('set_battery_saver', { enabled: toggle.checked });
      } catch (err) {
        console.error('set_battery_saver failed', err);
      }
    });
  }
}

function renderUsbPd(info) {
  const body = $('usb-pd-body');
  if (!info) {
    body.innerHTML = `<div class="text-muted" style="font-size:12px">No active USB-C Power Delivery source detected.</div>`;
    return;
  }
  const row = (label, value) => `<div class="info-row"><span class="label">${label}</span><span class="value">${value}</span></div>`;
  body.innerHTML = [
    row('Type', info.usbType),
    row('Voltage', info.voltageV != null ? `${info.voltageV.toFixed(2)} V` : '—'),
    row('Current', info.currentA != null ? `${info.currentA.toFixed(2)} A` : '—'),
    row('Max Current', info.maxCurrentA != null ? `${info.maxCurrentA.toFixed(2)} A` : '—'),
    row('Power', info.watts != null ? `${info.watts.toFixed(1)} W` : '—'),
  ].join('');
}

async function pollCharging() {
  try {
    const [thresholds, profileState, saverState, usbPd, estimatedDraw] = await Promise.all([
      invoke('get_charge_thresholds'),
      invoke('get_power_profile_state'),
      invoke('get_battery_saver_state'),
      invoke('get_usb_pd'),
      invoke('get_estimated_system_draw'),
    ]);
    renderChargeThreshold(thresholds);
    renderPowerProfileSeg(profileState);
    renderBatterySaver(saverState);
    renderUsbPd(usbPd);
    $('estimated-draw-tile').textContent = estimatedDraw;
  } catch (e) {
    console.error('charging poll failed', e);
  }
}

function initUsbPdRefresh() {
  $('usb-pd-refresh').addEventListener('click', async () => {
    renderUsbPd(await invoke('get_usb_pd'));
  });
}

// ── Power Usage ──────────────────────────────────────────────────────

const PROCESS_COLUMN_TYPES = { pid: 'numeric', cpuPercent: 'numeric', memPercent: 'numeric', runtime: 'etime' };
const processSortState = { key: 'cpuPercent', dir: 'desc' };

function renderProcessRows(rows, filterText) {
  const tbody = $('process-rows');
  const needle = (filterText || '').toLowerCase();
  const sorted = sortRows(rows, processSortState.key, processSortState.dir, PROCESS_COLUMN_TYPES);
  const filtered = needle ? sorted.filter((p) => p.name.toLowerCase().includes(needle) || p.pid.includes(needle)) : sorted;
  if (filtered.length === 0) {
    tbody.innerHTML = `<tr><td colspan="5" class="text-muted">No processes match "${escapeHtml(filterText)}".</td></tr>`;
    return;
  }
  tbody.innerHTML = '';
  for (const p of filtered) {
    const tr = document.createElement('tr');
    tr.innerHTML = `<td>${escapeHtml(p.pid)}</td><td>${escapeHtml(p.name)}</td><td>${escapeHtml(p.cpuPercent)}</td><td>${escapeHtml(p.memPercent)}</td><td>${escapeHtml(p.runtime)}</td>`;
    tbody.appendChild(tr);
  }
}

let lastProcessRows = [];

async function pollProcesses() {
  try {
    lastProcessRows = await invoke('get_top_processes', { limit: 20 });
    renderProcessRows(lastProcessRows, $('process-search').value);
  } catch (e) {
    console.error('get_top_processes failed', e);
  }
}

function initProcessSearch() {
  $('process-search').addEventListener('input', (ev) => {
    renderProcessRows(lastProcessRows, ev.target.value);
  });
  initSortableTable($('process-rows').closest('table').querySelector('thead'), processSortState, () =>
    renderProcessRows(lastProcessRows, $('process-search').value),
  );
}

function initPowertopScan() {
  const btn = $('powertop-scan-btn');
  const status = $('powertop-status');
  const table = $('powertop-table');
  const rowsEl = $('powertop-rows');
  btn.addEventListener('click', async () => {
    btn.disabled = true;
    status.textContent = 'Scanning... (15s, will prompt for your password)';
    table.style.display = 'none';
    try {
      const rows = await invoke('run_powertop_scan', { seconds: 15, limit: 20 });
      if (rows.length === 0) {
        status.textContent = 'Scan completed but no process rows were parsed from the report.';
      } else {
        status.textContent = `Scan completed — ${rows.length} process(es) found.`;
        rowsEl.innerHTML = rows
          .map((r) => `<tr><td>${r.pid != null ? escapeHtml(r.pid) : '—'}</td><td>${escapeHtml(r.name)}</td><td>${escapeHtml(r.impact)}</td><td>${escapeHtml(r.category)}</td></tr>`)
          .join('');
        table.style.display = '';
      }
    } catch (e) {
      status.textContent = `Scan failed: ${e}`;
    } finally {
      btn.disabled = false;
    }
  });
}

async function initRaplScan() {
  const card = $('rapl-card');
  const available = await invoke('get_rapl_available').catch(() => false);
  if (!available) {
    card.style.display = 'none';
    return;
  }
  const btn = $('rapl-scan-btn');
  const status = $('rapl-status');
  const tile = $('rapl-tile');
  btn.addEventListener('click', async () => {
    btn.disabled = true;
    status.textContent = 'Measuring... (1s, will prompt for your password)';
    try {
      const watts = await invoke('measure_package_power');
      tile.textContent = `${watts.toFixed(1)} W`;
      status.textContent = 'CPU package power only — not whole-system power.';
    } catch (e) {
      status.textContent = `Measurement failed: ${e}`;
    } finally {
      btn.disabled = false;
    }
  });
}

// ── Boot ─────────────────────────────────────────────────────────────

async function init() {
  initTheme();
  initNav();
  initRangeSeg();
  initTelemetryRangeSeg();
  initProcessSearch();
  initPowertopScan();
  initRaplScan();
  initUsbPdRefresh();
  initAlertFilterSeg();
  initMarkAllRead();
  initAlertThresholdSliders();
  initReportPeriodSeg();
  initGenerateReport();
  initDeviceSort();
  initReportSort();
  initReportDeleteAll();
  initSettingsControls();
  initRefreshButton();
  initQuickActions();
  initPalette();
  initShortcutsDialog();
  initGlobalKeyboardShortcuts();
  initRangeFills();

  attachChartHover('h24-svg', 'h24-crosshair', 'h24-tooltip', 200, () => (h24ChartState ? h24ChartState.timestamps.length : 0), buildH24TooltipHtml);
  attachChartHover('live-svg', 'live-crosshair', 'live-tooltip', 300, () => (liveChartState ? liveChartState.timestamps.length : 0), buildLiveTooltipHtml);
  attachChartHover('health-trend-svg', 'health-trend-crosshair', 'health-trend-tooltip', 160, () => (healthTrendState ? healthTrendState.length : 0), buildHealthTrendTooltipHtml);

  // Load the persisted "Refresh interval"/"Capacity units" preferences
  // before the first poll, so the very first render already uses them.
  try {
    const settings = await invoke('get_settings');
    capacityUnit = settings.capacityUnit;
    dashboardPollMs = Math.max(1, settings.refreshIntervalSecs) * 1000;
  } catch (e) {
    console.error('get_settings failed', e);
  }

  pollDashboard();
  startDashboardPolling();

  pollHistory();
  historyTimer = setInterval(pollHistory, HISTORY_POLL_MS);
}

function startDashboardPolling() {
  clearInterval(dashboardTimer);
  dashboardTimer = setInterval(pollDashboard, dashboardPollMs);
}

document.addEventListener('DOMContentLoaded', () => { init(); });
