'use strict';

const state = {
  view: 'overview',
  status: null,
  catalog: null,
  error: '',
  fleetQuery: '',
  fleetSort: 'name',
  loamQuery: '',
  lastLoad: 0,
  timer: 0,
};

const $ = (id) => document.getElementById(id);
const esc = (value) => String(value ?? '').replace(/[&<>"']/g, (c) => ({
  '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
}[c]));

function unwrap(section) {
  if (!section) return null;
  if (section.ok === false) throw new Error(section.error || 'unavailable');
  return section.value ?? section;
}

function hello() {
  const node = unwrap(state.status?.node);
  return node?.node?.hello || node?.hello || null;
}

function fleetRows() {
  try {
    const value = unwrap(state.status?.fleet);
    return Array.isArray(value) ? value : [];
  } catch {
    return [];
  }
}

function catalog() {
  return state.catalog && Array.isArray(state.catalog.entries) ? state.catalog : { entries: [], running: [] };
}

function runningSet() {
  const ids = catalog().running;
  return new Set(Array.isArray(ids) ? ids : []);
}

function ago(seconds) {
  const value = Number(seconds);
  if (!Number.isFinite(value) || value <= 0) return 'never';
  const delta = Math.max(0, Math.floor(Date.now() / 1000 - value));
  if (delta < 45) return `${delta}s ago`;
  if (delta < 3600) return `${Math.floor(delta / 60)}m ago`;
  if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`;
  return `${Math.floor(delta / 86400)}d ago`;
}

function fresh(seconds) {
  const value = Number(seconds);
  return Number.isFinite(value) && Date.now() / 1000 - value < 180;
}

function load(peer) {
  const value = Number(peer.load_average?.[0]);
  return Number.isFinite(value) ? value.toFixed(2) : '—';
}

function time(timestamp) {
  if (!timestamp) return '—';
  return new Date(timestamp).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
}

function dotClass(ok, warn) {
  if (ok) return 'ok';
  return warn ? 'warn' : 'bad';
}

function card(label, value, meta, tone = '') {
  return `<article class="card ${tone}"><div class="k">${esc(label)}</div><div class="v">${esc(value)}</div><div class="m">${esc(meta)}</div></article>`;
}

function renderIdentity() {
  const node = hello();
  const who = [node?.hostname, node?.site].filter(Boolean).join(' · ');
  $('identity').textContent = who || state.error || 'Mycelium';
  const failed = !state.status || state.status.ok === false;
  const anyError = ['node', 'fleet', 'frontdoor', 'tunnel'].some((key) => state.status?.[key]?.ok === false);
  $('live-dot').className = `dot ${failed ? 'bad' : anyError ? 'warn' : 'ok'}`;
  $('live-label').textContent = failed ? 'unreachable' : anyError ? 'partial' : 'live';
  $('generated').textContent = state.lastLoad ? `updated ${time(state.lastLoad)}` : '—';
}

function renderBanner() {
  const banner = $('banner');
  if (!state.error) {
    banner.hidden = true;
    banner.textContent = '';
    return;
  }
  banner.hidden = false;
  banner.textContent = state.error;
}

function renderOverview() {
  const node = hello();
  const peers = fleetRows();
  const entries = catalog().entries;
  const running = runningSet();
  const door = safe(() => unwrap(state.status?.frontdoor));
  const tunnel = safe(() => unwrap(state.status?.tunnel));
  const livePeers = peers.filter((peer) => fresh(peer.last_seen)).length;
  const doorHealthy = door?.frontdoor_listening === true && door?.health?.body?.ok !== false;

  $('fleet-count').textContent = peers.length || '—';
  $('fleet-nav-count').textContent = peers.length || '—';
  $('loam-count').textContent = entries.length || '—';

  $('overview-cards').innerHTML = [
    card('This machine', node?.hostname || '—', [node?.site, node?.daemon_version && `daemon ${node.daemon_version}`].filter(Boolean).join(' · ') || 'no node status', node ? '' : 'bad'),
    card('Peers', String(peers.length), `${livePeers} seen in the last 3 minutes`, livePeers ? 'ok' : peers.length ? 'warn' : ''),
    card('Spores', String(entries.length), `${running.size} running now`, entries.length ? '' : 'warn'),
    card('Frontdoor', doorHealthy ? 'listening' : 'unknown', door?.health?.body?.routes != null ? `${door.health.body.routes} routes` : 'no health body', doorHealthy ? 'ok' : door ? 'warn' : 'bad'),
    card('Tunnel', tunnel?.enabled ? 'enabled' : 'off', tunnel?.kernel_routes?.length ? `${tunnel.kernel_routes.length} kernel routes` : 'no overlay routes', tunnel?.enabled ? 'ok' : 'warn'),
  ].join('');

  const preview = peers.filter((peer) => fresh(peer.last_seen)).slice(0, 6);
  $('peer-preview').innerHTML = preview.length ? preview.map((peer) => `
    <div class="row">
      <i class="dot ${fresh(peer.last_seen) ? 'ok' : 'warn'}"></i>
      <div class="body"><div class="t">${esc(peer.hostname || 'unknown')}</div><div class="s">${esc(peer.site || peer.platform || '—')} · ${esc(peer.architecture || '')}</div></div>
      <div class="right">load ${esc(load(peer))}<br>${esc(ago(peer.last_seen))}</div>
    </div>`).join('') : `<div class="empty">No peer heartbeats.</div>`;

  const live = entries.filter((entry) => running.has(entry.id));
  $('running-list').innerHTML = live.length ? live.map((entry) => `
    <div class="row">
      <i class="dot ok"></i>
      <div class="body"><div class="t">${esc(entry.title || entry.name)}</div><div class="s">${esc(entry.id)}</div></div>
      <div class="right">${esc(entry.version || '')}<br>${esc(entry.default_surface || '')}</div>
    </div>`).join('') : `<div class="empty">Nothing is running. The launcher starts apps as separate windows.</div>`;
}

function safe(fn) {
  try { return fn(); } catch { return null; }
}

function sortedFleet() {
  const q = state.fleetQuery.toLowerCase();
  const rows = fleetRows().filter((peer) => !q || [peer.hostname, peer.site, peer.node_id, peer.platform, peer.daemon_version]
    .some((field) => String(field || '').toLowerCase().includes(q)));
  rows.sort((left, right) => {
    if (state.fleetSort === 'site') return String(left.site || '').localeCompare(String(right.site || '')) || String(left.hostname || '').localeCompare(String(right.hostname || ''));
    if (state.fleetSort === 'load') return (Number(right.load_average?.[0]) || 0) - (Number(left.load_average?.[0]) || 0);
    if (state.fleetSort === 'seen') return (Number(right.last_seen) || 0) - (Number(left.last_seen) || 0);
    return String(left.hostname || '').localeCompare(String(right.hostname || ''));
  });
  return rows;
}

function renderFleet() {
  const all = fleetRows();
  const rows = sortedFleet();
  $('fleet-meta').textContent = `${rows.length} of ${all.length}`;
  $('fleet-body').innerHTML = rows.length ? rows.map((peer) => {
    const seen = fresh(peer.last_seen);
    return `<tr>
      <td><i class="dot ${seen ? 'ok' : 'warn'}" style="margin-right:7px"></i><strong>${esc(peer.hostname || 'unknown')}</strong><div class="sub">${esc(short(peer.node_id, 12))}</div></td>
      <td>${esc(peer.site || '—')}</td>
      <td>${esc([peer.platform, peer.architecture].filter(Boolean).join(' · ') || '—')}</td>
      <td><span class="mono">${esc(peer.daemon_version || '—')}</span><div class="sub">${esc(peer.release_version || '')}</div></td>
      <td class="mono">${esc(load(peer))}</td>
      <td>${esc(ago(peer.last_seen))}${seen ? '' : ' <span class="tag off">stale</span>'}</td>
    </tr>`;
  }).join('') : `<tr><td colspan="6" class="empty">No peers match.</td></tr>`;
}

function short(value, length) {
  const text = String(value || '');
  return text.length > length ? `${text.slice(0, length)}…` : text;
}

function renderLoam() {
  const data = catalog();
  const q = state.loamQuery.toLowerCase();
  const running = runningSet();
  const rows = data.entries.filter((entry) => !q || [entry.title, entry.id, entry.role, ...(entry.surfaces || [])]
    .some((field) => String(field || '').toLowerCase().includes(q)));
  $('loam-meta').textContent = `${rows.length} of ${data.entries.length}${data.refused?.length ? ` · ${data.refused.length} refused` : ''}`;
  $('loam-body').innerHTML = rows.length ? rows.map((entry) => {
    const surfaces = Array.isArray(entry.surfaces) ? entry.surfaces : [];
    const isRunning = running.has(entry.id);
    return `<tr>
      <td><strong>${esc(entry.title || entry.name || entry.id)}</strong><div class="sub">${esc(entry.id)}</div></td>
      <td><span class="tag">${esc(entry.role || 'spore')}</span></td>
      <td>${surfaces.map((surface) => `<span class="tag">${esc(surface)}</span>`).join(' ') || '<span class="dim">—</span>'}</td>
      <td class="mono">${esc(entry.version || '—')}</td>
      <td>${isRunning ? '<span class="tag on"><i class="dot ok"></i> running</span>' : '<span class="tag off">idle</span>'}</td>
    </tr>`;
  }).join('') : `<tr><td colspan="5" class="empty">No Spores match.</td></tr>`;
}

function renderDoor() {
  const door = safe(() => unwrap(state.status?.frontdoor));
  const tunnel = safe(() => unwrap(state.status?.tunnel));
  const health = door?.health?.body || {};
  const cert = door?.certificate_trusted === true;
  const listening = door?.frontdoor_listening === true;

  $('door-summary').innerHTML = `<div class="panel-head"><h2>Local edge</h2><span class="tag ${listening ? 'on' : 'off'}">${listening ? 'listening' : 'not listening'}</span></div>
    <dl class="kv">
      <dt>Certificate</dt><dd class="${cert ? 'ok' : 'warn'}">${cert ? 'trusted' : 'not trusted'}</dd>
      <dt>Routes</dt><dd>${esc(health.routes ?? '—')}</dd>
      <dt>Derived</dt><dd>${esc(health.derived_routes ?? '—')}</dd>
      <dt>Auth</dt><dd>${health.auth_required ? 'required' : 'none'}</dd>
      <dt>Node</dt><dd>${esc(short(health.local_node, 16))}</dd>
      <dt>Binary</dt><dd>${esc(door?.frontdoor_binary || '—')}</dd>
    </dl>`;

  $('tunnel-summary').innerHTML = `<div class="panel-head"><h2>Overlay</h2><span class="tag ${tunnel?.enabled ? 'on' : 'off'}">${tunnel?.enabled ? 'enabled' : 'off'}</span></div>
    <dl class="kv">
      <dt>Endpoint</dt><dd>${esc(short(tunnel?.endpoint_id, 20))}</dd>
      <dt>Listen</dt><dd>${esc(tunnel?.listen || '—')}</dd>
      <dt>MTU</dt><dd>${esc(tunnel?.mtu || '—')}</dd>
      <dt>Routes</dt><dd>${esc(tunnel?.kernel_routes?.length ?? 0)}</dd>
      <dt>Error</dt><dd class="${tunnel?.last_error ? 'bad' : 'ok'}">${esc(tunnel?.last_error || 'none')}</dd>
    </dl>`;

  $('door-health').innerHTML = `<div class="panel-head"><h2>Health body</h2><span class="hint">the frontdoor's own answer</span></div>
    <pre class="mono" style="margin:0;white-space:pre-wrap;color:var(--muted)">${esc(JSON.stringify(health, null, 2))}</pre>`;
}

function render() {
  renderIdentity();
  renderBanner();
  renderOverview();
  renderFleet();
  renderLoam();
  renderDoor();
}

function setView(view) {
  state.view = view;
  document.querySelectorAll('.view').forEach((section) => section.classList.toggle('active', section.id === `view-${view}`));
  document.querySelectorAll('.navitem').forEach((button) => button.classList.toggle('active', button.dataset.goto === view));
  if (location.hash !== `#${view}`) history.replaceState(null, '', `#${view}`);
}

async function getJSON(path) {
  const response = await fetch(path, { headers: { accept: 'application/json' }, cache: 'no-store' });
  const value = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(value.error || `${response.status} ${response.statusText}`);
  return value;
}

async function load() {
  $('refresh').disabled = true;
  state.error = '';
  try {
    const [status, catalog] = await Promise.allSettled([
      getJSON('/api/mycelium/status'),
      getJSON('/api/spore/entries'),
    ]);
    if (status.status === 'fulfilled') {
      state.status = status.value;
    } else {
      state.status = null;
      state.error = `Mycelium status: ${status.reason.message}`;
    }
    if (catalog.status === 'fulfilled') {
      state.catalog = catalog.value;
    } else if (catalog.status === 'rejected') {
      state.error = [state.error, `Loam catalog: ${catalog.reason.message}`].filter(Boolean).join(' · ');
    }
    state.lastLoad = Date.now();
    render();
  } finally {
    $('refresh').disabled = false;
    schedule();
  }
}

function schedule() {
  clearTimeout(state.timer);
  if (!document.hidden) state.timer = setTimeout(load, 15000);
}

document.addEventListener('click', (event) => {
  const target = event.target.closest('[data-goto]');
  if (!target) return;
  event.preventDefault();
  setView(target.dataset.goto);
});
$('refresh').addEventListener('click', load);
$('fleet-search').addEventListener('input', (event) => { state.fleetQuery = event.target.value.trim(); renderFleet(); });
$('fleet-sort').addEventListener('change', (event) => { state.fleetSort = event.target.value; renderFleet(); });
$('loam-search').addEventListener('input', (event) => { state.loamQuery = event.target.value.trim(); renderLoam(); });
document.addEventListener('visibilitychange', schedule);
window.addEventListener('hashchange', () => setView(location.hash.slice(1) || 'overview'));

setView(location.hash.slice(1) || 'overview');
load();
