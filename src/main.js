// ============================================================
//  Nibble — frontend (vanilla, uses window.__TAURI__)
// ============================================================
const { invoke, convertFileSrc } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { open: openDialog } = window.__TAURI__.dialog;
const { getCurrentWindow } = window.__TAURI__.window;
const notif = window.__TAURI__.notification || {};

// ============================================================
//  Window controls (custom titlebar)
// ============================================================
const win = getCurrentWindow();
document.getElementById('btn-close').onclick = () => win.close();
document.getElementById('btn-min').onclick   = () => win.minimize();
document.getElementById('btn-max').onclick   = () => win.toggleMaximize();

// ============================================================
//  Toast notifications — top-right popup that survives view switches.
//  Use for terminal feedback ("✓ Downloaded", "✗ Failed") so the user
//  knows a background task finished even if they navigated away.
// ============================================================
function showToast(title, msg, type = 'ok', timeoutMs = 5000) {
  const stack = document.getElementById('toast-stack');
  if (!stack) return;
  const t = document.createElement('div');
  t.className = `toast ${type}`;
  const safeTitle = String(title ?? '');
  const safeMsg   = String(msg ?? '');
  t.innerHTML = `
    <div class="toast-icon">${type === 'ok' ? '✓' : '✗'}</div>
    <div class="toast-body">
      <div class="toast-title"></div>
      <div class="toast-msg"></div>
    </div>
    <button class="toast-close" aria-label="dismiss">×</button>
  `;
  t.querySelector('.toast-title').textContent = safeTitle;
  t.querySelector('.toast-msg').textContent   = safeMsg;
  stack.appendChild(t);
  // Cap the stack — if toasts pile up faster than they auto-dismiss (e.g. a
  // batch of failures), drop the oldest so they never march off-screen.
  while (stack.children.length > 4) {
    stack.firstElementChild.remove();
  }
  // animate in next frame so transition runs (browser needs initial layout)
  requestAnimationFrame(() => t.classList.add('show'));
  const dismiss = () => {
    t.classList.remove('show');
    t.classList.add('hide');
    setTimeout(() => t.remove(), 280);
  };
  t.querySelector('.toast-close').onclick = (e) => { e.stopPropagation(); dismiss(); };
  t.onclick = dismiss;
  if (timeoutMs > 0) setTimeout(dismiss, timeoutMs);
  return t;
}

// ============================================================
//  Settings (persisted in localStorage). Loaded first: the UI scale and the
//  appearance classes below depend on them.
// ============================================================
// Storage keys from early development builds → current names (one-time).
try {
  for (const [from, to] of [
    ['codeqs.settings.v1', 'nibble.settings.v1'],
    ['av1conv.download.saveDir', 'nibble.download.saveDir'],
    ['av1conv.download.cookiesBrowser', 'nibble.download.cookiesBrowser'],
  ]) {
    const v = localStorage.getItem(from);
    if (v !== null && localStorage.getItem(to) === null) localStorage.setItem(to, v);
    localStorage.removeItem(from);
  }
} catch (e) { /* storage unavailable */ }

const SETTINGS_KEY = 'nibble.settings.v1';
const DEFAULT_SETTINGS = {
  convertDir: '',
  usecase: 'auto',          // which preset is ★ recommended
  notif: true,
  confirmClearHistory: true,
  histDays: 0,              // Recent Activity: drop entries older than N days (0 = keep)
  deleteMode: 'trash',      // Recent Activity ×: 'trash' (Recycle Bin) | 'permanent'
  uiScale: 1,               // user factor on top of the automatic scale
  reduceMotion: false,
  decor: true,
  rememberWindow: false,
  rememberPreset: true,
  lastPreset: null,
  overwrite: false,         // false = keep both ("name (2).mp4")
  openFolderAfter: false,
  dlQuality: 'best',
  ytdlpAutoCheck: true,
  shareTtlMin: 10,
  shotHotkey: 'ctrl+shift+a',   // '' = off
  shotAction: 'copy',           // copy | save | both
  shotDir: '',                  // '' = Pictures\Nibble
  shotPopup: true,              // preview popup at the top of the screen
  shotSound: true,
  shotTools: true,              // drawing toolbar in the capture overlay (it reads this key itself)
  shotFormat: 'png',            // png | jpg | webp (saved screenshots)
  recHotkey: 'ctrl+shift+r',    // opens the capture screen in Video mode; '' = off
};
let settings = { ...DEFAULT_SETTINGS };

function loadSettings() {
  try {
    const raw = localStorage.getItem(SETTINGS_KEY);
    if (raw) {
      const parsed = JSON.parse(raw);
      settings = { ...DEFAULT_SETTINGS, ...parsed };
    }
  } catch (e) {
    console.debug('settings load failed:', e);
    settings = { ...DEFAULT_SETTINGS };
  }
  // Values from older versions
  if (settings.usecase === 'archive') settings.usecase = 'auto';
  // The default screenshot shortcut moved from Ctrl+Shift+S to Ctrl+Shift+A
  // (once; a shortcut the user picked themselves is kept)
  if (!settings.hkDefaultV2) {
    if (settings.shotHotkey === 'ctrl+shift+s') settings.shotHotkey = 'ctrl+shift+a';
    settings.hkDefaultV2 = true;
    try { localStorage.setItem(SETTINGS_KEY, JSON.stringify(settings)); } catch (e) { /* noop */ }
  }
}
function saveSettings() {
  try {
    localStorage.setItem(SETTINGS_KEY, JSON.stringify(settings));
  } catch (e) {
    console.debug('settings save failed:', e);
  }
}
loadSettings();

function applyAppearance() {
  const root = document.documentElement;
  root.classList.toggle('reduce-motion', !!settings.reduceMotion);
  root.classList.toggle('no-decor', settings.decor === false);
}
applyAppearance();

// ============================================================
//  UI scale. The layout is designed for a 1007×692 CSS-px window. The base
//  scale is what a 1510×1038 px window gets on a 4K screen (proportionally
//  elsewhere) — the reference size. The default window itself is bigger
//  (fit_window_to_monitor in lib.rs) so the home screen fits without
//  scrolling; that only adds room. Resizing the window does NOT grow the UI;
//  it only shrinks when the window gets too small for the layout.
//  Settings → Interface size multiplies on top (also Ctrl +/−/0, Ctrl+wheel).
// ============================================================
const DESIGN_W = 1007;
const DESIGN_H = 692;
const FIT_W = 800;           // smallest layout (design px) before shrinking
const FIT_H = 540;
const UI_SCALE_STEPS = [0.8, 0.9, 1, 1.1, 1.25, 1.5];
let uiScale = 1;
function defaultWindowCss() {
  // The reference window (1510×1038 on 4K) in CSS px (screen.* already is) —
  // the old default size, kept as the scale reference.
  const sw = window.screen.width, sh = window.screen.height;
  // No usable screen info (some embedded/emulated contexts) → treat the
  // current window as the default one.
  if (!(sw >= 640 && sh >= 480)) return [window.innerWidth, window.innerHeight];
  const w =Math.min(Math.max(sw * 1510 / 3840, 900), sw * 0.95);
  const h = Math.min(Math.max(sh * 1038 / 2160, 600), sh * 0.92);
  return [w, h];
}
function applyUiScale() {
  const [w, h] = defaultWindowCss();
  const base = Math.min(w / DESIGN_W, h / DESIGN_H);
  const want = base * (Number(settings.uiScale) || 1);
  const fit = Math.min(window.innerWidth / FIT_W, window.innerHeight / FIT_H);
  uiScale = Math.max(0.6, Math.min(3, want, fit));
  document.documentElement.style.zoom = uiScale.toFixed(4);
}
function setUiScaleFactor(f) {
  settings.uiScale = f;
  saveSettings();
  applyUiScale();
  syncUiScaleSeg();
}
function stepUiScale(dir) {
  const cur = Number(settings.uiScale) || 1;
  let i = UI_SCALE_STEPS.findIndex((s) => Math.abs(s - cur) < 0.001);
  if (i < 0) i = UI_SCALE_STEPS.indexOf(1);
  i = Math.max(0, Math.min(UI_SCALE_STEPS.length - 1, i + dir));
  setUiScaleFactor(UI_SCALE_STEPS[i]);
  // One live toast instead of a pile while stepping repeatedly
  document.querySelectorAll('.toast[data-kind="scale"]').forEach((t) => t.remove());
  const t = showToast('Interface size', `${Math.round(UI_SCALE_STEPS[i] * 100)} %`, 'ok', 1400);
  if (t) t.dataset.kind = 'scale';
}
function syncUiScaleSeg() {
  const cur = Number(settings.uiScale) || 1;
  document.querySelectorAll('#set-uiscale button').forEach((b) => {
    const on = Math.abs(Number(b.dataset.v) - cur) < 0.001;
    b.classList.toggle('active', on);
    b.setAttribute('aria-checked', String(on));
  });
}
applyUiScale();
// The window is sized to the monitor right after the page starts loading.
window.addEventListener('load', applyUiScale);
let lastWheelStep = 0;
window.addEventListener('wheel', (e) => {
  if (!e.ctrlKey) return;
  e.preventDefault();
  // Touchpads fire dozens of wheel events per gesture — one step per 250 ms.
  if (performance.now() - lastWheelStep < 250 || !e.deltaY) return;
  lastWheelStep = performance.now();
  stepUiScale(e.deltaY < 0 ? 1 : -1);
}, { passive: false });
{
  let pending = false;
  window.addEventListener('resize', () => {
    if (pending) return;
    pending = true;
    requestAnimationFrame(() => { pending = false; applyUiScale(); });
  });
}

// ============================================================
//  App version — read from the binary so the UI never shows a stale number.
// ============================================================
window.__TAURI__.app?.getVersion?.()
  .then((v) => {
    document.querySelectorAll('.app-version').forEach((el) => { el.textContent = `v${v}`; });
  })
  .catch(() => { /* keep the baked-in fallback */ });

// ============================================================
//  Utilities
// ============================================================
function escapeHtml(s) {
  return String(s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

// ============================================================
//  State
// ============================================================
let presets         = [];
let selectedPreset  = null;
let stagedFiles     = [];   // array of {path, name}
let convertingMap   = {};   // file_index -> row element
let hardwareInfo    = null; // populated at init; drives preset filtering + welcome display

// ============================================================
//  View management
// ============================================================
const views = {
  welcome:    document.getElementById('view-welcome'),
  picker:     document.getElementById('view-picker'),
  download:   document.getElementById('view-download'),
  settings:   document.getElementById('view-settings'),
  captures:   document.getElementById('view-captures'),
  converting: document.getElementById('view-converting'),
};
function showView(name) {
  for (const k of Object.keys(views)) {
    views[k].classList.toggle('active', k === name);
  }
  // measured after the entrance animation (its transforms shift the rects)
  if (name === 'welcome') { requestAnimationFrame(alignActivityBottom); setTimeout(alignActivityBottom, 800); }
}

// ============================================================
//  History rendering
// ============================================================
function ago(iso) {
  try {
    const t = new Date(iso);
    const diff = (Date.now() - t.getTime()) / 1000;
    if (diff < 60)        return `${Math.round(diff)}s ago`;
    if (diff < 3600)      return `${Math.round(diff/60)}m ago`;
    if (diff < 86400)     return `${Math.round(diff/3600)}h ago`;
    if (diff < 7*86400)   return `${Math.round(diff/86400)}d ago`;
    return `${Math.round(diff/(7*86400))}w ago`;
  } catch (e) { return '?'; }
}


// ---- Hover preview for Recent Activity files (images + playable videos) ----
const PEEK_IMG = /\.(png|jpe?g|webp|gif|bmp)$/i;
const PEEK_VID = /\.(mp4|m4v|webm|mov|mkv)$/i;
let peekEl = null, peekTimer = 0, peekFor = null;

function histPeekHide() {
  clearTimeout(peekTimer);
  peekFor = null;
  if (!peekEl) return;
  peekEl.classList.remove('show');
  const v = peekEl.querySelector('video');
  if (v) { v.pause(); v.removeAttribute('src'); v.load(); }
}

function histPeekShow(anchor, path, entry) {
  const isImg = PEEK_IMG.test(path), isVid = PEEK_VID.test(path);
  if (!isImg && !isVid) return;
  clearTimeout(peekTimer);
  peekFor = anchor;
  peekTimer = setTimeout(() => {
    if (peekFor !== anchor) return;
    if (!peekEl) {
      peekEl = document.createElement('div');
      peekEl.className = 'hist-peek';
      document.body.appendChild(peekEl);
    }
    peekEl.classList.remove('show');
    peekEl.innerHTML = '';
    const media = document.createElement(isImg ? 'img' : 'video');
    // Caption: file name, kind · dimensions · size, folder
    const cut = Math.max(path.lastIndexOf('\\'), path.lastIndexOf('/'));
    const cap = document.createElement('div');
    cap.className = 'peek-cap';
    const nm = document.createElement('div');
    nm.className = 'peek-name';
    nm.textContent = path.slice(cut + 1);
    const meta = document.createElement('div');
    meta.className = 'peek-meta';
    const dir = document.createElement('div');
    dir.className = 'peek-dir';
    // rtl box → a long path is cut at its start; LRM marks keep it in order
    dir.textContent = '‎' + path.slice(0, cut) + '‎';
    cap.append(nm, meta, dir);
    const fillMeta = (w, h) => {
      const bits = [entry?.preset, w && h ? `${w} × ${h}` : '',
        entry?.size_mb > 0.05 ? `${entry.size_mb.toFixed(1)} MB` : ''].filter(Boolean);
      meta.textContent = bits.join('  ·  ');
    };
    fillMeta();
    const place = () => {
      if (peekFor !== anchor) return;
      // html carries the UI zoom: rects are in screen px, styles get zoomed
      const z = parseFloat(document.documentElement.style.zoom) || 1;
      const r = anchor.getBoundingClientRect();
      const pw = peekEl.offsetWidth * z, ph = peekEl.offsetHeight * z;
      let left = Math.min(Math.max(8, r.left), window.innerWidth - pw - 8);
      let top = r.bottom + 8;
      if (top + ph > window.innerHeight - 8) top = r.top - ph - 8;
      peekEl.style.left = left / z + 'px';
      peekEl.style.top = Math.max(8, top) / z + 'px';
      peekEl.classList.add('show');
    };
    if (isImg) {
      media.onload = () => { fillMeta(media.naturalWidth, media.naturalHeight); place(); };
    } else {
      media.muted = true;
      media.loop = true;
      media.playsInline = true;
      media.preload = 'metadata';
      media.onloadedmetadata = () => {
        if (isFinite(media.duration) && media.duration > 4) media.currentTime = media.duration * 0.2;
        media.play().catch(() => {});
        fillMeta(media.videoWidth, media.videoHeight);
        place();
      };
    }
    media.onerror = () => { if (peekFor === anchor) histPeekHide(); };
    media.src = convertFileSrc(path);
    peekEl.append(media, cap);
  }, 220);
}

let lastHistTop = null, shownHist = new Set();
/** Recent Activity entry → short type label for the meta line. */
function histKind(preset = '') {
  if (preset === 'Screen recording') return 'Recording';
  if (preset === 'GIF recording') return 'GIF';
  if (preset.endsWith('(watcher)')) return preset.replace(/\s*\(watcher\)$/, '') + ' · auto';
  return preset || 'Conversion';
}

async function renderHistory() {
  const list = document.getElementById('history-list');
  histPeekHide();
  try {
    const hist = await invoke('get_history_cmd', { maxAgeDays: settings.histDays || 0 });
    document.getElementById('ra-count').textContent = hist && hist.length ? String(hist.length) : '';
    if (!hist || hist.length === 0) {
      list.innerHTML = '<div class="muted">Nothing yet — your captures, downloads and conversions show up here</div>';
      lastHistTop = null;
      return;
    }
    list.innerHTML = '';
    // A new entry on top (not the first render) gets a short highlight
    const freshTop = lastHistTop !== null && hist[0].when !== lastHistTop;
    lastHistTop = hist[0].when;
    const shownWhens = shownHist;            // rows already on screen don't animate again
    shownHist = new Set(hist.map((h) => h.when));
    // The list has a fixed height and scrolls — all entries, no "Show all"
    const showing = hist.length;
    for (let i = 0; i < showing; i++) {
      const h = hist[i];
      const row = document.createElement('div');
      row.className = 'history-row' + (i === 0 && freshTop ? ' fresh' : '') + (shownWhens.has(h.when) ? ' seen' : '');
      row.style.setProperty('--i', String(Math.min(i, 12)));   // staggered entrance
      row.dataset.idx = String(i);
      const status = h.ok
        ? '<span class="status ok" title="Done"></span>'
        : '<span class="status fail" title="Failed"></span>';
      const label = histKind(h.preset);
      const meta = [ago(h.when), label, h.size_mb > 0.05 ? `${h.size_mb >= 10 ? Math.round(h.size_mb) : h.size_mb.toFixed(1)} MB` : '']
        .filter(Boolean).join(' · ');
      const fname = escapeHtml(h.in_file);
      const fullTitle = escapeHtml(`${h.in_file}\n→ ${h.out_file}\n${h.preset}${h.path ? '\n' + h.path : ''}`);
      // Files with a hover preview carry their info in it — a native tooltip
      // would pop up on top of the preview
      const peekable = !!(h.path && h.ok && (PEEK_IMG.test(h.path) || PEEK_VID.test(h.path)));
      // Open / Show in folder when the entry knows its file (newer entries)
      const acts = h.path && h.ok ? `
        <span class="hist-acts">
          <button class="hist-act" data-act="copy" title="Copy to clipboard"><svg class="ic"><use href="#i-clipboard"/></svg></button>
          <button class="hist-act" data-act="folder" title="Show in folder"><svg class="ic"><use href="#i-folder"/></svg></button>
        </span>` : '<span></span>';
      row.innerHTML = `
        <div class="hr-main">
          <div class="name"${peekable ? '' : ` title="${fullTitle}"`}>${fname}</div>
          <div class="hr-meta">${escapeHtml(meta)}</div>
        </div>
        ${acts}
        ${status}
        <button class="hist-del" data-idx="${i}" aria-label="Remove from the list" title="Remove from the list">✕</button>
      `;
      if (h.path && h.ok) {
        row.classList.add('has-file');
        // A file that vanished since the last render: say so and drop the row
        const gone = (e) => {
          if (String(e) !== 'missing') {
            showToast('Could not open the file', String(e), 'fail');
            return;
          }
          showToast('File not found', 'It was moved or deleted — removed from Recent Activity.', 'fail');
          renderHistory();
        };
        const openIt = () => invoke('open_file', { path: h.path }).catch(gone);
        row.querySelector('.name').addEventListener('click', openIt);
        const nameEl = row.querySelector('.name');
        nameEl.addEventListener('mouseenter', () => histPeekShow(nameEl, h.path, h));
        nameEl.addEventListener('mouseleave', histPeekHide);
        nameEl.addEventListener('click', histPeekHide);
        row.querySelector('[data-act="copy"]').addEventListener('click', async (e) => {
          e.stopPropagation();
          const b = e.currentTarget;
          try {
            const kind = await invoke('copy_to_clipboard', { path: h.path });
            showToast('Copied to clipboard', kind === 'image' ? 'Paste the image with Ctrl+V' : 'Paste the file with Ctrl+V — Discord, chats, a folder…', 'ok', 2500);
            b.classList.add('done');                     // brief tick on the button
            b.querySelector('use').setAttribute('href', '#i-check');
            setTimeout(() => { b.classList.remove('done'); b.querySelector('use').setAttribute('href', '#i-clipboard'); }, 1400);
          } catch (err) {
            if (String(err) === 'missing') gone(err);
            else showToast('Could not copy', String(err), 'fail');
          }
        });
        row.querySelector('[data-act="folder"]').addEventListener('click', (e) => {
          e.stopPropagation();
          invoke('open_file', { path: h.path, reveal: true }).catch(gone);
        });
      }
      // × deletes the file itself (Recycle Bin or permanently — Settings);
      // rows without a file are just removed from the list
      const del = row.querySelector('.hist-del');
      if (h.path && h.ok) {
        const permanent = settings.deleteMode === 'permanent';
        del.title = permanent ? 'Delete the file permanently' : 'Move the file to the Recycle Bin';
        del.setAttribute('aria-label', del.title);
      }
      del.onclick = async (e) => {
        e.stopPropagation();
        if (!(h.path && h.ok)) {
          await invoke('delete_history_entry', { index: Number(e.currentTarget.dataset.idx) });
          await renderHistory();
          return;
        }
        const permanent = settings.deleteMode === 'permanent';
        const name = h.path.split(/[\\/]/).pop();
        if (permanent && !confirm(`Delete "${name}" permanently?\nThis can't be undone.`)) return;
        try {
          await invoke('delete_file', { path: h.path, permanent });
          showToast(permanent ? 'File deleted' : 'Moved to the Recycle Bin', name, 'ok', 2500);
        } catch (err) {
          showToast('Could not delete the file', String(err), 'fail');
        }
        await renderHistory();
      };
      list.appendChild(row);
    }
  } catch (e) {
    list.innerHTML = `<div class="muted">history error: ${e}</div>`;
  }
}

// ============================================================
//  Preset list rendering
// ============================================================
// Display groups — by what the user wants to do, not by backend category.
// "Mute video" lives with Audio; anything unknown (custom presets) → Custom.
const PRESET_GROUPS = [
  { key: 'convert',    label: 'Convert',         icon: 'refresh',  match: (p) => p.category === 'Convert' },
  { key: 'share',      label: 'Discord & size',  icon: 'discord',  match: (p) => p.category === 'Compress' },
  { key: 'trim',       label: 'Trim',            icon: 'scissors', match: (p) => p.category === 'Trim' },
  { key: 'resize',     label: 'Resize',          icon: 'minimize', match: (p) => p.category === 'Resize' },
  { key: 'audio',      label: 'Audio',           icon: 'music',    match: (p) => p.category === 'Audio' || (p.category === 'Utility' && p.id === 8) },
  { key: 'effects',    label: 'Effects',         icon: 'gauge',    match: (p) => p.category === 'Effects' },
  { key: 'shadowplay', label: 'Shadowplay → AE', icon: 'film',     match: (p) => p.category === 'Shadowplay' },
  { key: 'tools',      label: 'Tools',           icon: 'wrench',   match: (p) => p.category === 'Utility' },
  { key: 'custom',     label: 'Custom',          icon: 'code',     match: () => true },
];
function presetGroup(p) {
  return PRESET_GROUPS.find((g) => g.match(p));
}

// ★ recommended preset per "Recommended preset" setting
const USECASE_PRESET = { auto: 1, premiere: 2, ae: 3, share: 13 };
function recommendedPresetId() {
  const id = USECASE_PRESET[settings.usecase] ?? 1;
  return presets.some((p) => p.id === id) ? id : (presets.find((p) => p.star)?.id ?? presets[0]?.id);
}

// Short feature tags shown on cards / in the info panel, derived from args.
function presetTags(p) {
  const a = (p.args || []).join(' ');
  const tags = [];
  if (p.id === recommendedPresetId()) tags.push(['Recommended', 'hot']);
  if (p.target_size_mb) tags.push([`≤ ${p.target_size_mb} MB`, '']);
  if (/\{VENC:/.test(a)) tags.push(['GPU', 'gpu']);
  const reencodes = /\{VENC:|libx26|-c:v (?!copy)|-filter_complex|-af /.test(a);
  if (!reencodes && /\bcopy\b/.test(a)) tags.push(['No re-encode', 'fast']);
  if (isTrimPreset(p)) tags.push(['Trim editor', '']);
  if (p.hdr_thumbnail || /tonemap/.test(a)) tags.push(['HDR → SDR', '']);
  if (p.ext && p.ext !== 'mp4') tags.push([p.ext.toUpperCase(), '']);
  return tags;
}
function tagsHtml(tags) {
  return tags.map(([t, cls]) => `<span class="tag ${cls}">${escapeHtml(t)}</span>`).join('');
}

let presetFilter = 'all';     // active category tab
let presetQuery = '';         // search box text

async function renderPresets() {
  // Every preset is hardware-adaptive (the encoder is chosen per machine at
  // convert time), so nothing is hidden on non-NVIDIA systems.
  presets = await invoke('list_presets');
  renderCategoryTabs();
  renderPresetGrid();
  // Start on the last used preset (setting), else the recommended one.
  const last = settings.rememberPreset ? presets.find((p) => p.id === settings.lastPreset) : null;
  const start = last?.id ?? recommendedPresetId();
  if (start != null) selectPreset(start, { remember: false });
}

function renderCategoryTabs() {
  const nav = document.getElementById('cat-tabs');
  const counts = {};
  for (const p of presets) { const g = presetGroup(p); counts[g.key] = (counts[g.key] || 0) + 1; }
  const tabs = [{ key: 'all', label: 'All', icon: 'grid' }, ...PRESET_GROUPS.filter((g) => counts[g.key])];
  nav.innerHTML = tabs.map((g) => `
    <button class="cat-tab${g.key === presetFilter ? ' active' : ''}" data-key="${g.key}">
      <svg class="ic"><use href="#i-${g.icon}"/></svg>${escapeHtml(g.label)}
      <span class="count">${g.key === 'all' ? presets.length : counts[g.key]}</span>
    </button>`).join('');
  nav.querySelectorAll('.cat-tab').forEach((b) => {
    b.onclick = () => {
      presetFilter = b.dataset.key;
      nav.querySelectorAll('.cat-tab').forEach((x) => x.classList.toggle('active', x === b));
      renderPresetGrid();
    };
  });
}

// Search the short texts first (name, description, category, tags); the long
// info text only when nothing matched — otherwise "audio" would also list
// every preset whose description mentions its audio bitrate.
function presetMatchesQuery(p, q, deep) {
  if (!q) return true;
  const tags = presetTags(p).map(([t]) => t).join(' ');
  let hay = `${p.name} ${p.desc} ${presetGroup(p).label} ${tags}`;
  if (deep) hay += ` ${p.info} ${p.specs}`;
  hay = hay.toLowerCase();
  return q.split(/\s+/).every((w) => hay.includes(w));
}

function renderPresetGrid() {
  const list = document.getElementById('preset-list');
  const q = presetQuery.trim().toLowerCase();
  // A search looks through every category.
  const filter = q ? 'all' : presetFilter;
  const deep = !!q && !presets.some((p) => presetMatchesQuery(p, q, false));
  list.innerHTML = '';
  let shown = 0;
  for (const g of PRESET_GROUPS) {
    if (filter !== 'all' && filter !== g.key) continue;
    const items = presets.filter((p) => presetGroup(p) === g && presetMatchesQuery(p, q, deep));
    if (items.length === 0) continue;
    if (filter === 'all') {
      const head = document.createElement('div');
      head.className = 'grid-head';
      head.textContent = g.label;
      list.appendChild(head);
    }
    for (const p of items) {
      const card = document.createElement('button');
      card.className = 'preset-card';
      card.dataset.id = p.id;
      card.setAttribute('role', 'option');
      const star = p.id === recommendedPresetId()
        ? '<svg class="pc-star" aria-label="recommended"><use href="#i-star"/></svg>' : '';
      // Custom presets come from a user-edited JSON file — escape everything.
      card.innerHTML = `
        <span class="pc-icon"><svg class="ic"><use href="#i-${g.icon}"/></svg></span>
        <span class="pc-body">
          <span class="pc-name">${escapeHtml(p.name)}${star}</span>
          <span class="pc-desc">${escapeHtml(p.desc)}</span>
          <span class="pc-tags">${tagsHtml(presetTags(p).filter(([t]) => t !== 'Recommended').slice(0, 3))}</span>
        </span>`;
      card.onclick = () => selectPreset(p.id);
      list.appendChild(card);
      shown++;
    }
  }
  if (shown === 0) {
    list.innerHTML = `<div class="preset-empty">No preset matches “${escapeHtml(presetQuery.trim())}”</div>`;
  }
  markSelectedCard();
}

function markSelectedCard() {
  document.querySelectorAll('.preset-card').forEach((c) => {
    const on = Number(c.dataset.id) === selectedPreset?.id;
    c.classList.toggle('selected', on);
    c.setAttribute('aria-selected', String(on));
  });
}

function selectPreset(id, { remember = true } = {}) {
  selectedPreset = presets.find((p) => p.id === id) || null;
  markSelectedCard();
  if (remember && selectedPreset) {
    settings.lastPreset = selectedPreset.id;
    saveSettings();
    // Show the new preset's description from the top
    document.querySelector('.picker-right').scrollTop = 0;
  }
  renderInfoPanel();
  renderParamInputs();
  updateStartButton();
}

function renderInfoPanel() {
  const name = document.getElementById('info-name');
  const text = document.getElementById('info-text');
  const specs = document.getElementById('info-specs');
  const cat = document.getElementById('info-cat');
  const tags = document.getElementById('info-tags');
  const icon = document.getElementById('info-icon');
  if (!selectedPreset) {
    name.textContent = '—';
    cat.textContent = '';
    text.textContent = 'Select a preset on the left.';
    specs.textContent = '';
    tags.innerHTML = '';
    return;
  }
  const g = presetGroup(selectedPreset);
  icon.innerHTML = `<svg class="ic"><use href="#i-${g.icon}"/></svg>`;
  cat.textContent = g.label;
  name.textContent = selectedPreset.name;
  tags.innerHTML = tagsHtml(presetTags(selectedPreset));
  text.textContent = selectedPreset.info || selectedPreset.desc;
  specs.textContent = selectedPreset.specs || '';
}

// Search box: filter as you type; "/" focuses it, Esc clears it.
{
  const search = document.getElementById('preset-search');
  search.addEventListener('input', () => {
    presetQuery = search.value;
    document.querySelectorAll('.cat-tab').forEach((x) =>
      x.classList.toggle('active', presetQuery.trim() ? x.dataset.key === 'all' : x.dataset.key === presetFilter));
    renderPresetGrid();
  });
  search.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      if (search.value) { search.value = ''; search.dispatchEvent(new Event('input')); }
      else search.blur();
    } else if (e.key === 'Enter' || e.key === 'ArrowDown') {
      // Jump into the results: select the first match
      e.preventDefault();
      const first = document.querySelector('.preset-card');
      if (first) { selectPreset(Number(first.dataset.id)); first.focus(); }
    }
  });
}

function isTrimPreset(p) {
  if (!p?.params) return false;
  const names = p.params.map(x => x.name);
  return names.includes('start') && names.includes('end');
}

function renderParamInputs() {
  const area = document.getElementById('params-area');
  const trimEditor = document.getElementById('trim-editor');
  area.innerHTML = '';
  const params = selectedPreset?.params || [];

  // Trim preset → use rich editor, hide simple inputs
  if (isTrimPreset(selectedPreset)) {
    area.classList.add('hidden');
    trimEditor.classList.remove('hidden');
    // (re)load thumbnails for first staged file if available
    maybeLoadTrimThumbs();
    return;
  }
  trimEditor.classList.add('hidden');

  if (params.length === 0) {
    area.classList.add('hidden');
    return;
  }
  area.classList.remove('hidden');
  for (const p of params) {
    const row = document.createElement('div');
    row.className = 'param-row';
    // Custom presets (user JSON) can put anything here — escape it all.
    const n = escapeHtml(p.name);
    row.innerHTML = `
      <label for="param-${n}">${escapeHtml(p.label)}</label>
      <input type="text" id="param-${n}" name="${n}"
             value="${escapeHtml(p.default_value)}" placeholder="${escapeHtml(p.placeholder)}" />
    `;
    area.appendChild(row);
  }
}

function collectParamValues() {
  const out = {};
  // Standard param inputs
  document.querySelectorAll('.params-area input').forEach(inp => {
    out[inp.name] = inp.value.trim();
  });
  // Trim editor inputs (if active)
  if (isTrimPreset(selectedPreset) && !document.getElementById('trim-editor').classList.contains('hidden')) {
    const s = document.getElementById('trim-input-start')?.value.trim();
    const e = document.getElementById('trim-input-end')?.value.trim();
    if (s) out.start = s;
    if (e) out.end = e;
  }
  return out;
}

// ============================================================
//  Trim editor — thumbnail strip + dual-handle slider
// ============================================================
let trimDurationMs = 0;
let trimStartMs = 0;
let trimEndMs = 0;
let trimThumbsLoadedFor = null;  // path we already requested

function fmtTime(ms) {
  if (!Number.isFinite(ms) || ms < 0) ms = 0;
  const totalS = ms / 1000;
  const h = Math.floor(totalS / 3600);
  const m = Math.floor((totalS % 3600) / 60);
  const s = (totalS % 60);
  const pad = (n) => String(Math.floor(n)).padStart(2, '0');
  // 1 decimal place since trim handles snap in 100ms increments
  const sStr = s.toFixed(1).padStart(4, '0');
  return `${pad(h)}:${pad(m)}:${sStr}`;
}
function parseTime(s) {
  if (!s) return NaN;
  s = s.trim();
  const parts = s.split(':').map(x => x.trim());
  if (parts.length === 1) return parseFloat(parts[0]) * 1000;
  if (parts.length === 2) {
    return (parseFloat(parts[0]) * 60 + parseFloat(parts[1])) * 1000;
  }
  if (parts.length === 3) {
    return (parseFloat(parts[0]) * 3600 + parseFloat(parts[1]) * 60 + parseFloat(parts[2])) * 1000;
  }
  return NaN;
}

async function maybeLoadTrimThumbs() {
  const wrap = document.getElementById('trim-video-wrap');
  const firstFile = stagedFiles[0];
  if (!firstFile) {
    document.getElementById('trim-strip').innerHTML =
      '<div class="trim-strip-loading">drop a file to preview</div>';
    setTrimRange(0, 0, 0);
    // Clear video too
    const v = document.getElementById('trim-video');
    v.removeAttribute('src');
    v.load?.();
    wrap.classList.remove('error', 'playing', 'loading');
    return;
  }
  if (trimThumbsLoadedFor === firstFile.path) return; // already loaded

  // Show the loader overlay until thumbnails are ready
  wrap.classList.add('loading');
  wrap.classList.remove('error', 'playing');

  // Load video preview immediately (don't wait for thumbnails)
  loadTrimVideo(firstFile.path);

  document.getElementById('trim-strip').innerHTML =
    '<div class="trim-strip-loading">generating preview…</div>';
  try {
    const res = await invoke('generate_trim_thumbnails', { path: firstFile.path, count: 25 });
    trimThumbsLoadedFor = firstFile.path;
    renderTrimStrip(res.thumbnails);
    trimDurationMs = res.duration_ms;
    setTrimRange(0, res.duration_ms, res.duration_ms);
  } catch (err) {
    document.getElementById('trim-strip').innerHTML =
      `<div class="trim-strip-loading">preview failed: ${err}</div>`;
  } finally {
    wrap.classList.remove('loading');
  }
}

async function loadTrimVideo(filePath) {
  const v = document.getElementById('trim-video');
  const wrap = document.getElementById('trim-video-wrap');
  wrap.classList.remove('error', 'playing');
  try {
    let srcPath = filePath;
    try {
      const proxy = await invoke('ensure_hdr_proxy', { path: filePath });
      if (proxy) srcPath = proxy;
    } catch (e) {
      console.debug('HDR proxy not generated, using original:', e);
    }
    v.src = convertFileSrc(srcPath);
    v.currentTime = 0;
    v.pause();
  } catch (err) {
    console.error('video load failed:', err);
    wrap.classList.add('error');
  }
}

function renderTrimStrip(thumbs) {
  const strip = document.getElementById('trim-strip');
  strip.innerHTML = '';
  for (const src of thumbs) {
    const img = document.createElement('img');
    img.src = src;
    img.draggable = false;
    strip.appendChild(img);
  }
}

function setTrimRange(startMs, endMs, durationMs) {
  trimStartMs = Math.max(0, Math.min(startMs, durationMs));
  trimEndMs   = Math.max(trimStartMs, Math.min(endMs, durationMs));
  trimDurationMs = durationMs;
  // Update handle positions
  const pctStart = durationMs ? (trimStartMs / durationMs) : 0;
  const pctEnd   = durationMs ? (trimEndMs   / durationMs) : 1;
  const hStart = document.getElementById('trim-handle-start');
  const hEnd   = document.getElementById('trim-handle-end');
  const range  = document.getElementById('trim-range');
  hStart.style.left = `${pctStart * 100}%`;
  hEnd.style.left   = `${pctEnd * 100}%`;
  range.style.left  = `${pctStart * 100}%`;
  range.style.width = `${(pctEnd - pctStart) * 100}%`;
  // Strip dim overlays
  document.getElementById('trim-dim-left').style.width  = `${pctStart * 100}%`;
  document.getElementById('trim-dim-right').style.width = `${(1 - pctEnd) * 100}%`;
  // Time inputs
  const inS = document.getElementById('trim-input-start');
  const inE = document.getElementById('trim-input-end');
  if (document.activeElement !== inS) inS.value = fmtTime(trimStartMs);
  if (document.activeElement !== inE) inE.value = fmtTime(trimEndMs);
  // Duration display
  document.getElementById('trim-duration').textContent =
    `selection: ${fmtTime(trimEndMs - trimStartMs)}  /  total: ${fmtTime(trimDurationMs)}`;
}

function setupTrimSlider() {
  const slider = document.getElementById('trim-slider');
  const hStart = document.getElementById('trim-handle-start');
  const hEnd   = document.getElementById('trim-handle-end');
  const video  = document.getElementById('trim-video');
  const wrap   = document.getElementById('trim-video-wrap');
  const overlay = document.getElementById('trim-play-overlay');

  function startDrag(handle) {
    return (ev) => {
      ev.preventDefault();
      video.pause();    // pause while scrubbing
      const rect = slider.getBoundingClientRect();
      function onMove(e) {
        const x = (e.clientX ?? e.touches?.[0]?.clientX) - rect.left;
        const pct = Math.max(0, Math.min(1, x / rect.width));
        const ms = pct * trimDurationMs;
        if (handle === 'start') {
          setTrimRange(Math.min(ms, trimEndMs - 100), trimEndMs, trimDurationMs);
          if (video.readyState >= 1) video.currentTime = trimStartMs / 1000;
        } else {
          setTrimRange(trimStartMs, Math.max(ms, trimStartMs + 100), trimDurationMs);
          if (video.readyState >= 1) video.currentTime = Math.max(0, (trimEndMs - 500)) / 1000;
        }
      }
      function onUp() {
        window.removeEventListener('pointermove', onMove);
        window.removeEventListener('pointerup', onUp);
      }
      window.addEventListener('pointermove', onMove);
      window.addEventListener('pointerup', onUp);
    };
  }
  hStart.addEventListener('pointerdown', startDrag('start'));
  hEnd.addEventListener('pointerdown', startDrag('end'));

  // Time inputs → slider sync
  document.getElementById('trim-input-start').addEventListener('change', (e) => {
    const ms = parseTime(e.target.value);
    if (!Number.isFinite(ms)) return;
    setTrimRange(ms, trimEndMs, trimDurationMs);
    if (video.readyState >= 1) video.currentTime = trimStartMs / 1000;
  });
  document.getElementById('trim-input-end').addEventListener('change', (e) => {
    const ms = parseTime(e.target.value);
    if (!Number.isFinite(ms)) return;
    setTrimRange(trimStartMs, ms, trimDurationMs);
  });

  // Click on timeline = seek video (if not on a handle)
  slider.addEventListener('click', (e) => {
    if (e.target.classList.contains('trim-handle')) return;
    const rect = slider.getBoundingClientRect();
    const pct = (e.clientX - rect.left) / rect.width;
    const ms = pct * trimDurationMs;
    if (video.readyState >= 1) video.currentTime = ms / 1000;
  });

  // Re-position handles + playhead if window resizes
  window.addEventListener('resize', () => {
    if (trimDurationMs > 0) setTrimRange(trimStartMs, trimEndMs, trimDurationMs);
  });

  // Video events
  overlay.addEventListener('click', () => {
    if (video.paused) {
      // If outside trim range, snap to start first
      const tMs = video.currentTime * 1000;
      if (tMs < trimStartMs - 50 || tMs > trimEndMs) {
        video.currentTime = trimStartMs / 1000;
      }
      video.play().catch(err => console.error('play failed:', err));
    } else {
      video.pause();
    }
  });
  video.addEventListener('play',  () => wrap.classList.add('playing'));
  video.addEventListener('pause', () => wrap.classList.remove('playing'));
  video.addEventListener('loadedmetadata', () => {
    // If we didn't get duration from ffprobe yet, use video's metadata
    if (trimDurationMs === 0 && Number.isFinite(video.duration)) {
      trimDurationMs = video.duration * 1000;
      setTrimRange(0, trimDurationMs, trimDurationMs);
    }
    document.getElementById('trim-playhead').classList.add('visible');
  });
  video.addEventListener('error', () => {
    console.error('video element error:', video.error);
    wrap.classList.add('error');
  });
  video.addEventListener('timeupdate', () => {
    const tMs = video.currentTime * 1000;
    // Loop within selected range
    if (tMs > trimEndMs && !video.paused) {
      video.currentTime = trimStartMs / 1000;
      return;
    }
    // Update playhead position
    if (trimDurationMs > 0) {
      const pct = Math.max(0, Math.min(1, tMs / trimDurationMs));
      document.getElementById('trim-playhead').style.left = `${pct * 100}%`;
    }
  });
}

// Init slider once at startup. Script is at end of body so DOM is already
// parsed by the time we run — single direct call is enough.
setupTrimSlider();

// ============================================================
//  File staging
// ============================================================
function isVideoPath(p) {
  return /\.(mp4|mkv|mov|m4v|webm)$/i.test(p);
}

function addFiles(paths) {
  for (const p of paths) {
    if (!isVideoPath(p)) continue;
    if (stagedFiles.find(f => f.path === p)) continue;
    const name = p.split(/[\\/]/).pop();
    stagedFiles.push({ path: p, name });
  }
  renderStagedFiles();
}

function removeFile(path) {
  stagedFiles = stagedFiles.filter(f => f.path !== path);
  renderStagedFiles();
}

function renderStagedFiles() {
  const wrap = document.getElementById('file-staging');
  wrap.innerHTML = '';
  for (const f of stagedFiles) {
    const div = document.createElement('div');
    div.className = 'staged-file';
    const safePath = escapeHtml(f.path);
    const safeName = escapeHtml(f.name);
    div.innerHTML = `
      <span class="name" title="${safePath}">${safeName}</span>
      <button class="x" data-path="${safePath}" aria-label="remove" title="Remove"><svg class="ic"><use href="#i-x"/></svg></button>
    `;
    div.querySelector('.x').onclick = (e) => removeFile(e.currentTarget.dataset.path);
    wrap.appendChild(div);
  }
  document.getElementById('drop-zone').classList.toggle('has-files', stagedFiles.length > 0);
  document.getElementById('btn-pick-label').textContent = stagedFiles.length ? 'Add more files…' : 'Pick files…';
  updateStartButton();
  // Refresh trim thumbnails if a Trim preset is active and the first staged file changed
  if (isTrimPreset(selectedPreset)) {
    const firstPath = stagedFiles[0]?.path || null;
    if (firstPath !== trimThumbsLoadedFor) {
      trimThumbsLoadedFor = null;     // force reload
      maybeLoadTrimThumbs();
    }
  }
}

function updateStartButton() {
  const btn = document.getElementById('btn-start');
  btn.classList.toggle('hidden', stagedFiles.length === 0 || !selectedPreset);
  if (stagedFiles.length > 0 && selectedPreset) {
    const n = stagedFiles.length;
    btn.innerHTML = `Start · ${n} ${n === 1 ? 'file' : 'files'}<svg class="ic"><use href="#i-arrow-right"/></svg>`;
  }
}

// ============================================================
//  File picker dialog
// ============================================================
document.getElementById('btn-pick-files').onclick = async () => {
  const sel = await openDialog({
    multiple: true,
    filters: [
      { name: 'Video', extensions: ['mp4', 'mkv', 'mov', 'm4v', 'webm'] }
    ]
  });
  if (!sel) return;
  const arr = Array.isArray(sel) ? sel : [sel];
  addFiles(arr);
};

// Welcome hero drop zone: click = open the preset picker (files can then be
// dropped there or chosen with "Pick files…"). Ctrl+O still opens the dialog.
document.getElementById('welcome-drop').onclick = () => showView('picker');

// ============================================================
//  Drag & drop (Tauri 2 webview drop)
// ============================================================
win.onDragDropEvent((event) => {
  // Highlight whichever drop target is visible (welcome hero or picker zone).
  const zones = [document.getElementById('drop-zone'), document.getElementById('welcome-drop')];
  const setOver = (on) => zones.forEach((z) => z?.classList.toggle('dragover', on));
  if (event.payload.type === 'over' || event.payload.type === 'enter') {
    setOver(true);
  } else if (event.payload.type === 'leave') {
    setOver(false);
  } else if (event.payload.type === 'drop') {
    setOver(false);
    addFiles(event.payload.paths || []);
    if (views.welcome.classList.contains('active')) {
      showView('picker');
    }
  }
});

// ============================================================
//  URL detection: paste a URL anywhere → jump to Download view
// ============================================================
const URL_RE = /^(https?:\/\/)[^\s]+$/i;
document.addEventListener('paste', (e) => {
  // Ignore paste events when typing into an input — the user just wants to paste text
  if (isTypingInInput(e.target)) return;
  if (!document.getElementById('onboard').classList.contains('hidden')) return;
  const text = (e.clipboardData || window.clipboardData)?.getData('text')?.trim();
  if (!text || !URL_RE.test(text)) return;
  // Prefill download view with the pasted URL
  // Pasting from elsewhere means "download this"; inside Quick share keep sharing.
  if (activeViewName() !== 'download') setShareMode(linkMode() === 'share');
  document.getElementById('download-url').value = text;
  document.getElementById('download-url').dispatchEvent(new Event('input', { bubbles: true }));
  showView('download');
  // Preserve live progress if a previous download is still running.
  if (!downloadInProgress) resetDownloadView();
});

// ============================================================
//  Navigation
// ============================================================
document.getElementById('btn-back-welcome').onclick = () => {
  stagedFiles = [];
  renderStagedFiles();
  showView('welcome');
  renderHistory();
};

// ============================================================
//  Settings panel
// ============================================================
// Settings → System → Detected hardware: [icon, label, value, tags]. Tags say
// what the hardware means for Nibble (which encoder runs), not just "found".
const ENCODER_LABEL = {
  nvenc: ['GPU · NVIDIA NVENC', 'Fast'],
  amf:   ['GPU · AMD AMF', 'Fast'],
  qsv:   ['GPU · Intel Quick Sync', 'Fast'],
  cpu:   ['CPU · x264 / x265', 'Slower'],
};
function hardwareRows(hw) {
  const rows = [];
  const gpuTags = [];
  if (hw.nvenc_available) gpuTags.push(['NVENC', 'gpu']);
  else if (hw.gpu_vendor === 'AMD') gpuTags.push(['AMF', 'gpu']);
  else if (hw.gpu_vendor === 'Intel') gpuTags.push(['Quick Sync', 'gpu']);
  if (hw.nvenc_av1) gpuTags.push(['AV1 encode', 'gpu']);
  rows.push(['monitor', 'Graphics', hw.gpu_name || 'No supported GPU found', gpuTags]);
  if (!hw.ffmpeg_found) {
    rows.push(['zap', 'Encoding', 'ffmpeg not installed', [['Install from the home screen', 'hot']]]);
  } else {
    const [text, speed] = ENCODER_LABEL[hw.encoder] || ENCODER_LABEL.cpu;
    rows.push(['zap', 'Encoding', text, [[speed, hw.encoder === 'cpu' ? 'fast' : 'gpu']]]);
  }
  const cpuBits = [];
  if (hw.cpu_cores) cpuBits.push([`${hw.cpu_cores} cores`, '']);
  if (hw.cpu_threads) cpuBits.push([`${hw.cpu_threads} threads`, '']);
  rows.push(['cpu', 'Processor', hw.cpu_name ? hw.cpu_name.replace(/\s+/g, ' ') : 'Unknown', cpuBits]);
  const ramTags = [];
  if (hw.ram_type) ramTags.push([hw.ram_mts ? `${hw.ram_type}-${hw.ram_mts}` : hw.ram_type, '']);
  if (hw.ram_sticks) ramTags.push([`${hw.ram_sticks} ${hw.ram_sticks === 1 ? 'module' : 'modules'}`, '']);
  rows.push(['layers', 'Memory', hw.ram_gb ? `${hw.ram_gb} GB` : 'Unknown', ramTags]);
  rows.push(['hard-drive', 'System', hw.os_name || 'Unknown', []]);
  return rows;
}

function applySettingsToUI() {
  const $ = (id) => document.getElementById(id);
  syncUiScaleSeg();
  $('set-reduce-motion').checked = !!settings.reduceMotion;
  $('set-decor').checked = settings.decor !== false;
  $('set-remember-window').value = settings.rememberWindow ? 'last' : 'default';
  $('settings-convert-dir').value = settings.convertDir || '';
  $('set-overwrite').value = settings.overwrite ? 'overwrite' : 'keep';
  $('settings-usecase').value = USECASE_PRESET[settings.usecase] ? settings.usecase : 'auto';
  $('set-remember-preset').checked = settings.rememberPreset !== false;
  $('set-open-folder').checked = !!settings.openFolderAfter;
  $('settings-notif').checked = settings.notif !== false;
  $('settings-confirm-clear-history').checked = settings.confirmClearHistory !== false;
  $('set-hist-days').value = String(settings.histDays || 0);
  $('set-delete-mode').value = settings.deleteMode === 'permanent' ? 'permanent' : 'trash';
  // Download / share defaults live in the Download view's own controls.
  $('set-dl-dir').value = savedirInput.value;
  $('set-dl-quality').value = document.getElementById('download-quality').value;
  $('set-dl-cookies').value = browserSelect.value;
  $('set-ytdlp-check').checked = settings.ytdlpAutoCheck !== false;
  $('set-ytdlp-ver').textContent = ytdlpStatus.available
    ? `Installed: yt-dlp v${ytdlpStatus.version || '?'}` : 'yt-dlp is not installed';
  $('set-ytdlp-update').disabled = !ytdlpStatus.available;
  $('set-share-fit').value = fitSelect.value;
  $('set-share-ttl').value = String(settings.shareTtlMin || 10);
  paintHotkeys();
  $('set-shot-action').value = settings.shotAction || 'copy';
  $('set-shot-dir').value = settings.shotDir || '';
  $('set-shot-popup').checked = settings.shotPopup !== false;
  $('set-shot-sound').checked = settings.shotSound !== false;
  $('set-shot-tools').checked = settings.shotTools !== false;
  $('set-shot-format').value = settings.shotFormat || 'png';
  fillRecSettings();

  // Hardware info
  const hwEl = document.getElementById('settings-hw');
  hwEl.innerHTML = hardwareInfo
    ? hardwareRows(hardwareInfo).map(([icon, label, value, tags]) => `
        <div class="hw-row">
          <svg class="ic"><use href="#i-${icon}"/></svg>
          <span class="hw-k">${label}</span>
          <span class="hw-v">${escapeHtml(value)}</span>
          <span class="hw-tags">${tagsHtml(tags)}</span>
        </div>`).join('')
    : '<span class="muted">Detecting…</span>';

  // Storage info
  document.getElementById('settings-storage-info').textContent = appInfo
    ? `${appInfo.data_dir}${appInfo.portable ? '  (portable — next to the exe)' : ''}`
    : '—';
}

document.getElementById('btn-open-settings').onclick = () => {
  applySettingsToUI();
  showView('settings');
  refreshToolsStatus();
  document.getElementById('settings-body').scrollTop = 0;
  document.querySelectorAll('#settings-nav .sn-item').forEach((b, i) => b.classList.toggle('active', i === 0));
};

// ============================================================
//  Popovers (Keyboard shortcuts + About)
// ============================================================
const shortcutsPopover = document.getElementById('shortcuts-popover');
const aboutPopover     = document.getElementById('about-popover');

function toggleShortcuts(open) {
  const willOpen = open ?? shortcutsPopover.classList.contains('hidden');
  if (willOpen) aboutPopover.classList.add('hidden'); // mutual exclusion
  shortcutsPopover.classList.toggle('hidden', !willOpen);
}
function toggleAbout(open) {
  const willOpen = open ?? aboutPopover.classList.contains('hidden');
  if (willOpen) {
    shortcutsPopover.classList.add('hidden');
    fillAboutHardware();
  }
  aboutPopover.classList.toggle('hidden', !willOpen);
}
function fillAboutHardware() {
  const el = document.getElementById('about-hw');
  if (!hardwareInfo) { el.textContent = '(not detected)'; return; }
  const lines = [];
  if (hardwareInfo.gpu_name) {
    let t = hardwareInfo.gpu_name;
    if (hardwareInfo.nvenc_av1) t += ' · NVENC AV1';
    else if (hardwareInfo.nvenc_available) t += ' · NVENC';
    else t += ' · no NVENC';
    lines.push(t);
  }
  const cpu = [];
  if (hardwareInfo.cpu_name)  cpu.push(hardwareInfo.cpu_name.replace(/\s+/g, ' '));
  if (hardwareInfo.cpu_cores) cpu.push(`${hardwareInfo.cpu_cores} cores`);
  if (hardwareInfo.ram_gb)    cpu.push(`${hardwareInfo.ram_gb} GB RAM`);
  if (cpu.length) lines.push(cpu.join(' · '));
  if (hardwareInfo.os_name) lines.push(hardwareInfo.os_name);
  el.innerHTML = '';
  for (const ln of lines) {
    const div = document.createElement('div');
    div.textContent = ln;
    el.appendChild(div);
  }
}

document.getElementById('btn-open-shortcuts').onclick = (e) => {
  e.stopPropagation();
  toggleShortcuts();
};
document.getElementById('btn-close-shortcuts').onclick = () => toggleShortcuts(false);
document.getElementById('btn-open-about').onclick = (e) => {
  e.stopPropagation();
  toggleAbout();
};
document.getElementById('btn-close-about').onclick = () => toggleAbout(false);

// Click anywhere outside either popover closes it
document.addEventListener('click', (e) => {
  if (!shortcutsPopover.classList.contains('hidden')
      && !shortcutsPopover.contains(e.target)
      && !e.target.closest('#btn-open-shortcuts')) {
    toggleShortcuts(false);
  }
  if (!aboutPopover.classList.contains('hidden')
      && !aboutPopover.contains(e.target)
      && !e.target.closest('#btn-open-about')) {
    toggleAbout(false);
  }
});
// Leaving Settings: the ★ preset may have changed — refresh the cards but
// keep the user's current selection.
function leaveSettings() {
  showView('welcome');
  renderHistory();
  renderPresetGrid();
  renderInfoPanel();
  applyHardwareToWelcome();
}
document.getElementById('btn-back-from-settings').onclick = leaveSettings;

// ---- Section nav: click scrolls, scrolling highlights (scroll-spy) ----
{
  const body = document.getElementById('settings-body');
  const items = Array.from(document.querySelectorAll('#settings-nav .sn-item'));
  const setActive = (id) => items.forEach((b) => b.classList.toggle('active', b.dataset.sec === id));
  let spyLock = 0;
  items.forEach((b) => {
    b.onclick = () => {
      setActive(b.dataset.sec);
      spyLock = performance.now() + 600;   // don't fight the smooth scroll
      document.getElementById(b.dataset.sec).scrollIntoView({ behavior: 'smooth', block: 'start' });
    };
  });
  body.addEventListener('scroll', () => {
    if (performance.now() < spyLock) return;
    const top = body.getBoundingClientRect().top;
    let current = items[0].dataset.sec;
    for (const b of items) {
      if (document.getElementById(b.dataset.sec).getBoundingClientRect().top - top <= 60) current = b.dataset.sec;
    }
    // At the very bottom the last (short) section can't reach the top
    if (body.scrollTop + body.clientHeight >= body.scrollHeight - 4) current = items[items.length - 1].dataset.sec;
    setActive(current);
  }, { passive: true });
}

// ---- Appearance ----
document.querySelectorAll('#set-uiscale button').forEach((b) => {
  b.onclick = () => setUiScaleFactor(Number(b.dataset.v));
});
function bindSwitch(id, key, after) {
  document.getElementById(id).addEventListener('change', (e) => {
    settings[key] = e.target.checked;
    saveSettings();
    after?.();
  });
}
bindSwitch('set-reduce-motion', 'reduceMotion', applyAppearance);
bindSwitch('set-decor', 'decor', applyAppearance);
document.getElementById('set-remember-window').addEventListener('change', (e) => {
  settings.rememberWindow = e.target.value === 'last';
  saveSettings();
  invoke('set_remember_window', { enabled: settings.rememberWindow })
    .catch((err) => console.debug('set_remember_window failed:', err));
});
document.getElementById('set-reset-window').onclick = () => {
  invoke('reset_window_size').catch((e) => console.debug('reset_window_size failed:', e));
};

// ---- Conversion ----
document.getElementById('settings-convert-dir').addEventListener('change', (e) => {
  settings.convertDir = e.target.value.trim();
  saveSettings();
});
document.getElementById('settings-pick-convert-dir').onclick = async () => {
  try {
    const folder = await openDialog({
      directory: true,
      multiple: false,
      defaultPath: settings.convertDir || undefined,
    });
    if (folder) {
      settings.convertDir = folder;
      document.getElementById('settings-convert-dir').value = folder;
      saveSettings();
    }
  } catch (e) { console.debug('folder pick failed:', e); }
};
document.getElementById('set-overwrite').addEventListener('change', (e) => {
  settings.overwrite = e.target.value === 'overwrite';
  saveSettings();
});
document.getElementById('settings-usecase').addEventListener('change', (e) => {
  settings.usecase = e.target.value;
  saveSettings();
});
bindSwitch('set-remember-preset', 'rememberPreset');
bindSwitch('set-open-folder', 'openFolderAfter');
bindSwitch('settings-notif', 'notif');
bindSwitch('settings-confirm-clear-history', 'confirmClearHistory');

// ---- Downloads (mirrors the Download view's controls, same storage) ----
document.getElementById('set-dl-dir').addEventListener('change', (e) => {
  savedirInput.value = e.target.value.trim();
  try { localStorage.setItem(SAVE_DIR_KEY, savedirInput.value); } catch (err) { /* noop */ }
});
document.getElementById('set-pick-dl-dir').onclick = async () => {
  try {
    const folder = await openDialog({ directory: true, multiple: false, defaultPath: savedirInput.value || undefined });
    if (folder) {
      savedirInput.value = folder;
      document.getElementById('set-dl-dir').value = folder;
      try { localStorage.setItem(SAVE_DIR_KEY, folder); } catch (err) { /* noop */ }
    }
  } catch (e) { console.debug('folder pick failed:', e); }
};
document.getElementById('set-dl-quality').addEventListener('change', (e) => {
  settings.dlQuality = e.target.value;
  saveSettings();
  document.getElementById('download-quality').value = e.target.value;
});
document.getElementById('set-dl-cookies').addEventListener('change', (e) => {
  browserSelect.value = e.target.value;
  try { localStorage.setItem(COOKIES_BROWSER_KEY, e.target.value); } catch (err) { /* noop */ }
  urlCookies = { url: null, value: null };
});
bindSwitch('set-ytdlp-check', 'ytdlpAutoCheck');
document.getElementById('set-ytdlp-update').onclick = async () => {
  const btn = document.getElementById('set-ytdlp-update');
  btn.disabled = true;
  document.getElementById('set-ytdlp-ver').textContent = 'Updating yt-dlp…';
  await doYtdlpUpdate();
  btn.disabled = false;
  applySettingsToUI();
};

// ---- Quick share ----
document.getElementById('set-share-fit').addEventListener('change', (e) => {
  fitSelect.value = e.target.value;
  try { localStorage.setItem(SHARE_FIT_KEY, e.target.value); } catch (err) { /* noop */ }
});
document.getElementById('set-share-ttl').addEventListener('change', (e) => {
  settings.shareTtlMin = Number(e.target.value) || 10;
  saveSettings();
});

// ---- Global shortcuts: record any combination ----
// Stored as accelerator strings the backend parses ("ctrl+shift+s",
// "alt+f9", "ctrl+digit1", "printscreen"); key names come from KeyboardEvent.code.
const HK_MODS = ['ctrl', 'alt', 'shift', 'super'];
// Keys that are fine alone (no typing conflict → no heads-up)
const HK_SOLO = /^(f([1-9]|1[0-9]|2[0-4])|printscreen|pause|scrolllock|insert|mediaplaypause|mediatracknext|mediatrackprevious)$/;
let hkRecording = null;          // the .hk-field being recorded
let hotkeysPaused = false;       // global shortcuts off while one is being recorded
function hkKeyName(code) {
  const c = code.toLowerCase();
  if (/^key[a-z]$/.test(c)) return c.slice(3);              // KeyS → s
  if (/^digit[0-9]$/.test(c)) return 'digit' + c.slice(5);   // Digit1 (backend wants the code)
  return c;                                                  // f9, numpad1, space, printscreen…
}
function hkLabel(combo) {
  if (!combo) return 'Off';
  const names = { ctrl: 'Ctrl', alt: 'Alt', shift: 'Shift', super: 'Win', printscreen: 'Print Screen',
    space: 'Space', backquote: '`', minus: '-', equal: '=', comma: ',', period: '.', slash: '/',
    backslash: '\\', semicolon: ';', quote: "'", bracketleft: '[', bracketright: ']', scrolllock: 'Scroll Lock' };
  return combo.split('+').map((k) => names[k]
    || (k.startsWith('digit') ? k.slice(5) : k.startsWith('numpad') ? 'Num ' + k.slice(6) : k.length === 1 ? k.toUpperCase() : k.charAt(0).toUpperCase() + k.slice(1)))
    .join(' + ');
}
function paintHotkeys() {
  document.querySelectorAll('.hk-field').forEach((f) => {
    const key = f.dataset.setting;
    const combo = settings[key] ?? f.dataset.default;
    const btn = f.querySelector('.hk-btn');
    if (f === hkRecording) return;
    btn.textContent = hkLabel(combo);
    btn.classList.toggle('off', !combo);
    f.querySelector('.hk-clear').hidden = !combo;
  });
}
function hkStop(field, save) {
  if (!field) return;
  field.classList.remove('recording');
  hkRecording = null;
  hotkeysPaused = false;
  if (save !== undefined) {
    settings[field.dataset.setting] = save;
    saveSettings();
  }
  paintHotkeys();
  pushCaptureConfig(save !== undefined && save !== '');
}
document.querySelectorAll('.hk-field').forEach((field) => {
  const btn = field.querySelector('.hk-btn');
  btn.addEventListener('click', () => {
    if (hkRecording) hkStop(hkRecording);
    hkRecording = field;
    field.classList.add('recording');
    btn.textContent = 'Press a shortcut…';
    // The current global shortcut would fire while it's being pressed here
    hotkeysPaused = true;
    pushCaptureConfig();
  });
  btn.addEventListener('blur', () => { if (hkRecording === field) hkStop(field); });
  field.querySelector('.hk-clear').addEventListener('click', () => hkStop(field, ''));
});
// Capture phase: the recorder wins over the app's own shortcuts
window.addEventListener('keydown', (e) => {
  if (!hkRecording) return;
  e.preventDefault();
  e.stopPropagation();
  if (e.key === 'Escape') { hkStop(hkRecording); return; }
  const key = hkKeyName(e.code);
  if (['controlleft', 'controlright', 'shiftleft', 'shiftright', 'altleft', 'altright', 'metaleft', 'metaright'].includes(key)) {
    // Only modifiers so far: show them live
    const live = HK_MODS.filter((m) => ({ ctrl: e.ctrlKey, alt: e.altKey, shift: e.shiftKey, super: e.metaKey })[m]);
    hkRecording.querySelector('.hk-btn').textContent = live.length ? hkLabel(live.join('+')) + ' + …' : 'Press a shortcut…';
    return;
  }
  // Any key or combination is allowed (like ShareX). A plain typing key is
  // taken from every app while Nibble runs, so that one gets a heads-up.
  const mods = HK_MODS.filter((m) => ({ ctrl: e.ctrlKey, alt: e.altKey, shift: e.shiftKey, super: e.metaKey })[m]);
  const combo = [...mods, key].join('+');
  if (!mods.length && !HK_SOLO.test(key)) {
    setTimeout(() => showToast('Heads-up', `${hkLabel(combo)} on its own won't type in other apps while Nibble is running.`, 'ok', 5000), 50);
  }
  const other = hkRecording.dataset.setting === 'shotHotkey' ? settings.recHotkey ?? 'ctrl+shift+r' : settings.shotHotkey ?? 'ctrl+shift+a';
  if (combo === other) {
    hkRecording.querySelector('.hk-btn').textContent = 'Already used by the other shortcut';
    return;
  }
  hkStop(hkRecording, combo);
}, true);
window.addEventListener('keyup', (e) => { if (hkRecording) { e.preventDefault(); e.stopPropagation(); } }, true);

// ---- Screenshots ----
document.getElementById('set-shot-action').addEventListener('change', (e) => {
  settings.shotAction = e.target.value;
  saveSettings();
  pushCaptureConfig();
});
document.getElementById('set-shot-dir').addEventListener('change', (e) => {
  settings.shotDir = e.target.value.trim();
  saveSettings();
  pushCaptureConfig();
});
bindSwitch('set-shot-popup', 'shotPopup', () => pushCaptureConfig());
bindSwitch('set-shot-sound', 'shotSound', () => pushCaptureConfig());
bindSwitch('set-shot-tools', 'shotTools');
document.getElementById('set-shot-format').addEventListener('change', (e) => {
  settings.shotFormat = e.target.value;
  saveSettings();
  pushCaptureConfig();
});

// ---- Recording (shared with the capture overlay's quick settings) ----
const REC_KEY = 'nibble.rec';
const REC_DEFAULTS = { format: 'mp4', countdown: 3, audio: 'off', cursor: true, fps: 30, gifFps: 15, dir: '', copy: true, maxMin: 60, maxMb: 2048 };
function loadRec() {
  try { return { ...REC_DEFAULTS, ...JSON.parse(localStorage.getItem(REC_KEY) || '{}') }; } catch (e) { return { ...REC_DEFAULTS }; }
}
function setRec(key, value) {
  const r = loadRec();            // re-read: the overlay may have changed other keys
  r[key] = value;
  try { localStorage.setItem(REC_KEY, JSON.stringify(r)); } catch (e) { /* noop */ }
}
function fillRecSettings() {
  const $ = (id) => document.getElementById(id);
  const r = loadRec();
  $('set-rec-format').value = r.format;
  $('set-rec-countdown').value = String(r.countdown);
  $('set-rec-audio').value = r.audio;
  $('set-rec-cursor').checked = !!r.cursor;
  $('set-rec-fps').value = String(r.fps);
  $('set-rec-giffps').value = String(r.gifFps);
  $('set-rec-maxmin').value = String(r.maxMin);
  $('set-rec-maxmb').value = String(r.maxMb);
  $('set-rec-copy').checked = !!r.copy;
  $('set-rec-dir').value = r.dir || '';
}
[['set-rec-format', 'format', String], ['set-rec-countdown', 'countdown', Number], ['set-rec-audio', 'audio', String],
 ['set-rec-fps', 'fps', Number], ['set-rec-giffps', 'gifFps', Number],
 ['set-rec-maxmin', 'maxMin', Number], ['set-rec-maxmb', 'maxMb', Number]].forEach(([id, key, conv]) => {
  document.getElementById(id).addEventListener('change', (e) => setRec(key, conv(e.target.value)));
});
document.getElementById('set-rec-cursor').addEventListener('change', (e) => setRec('cursor', e.target.checked));
document.getElementById('set-rec-copy').addEventListener('change', (e) => setRec('copy', e.target.checked));
document.getElementById('set-rec-dir').addEventListener('change', (e) => setRec('dir', e.target.value.trim()));
document.getElementById('set-pick-rec-dir').onclick = async () => {
  try {
    const folder = await openDialog({ directory: true, multiple: false, defaultPath: loadRec().dir || undefined });
    if (folder) {
      setRec('dir', folder);
      document.getElementById('set-rec-dir').value = folder;
    }
  } catch (e) { console.debug('folder pick failed:', e); }
};

document.getElementById('set-pick-shot-dir').onclick = async () => {
  try {
    const folder = await openDialog({ directory: true, multiple: false, defaultPath: settings.shotDir || undefined });
    if (folder) {
      settings.shotDir = folder;
      document.getElementById('set-shot-dir').value = folder;
      saveSettings();
      pushCaptureConfig();
    }
  } catch (e) { console.debug('folder pick failed:', e); }
};

// ---- System ----
document.getElementById('set-open-data').onclick = async () => {
  if (!appInfo?.data_dir) return;
  try {
    await invoke('reveal_in_explorer', { path: appInfo.data_dir });
  } catch (e) {
    showToast('Could not open folder', String(e), 'fail');
  }
};
document.getElementById('settings-reset').onclick = () => {
  if (!confirm('Reset all preferences to their defaults?\n\nHistory and the watch folder are kept.')) return;
  settings = { ...DEFAULT_SETTINGS };
  saveSettings();
  try {
    localStorage.removeItem(SAVE_DIR_KEY);
    localStorage.setItem(COOKIES_BROWSER_KEY, 'auto');
    localStorage.removeItem(SHARE_FIT_KEY);
  } catch (e) { /* noop */ }
  savedirInput.value = '';
  browserSelect.value = 'auto';
  fitSelect.value = '20';
  document.getElementById('download-quality').value = 'best';
  invoke('set_remember_window', { enabled: false }).catch(() => {});
  applyAppearance();
  applyUiScale();
  applySettingsToUI();
  showToast('Preferences reset', 'Everything is back to the defaults.', 'ok', 2500);
};
document.getElementById('settings-edit-custom-presets').onclick = async () => {
  try {
    await invoke('open_custom_presets_file');
  } catch (e) {
    showToast('Could not open presets file', String(e), 'fail');
  }
};

// ============================================================
//  Watch folder configuration (in Settings)
// ============================================================
async function loadWatchConfigUI() {
  let cfg = { enabled: false, folder: '', preset_id: 1, output_dir: '' };
  try { cfg = await invoke('get_watch_config'); } catch (e) { /* defaults */ }
  document.getElementById('watch-enabled').checked = !!cfg.enabled;
  document.getElementById('watch-folder').value = cfg.folder || '';
  document.getElementById('watch-output').value = cfg.output_dir || '';

  // Fill preset dropdown
  const sel = document.getElementById('watch-preset');
  sel.innerHTML = '';
  for (const p of presets) {
    const opt = document.createElement('option');
    opt.value = String(p.id);
    opt.textContent = `${p.name} — ${p.desc}`;
    sel.appendChild(opt);
  }
  if (presets.find(p => p.id === cfg.preset_id)) {
    sel.value = String(cfg.preset_id);
  }
  updateWatchStatusLabel(cfg);
}

function updateWatchStatusLabel(cfg) {
  const el = document.getElementById('watch-status');
  if (!cfg.enabled) {
    el.textContent = '○ Off';
    el.style.color = 'var(--text-dim)';
  } else if (!cfg.folder) {
    el.textContent = '● On — pick a folder to start watching';
    el.style.color = 'var(--red)';
  } else {
    el.textContent = `● Watching ${cfg.folder}`;
    el.style.color = 'var(--green)';
  }
}

async function applyWatchSettings() {
  const cfg = {
    enabled: document.getElementById('watch-enabled').checked,
    folder: document.getElementById('watch-folder').value.trim(),
    preset_id: Number(document.getElementById('watch-preset').value) || 1,
    output_dir: document.getElementById('watch-output').value.trim() || null,
  };
  try {
    await invoke('set_watch_config', { config: cfg });
    updateWatchStatusLabel(cfg);
  } catch (e) {
    showToast('Watch setup failed', String(e), 'fail');
    document.getElementById('watch-enabled').checked = false;
    updateWatchStatusLabel({ enabled: false, folder: cfg.folder });
  }
}

['watch-enabled','watch-folder','watch-preset','watch-output'].forEach(id => {
  const el = document.getElementById(id);
  el.addEventListener('change', applyWatchSettings);
});
document.getElementById('watch-pick-folder').onclick = async () => {
  const folder = await openDialog({ directory: true, multiple: false });
  if (folder) {
    document.getElementById('watch-folder').value = folder;
    await applyWatchSettings();
  }
};
document.getElementById('watch-pick-output').onclick = async () => {
  const folder = await openDialog({ directory: true, multiple: false });
  if (folder) {
    document.getElementById('watch-output').value = folder;
    await applyWatchSettings();
  }
};

// Refresh watcher UI whenever the settings view opens
const originalOpenSettings = document.getElementById('btn-open-settings').onclick;
document.getElementById('btn-open-settings').onclick = async () => {
  await loadWatchConfigUI();
  originalOpenSettings();
};

// Live updates: watcher events emit from Rust
listen('watch-detected', (event) => {
  // Show a brief toast-like hint via the welcome screen? For now just log.
  console.info('watch detected:', event.payload);
});
listen('watch-converted', (event) => {
  const p = event.payload;
  console.info('watch converted:', p);
  // Refresh history so the new entry appears
  renderHistory();
  // Optional desktop notification
  maybeNotify(
    p.ok ? 'Nibble · Watched file converted' : 'Nibble · Watched file failed',
    `${p.in_file} → ${p.out_file} (${p.size_mb} MB)`,
  );
});

// ============================================================
//  Keyboard shortcuts
// ============================================================
function isTypingInInput(target) {
  if (!target) return false;
  const tag = (target.tagName || '').toLowerCase();
  return tag === 'input' || tag === 'textarea' || tag === 'select' || target.isContentEditable;
}
function activeViewName() {
  for (const k of Object.keys(views)) {
    if (views[k].classList.contains('active')) return k;
  }
  return null;
}
function goBackOneLevel() {
  const v = activeViewName();
  if (v === 'settings') {
    leaveSettings();
  } else if (v === 'captures') {
    closeCaptures();
  } else if (v === 'picker' || v === 'download') {
    if (v === 'picker') {
      stagedFiles = [];
      renderStagedFiles();
    }
    showView('welcome');
    renderHistory();
  } else if (v === 'converting') {
    // During an active conversion, Esc triggers Cancel; if already finished,
    // Esc goes back to picker (same target as the on-screen "Back to menu" button).
    const summaryHidden = document.getElementById('batch-summary').classList.contains('hidden');
    if (summaryHidden) {
      document.getElementById('btn-cancel').click();
    } else {
      document.getElementById('btn-return').click();
    }
  }
}

document.addEventListener('keydown', (e) => {
  // The welcome modal owns the keyboard while it's open
  if (!document.getElementById('onboard').classList.contains('hidden')) return;
  const view = activeViewName();
  const typing = isTypingInInput(e.target);
  const mod = e.ctrlKey || e.metaKey;

  // Global: Ctrl + / Ctrl − / Ctrl 0 = interface size
  if (mod && !e.altKey && (e.key === '+' || e.key === '=' || e.code === 'NumpadAdd')) {
    e.preventDefault(); stepUiScale(1); return;
  }
  if (mod && !e.altKey && (e.key === '-' || e.code === 'NumpadSubtract')) {
    e.preventDefault(); stepUiScale(-1); return;
  }
  if (mod && !e.altKey && (e.key === '0' || e.code === 'Numpad0')) {
    e.preventDefault(); setUiScaleFactor(1); showToast('Interface size', '100 %', 'ok', 1200); return;
  }
  // Global: Ctrl+O = open file picker (welcome or picker)
  if (mod && e.key.toLowerCase() === 'o' && !typing) {
    e.preventDefault();
    if (view === 'welcome') showView('picker');
    document.getElementById('btn-pick-files').click();
    return;
  }
  // Global: Ctrl+D = "Video from a link" (last used mode)
  if (mod && e.key.toLowerCase() === 'd' && !typing) {
    e.preventDefault();
    openLinkView();
    return;
  }
  // Global: Ctrl+, = open Settings
  if (mod && e.key === ',' && !typing) {
    e.preventDefault();
    document.getElementById('btn-open-settings').click();
    return;
  }
  // Esc: close popovers first, then back one level
  if (e.key === 'Escape' && !typing) {
    e.preventDefault();
    if (!shortcutsPopover.classList.contains('hidden')) {
      toggleShortcuts(false);
      return;
    }
    if (!aboutPopover.classList.contains('hidden')) {
      toggleAbout(false);
      return;
    }
    goBackOneLevel();
    return;
  }
  // "?" key: toggle shortcuts popover
  if (!typing && !mod && (e.key === '?' || (e.key === '/' && e.shiftKey))) {
    e.preventDefault();
    toggleShortcuts();
    return;
  }
  // Welcome: Enter goes to picker (more predictable than "any key")
  if (view === 'welcome' && !typing && (e.key === 'Enter' || e.key === ' ')) {
    e.preventDefault();
    showView('picker');
    return;
  }
  // Picker: "/" focuses the preset search
  if (view === 'picker' && !typing && !mod && e.key === '/') {
    e.preventDefault();
    document.getElementById('preset-search').focus();
    return;
  }
  // Picker: arrows move through the preset cards (grid order)
  if (view === 'picker' && !typing && ['ArrowDown', 'ArrowUp', 'ArrowLeft', 'ArrowRight'].includes(e.key)) {
    e.preventDefault();
    const visible = Array.from(document.querySelectorAll('.preset-card'));
    if (visible.length === 0) return;
    const currentIdx = visible.findIndex(r => Number(r.dataset.id) === selectedPreset?.id);
    // Up/Down jump a whole row of the grid, Left/Right one card.
    const grid = document.getElementById('preset-list');
    const cols = getComputedStyle(grid).gridTemplateColumns.split(' ').length || 1;
    const step = { ArrowDown: cols, ArrowUp: -cols, ArrowRight: 1, ArrowLeft: -1 }[e.key];
    let next = currentIdx < 0 ? 0 : currentIdx + step;
    next = Math.max(0, Math.min(visible.length - 1, next));
    selectPreset(Number(visible[next].dataset.id));
    visible[next].focus({ preventScroll: true });
    visible[next].scrollIntoView({ block: 'nearest' });
    return;
  }
  // Picker: Enter triggers Start if available
  if (view === 'picker' && !typing && e.key === 'Enter') {
    const btn = document.getElementById('btn-start');
    if (!btn.classList.contains('hidden')) {
      e.preventDefault();
      btn.click();
    }
    return;
  }
  // Trim editor: Space toggles play/pause (only if trim editor visible and not typing)
  if (view === 'picker' && !typing && e.key === ' ') {
    const editor = document.getElementById('trim-editor');
    if (!editor.classList.contains('hidden')) {
      e.preventDefault();
      document.getElementById('trim-play-overlay').click();
    }
    return;
  }
});

// ============================================================
//  Conversion start
// ============================================================
document.getElementById('btn-start').onclick = async () => {
  if (stagedFiles.length === 0 || !selectedPreset) return;
  const btn = document.getElementById('btn-start');
  // Double-click guard — mashing the button would otherwise queue multiple
  // parallel convert() invokes that fight over `convertingMap` and emit
  // overlapping progress events.
  if (btn.disabled) return;
  btn.disabled = true;
  const options = collectParamValues();
  showView('converting');
  prepareConvertList();
  const outputDir = settings.convertDir?.trim() || null;
  try {
    await invoke('convert', {
      presetId: selectedPreset.id,
      files: stagedFiles.map(f => f.path),
      options,
      outputDir,
      overwrite: !!settings.overwrite,
    });
  } catch (err) {
    console.error('convert error:', err);
  } finally {
    btn.disabled = false;
  }
};

document.getElementById('btn-cancel').onclick = async () => {
  const btn = document.getElementById('btn-cancel');
  btn.disabled = true;
  btn.textContent = 'Canceling…';
  try {
    await invoke('cancel_conversion');
  } catch (e) {
    console.error('cancel failed:', e);
    btn.disabled = false;
    btn.textContent = 'Cancel';
  }
};

document.getElementById('btn-return').onclick = () => {
  document.getElementById('batch-summary').classList.add('hidden');
  stagedFiles = [];
  renderStagedFiles();
  showView('picker');     // back to preset picker, not welcome
  renderHistory();        // refresh history in case user visits welcome later
};

function prepareConvertList() {
  const list = document.getElementById('convert-list');
  list.innerHTML = '';
  convertingMap = {};
  stagedFiles.forEach((f, i) => {
    const row = document.createElement('div');
    row.className = 'convert-row';
    row.dataset.idx = i;
    // First row starts in indeterminate mode (ffprobe + ffmpeg startup take
    // ~1-2s before first stats line). Others wait their turn.
    const indeterminate = i === 0 ? ' indeterminate' : '';
    const initialStatus = i === 0 ? 'starting…' : 'waiting…';
    row.innerHTML = `
      <div class="cr-head">
        <span class="cr-name">${escapeHtml(f.name)}</span>
        <span class="cr-status">${initialStatus}</span>
      </div>
      <div class="progress-bar${indeterminate}"><div class="progress-fill"></div></div>
      <div class="cr-stats">
        <span>FPS: <span class="v cr-fps">–</span></span>
        <span>Speed: <span class="v cr-speed">–</span></span>
        <span>Frame: <span class="v cr-frame">–</span></span>
        <span>Bitrate: <span class="v cr-bitrate">–</span></span>
      </div>
    `;
    list.appendChild(row);
    convertingMap[i] = row;
  });
  document.getElementById('batch-summary').classList.add('hidden');
  const cancelBtn = document.getElementById('btn-cancel');
  cancelBtn.classList.remove('hidden');
  cancelBtn.disabled = false;
  cancelBtn.textContent = 'Cancel';
  document.getElementById('converting-title').textContent =
    `Converting ${stagedFiles.length} ${stagedFiles.length === 1 ? 'file' : 'files'} · preset ${selectedPreset.name}`;
}

// ============================================================
//  Progress events from Rust
// ============================================================
listen('conversion-progress', (event) => {
  const p = event.payload;
  const row = convertingMap[p.file_index];
  if (!row) return;
  // Race guard: if this file is already done/failed, drop any late progress event
  if (row.classList.contains('done') || row.classList.contains('fail')) return;
  row.classList.add('active');
  // Swap indeterminate animation for real progress on first event
  const bar = row.querySelector('.progress-bar');
  if (bar.classList.contains('indeterminate')) bar.classList.remove('indeterminate');
  row.querySelector('.cr-status').textContent = 'converting…';
  if (p.fps != null)     row.querySelector('.cr-fps').textContent = p.fps.toFixed(0);
  if (p.speed != null)   row.querySelector('.cr-speed').textContent = p.speed.toFixed(2) + 'x';
  if (p.frame != null)   row.querySelector('.cr-frame').textContent = p.frame;
  if (p.bitrate != null) row.querySelector('.cr-bitrate').textContent = p.bitrate;

  if (p.time_ms != null) {
    const total = p.total_ms || 30_000;
    const pct = Math.min(99, (p.time_ms / total) * 100);
    row.querySelector('.progress-fill').style.width = pct + '%';
  }
});

listen('file-done', (event) => {
  const p = event.payload;
  const row = convertingMap[p.file_index];
  if (!row) return;
  row.classList.remove('active');
  row.classList.add(p.ok ? 'done' : 'fail');
  const bar = row.querySelector('.progress-bar');
  bar.classList.remove('indeterminate');
  row.querySelector('.progress-fill').style.width = '100%';
  // When a file finishes, kick the NEXT one's row into indeterminate so the
  // user sees the new file is being processed even before its first event.
  const next = convertingMap[p.file_index + 1];
  if (next && !next.classList.contains('done') && !next.classList.contains('fail')) {
    next.querySelector('.progress-bar').classList.add('indeterminate');
    next.querySelector('.cr-status').textContent = 'starting…';
  }
  const status = row.querySelector('.cr-status');
  if (p.ok) {
    status.className = 'cr-status ok';
    status.textContent = `✓ ${p.out_file} · ${p.size_mb} MB · ${p.secs}s`;
    status.title = p.out_path || '';
    if (p.out_path) {
      row.dataset.out = p.out_path;
      const reveal = document.createElement('button');
      reveal.className = 'cr-reveal';
      reveal.textContent = 'Show in folder';
      reveal.onclick = () => invoke('reveal_in_explorer', { path: p.out_path })
        .catch((e) => showToast('Could not open folder', String(e), 'fail'));
      status.after(reveal);
    }
  } else {
    status.className = 'cr-status fail';
    status.textContent = `✗ Failed · ${p.error || ''}`;
  }
});

listen('batch-done', async (event) => {
  const p = event.payload;
  const total = p.ok_count + p.fail_count;
  document.getElementById('summary-ok').textContent  = p.ok_count + ' OK';
  document.getElementById('summary-fail').textContent = p.fail_count + ' FAIL';
  document.getElementById('batch-summary').classList.remove('hidden');
  const fileWord = total === 1 ? 'file' : 'files';
  document.getElementById('converting-title').textContent =
    p.fail_count === 0
      ? `Finished · ${p.ok_count} ${fileWord}`
      : `Finished with errors · ${p.ok_count} OK / ${p.fail_count} failed`;
  document.getElementById('btn-cancel').classList.add('hidden');
  // Any rows that never finished (cancel mid-batch, or never started) get
  // their indeterminate animation cleared and marked as cancelled so the UI
  // doesn't leave them in a perpetual "starting…" state.
  Object.values(convertingMap).forEach(row => {
    const bar = row.querySelector('.progress-bar');
    bar.classList.remove('indeterminate');
    if (!row.classList.contains('done') && !row.classList.contains('fail')) {
      row.classList.add('fail');
      const status = row.querySelector('.cr-status');
      status.className = 'cr-status fail';
      status.textContent = '✗ Cancelled';
    }
  });
  renderHistory();

  // Settings → "Show the result in Explorer when done"
  if (settings.openFolderAfter && p.ok_count > 0) {
    const first = Object.values(convertingMap).find((r) => r.dataset.out);
    if (first) invoke('reveal_in_explorer', { path: first.dataset.out }).catch(() => {});
  }

  // In-app toast (always shown — works even if user is in settings or elsewhere)
  showToast(
    p.fail_count === 0 ? 'Conversion complete' : 'Conversion finished with errors',
    p.fail_count === 0
      ? `${p.ok_count} ${fileWord} converted`
      : `${p.ok_count} OK · ${p.fail_count} failed`,
    p.fail_count === 0 ? 'ok' : 'fail',
  );

  // Desktop notification (only if window isn't focused — user is in another app)
  await maybeNotify(
    p.fail_count === 0 ? 'Nibble · Conversion finished' : 'Nibble · Finished with errors',
    p.fail_count === 0
      ? `${p.ok_count} ${fileWord} converted`
      : `${p.ok_count} OK · ${p.fail_count} failed`,
  );
});

async function maybeNotify(title, body) {
  try {
    if (settings.notif === false) return;
    const focused = await win.isFocused();
    if (focused) return;
    if (!notif.sendNotification) return;
    let granted = await notif.isPermissionGranted?.();
    if (!granted && notif.requestPermission) {
      const perm = await notif.requestPermission();
      granted = perm === 'granted';
    }
    if (granted) {
      notif.sendNotification({ title, body });
    }
  } catch (e) {
    console.debug('notification skipped:', e);
  }
}

// ============================================================
//  Clear history
// ============================================================
async function clearHistory() {
  if (settings.confirmClearHistory && !confirm('Clear Recent Activity? Your files stay where they are.')) return;
  await invoke('clear_history');
  await renderHistory();
}
document.getElementById('btn-clear-history').onclick = clearHistory;
document.getElementById('set-delete-mode').addEventListener('change', (e) => {
  settings.deleteMode = e.target.value;
  saveSettings();
  renderHistory();
});
document.getElementById('set-hist-days').addEventListener('change', (e) => {
  settings.histDays = Number(e.target.value) || 0;
  saveSettings();
  renderHistory();
});

// ============================================================
//  Download from URL (yt-dlp)
// ============================================================
let lastDownloadedPath = null;
// Tracks whether a download is currently running. We use it so that when the
// user navigates away (settings / welcome) and back into the Download view,
// the progress bar isn't wiped — the backend kept going and the user expects
// to see the live state, not a blank form.
let downloadInProgress = false;

// ============================================================
//  Quick share mode — the Download view, but the result goes to the clipboard
//  (temp file, auto-deleted) instead of the Downloads folder.
// ============================================================
let shareMode = false;
const SHARE_FIT_KEY = 'nibble.share.fit';
// Auto-delete delay for the shared temp file (Settings → Quick share).
const shareTtlSeconds = () => Math.max(1, Number(settings.shareTtlMin) || 10) * 60;
let shareTimer = null;
let sharePath = null;

function setShareMode(on) {
  shareMode = on;
  document.getElementById('view-download').classList.toggle('share-mode', on);
  document.querySelectorAll('#link-mode button').forEach((b) => {
    const active = (b.dataset.mode === 'share') === on;
    b.classList.toggle('active', active);
    b.setAttribute('aria-selected', String(active));
  });
  document.getElementById('share-note').classList.toggle('hidden', !on);
  document.getElementById('dl-fit-row').classList.toggle('hidden', !on);
  document.getElementById('dl-savedir-row').classList.toggle('hidden', on);
  document.getElementById('dl-toggles-row').classList.toggle('hidden', on);
  document.getElementById('btn-download').innerHTML = on
    ? 'Copy to clipboard<svg class="ic"><use href="#i-clipboard"/></svg>'
    : 'Download<svg class="ic"><use href="#i-arrow-right"/></svg>';
}

function fmtCountdown(s) {
  const m = Math.floor(s / 60), r = s % 60;
  return `${m}:${String(r).padStart(2, '0')}`;
}

async function deleteSharedFile(reason) {
  clearInterval(shareTimer);
  shareTimer = null;
  sharePath = null;
  try { await invoke('share_clear'); } catch (e) { console.debug('share_clear failed:', e); }
  document.getElementById('download-shared').classList.add('hidden');
  if (reason) showToast('Shared clip deleted', reason, 'ok', 3500);
}

function startShareCountdown(path) {
  sharePath = path;
  clearInterval(shareTimer);
  const deadline = Date.now() + shareTtlSeconds() * 1000;
  const el = document.getElementById('share-timer');
  const tick = () => {
    const left = Math.max(0, Math.round((deadline - Date.now()) / 1000));
    el.textContent = `Temporary file deletes itself in ${fmtCountdown(left)}`;
    if (left <= 0) deleteSharedFile('The temporary file was removed automatically.');
  };
  tick();
  shareTimer = setInterval(tick, 1000);
}

document.getElementById('btn-share-copy').onclick = async () => {
  if (!sharePath) return;
  try {
    await invoke('share_copy', { path: sharePath });
    showToast('Copied again', 'Paste it with Ctrl+V', 'ok', 2500);
  } catch (e) {
    showToast('Copy failed', String(e), 'fail');
  }
};
document.getElementById('btn-share-delete').onclick = () => deleteSharedFile('Removed from disk and clipboard.');

const fitSelect = document.getElementById('download-fit');
try { const v = localStorage.getItem(SHARE_FIT_KEY); if (v !== null) fitSelect.value = v; } catch (e) { /* noop */ }
fitSelect.addEventListener('change', () => {
  try { localStorage.setItem(SHARE_FIT_KEY, fitSelect.value); } catch (e) { /* noop */ }
});

// "Video from a link": one page, two modes (Quick share → clipboard /
// Download → folder). The last used mode is remembered.
const LINK_MODE_KEY = 'nibble.link.mode';
function linkMode() {
  try { return localStorage.getItem(LINK_MODE_KEY) === 'download' ? 'download' : 'share'; } catch (e) { return 'share'; }
}
function openLinkView(mode = linkMode()) {
  setShareMode(mode === 'share');
  showView('download');
  // Only wipe the panel if there's nothing live to display.
  if (!downloadInProgress) resetDownloadView();
  document.getElementById('download-url').focus();
}
document.getElementById('btn-go-link').onclick = () => openLinkView();
document.querySelectorAll('#link-mode button').forEach((b) => {
  b.onclick = () => {
    if (downloadInProgress) return;   // don't switch under a running job
    try { localStorage.setItem(LINK_MODE_KEY, b.dataset.mode); } catch (e) { /* noop */ }
    setShareMode(b.dataset.mode === 'share');
    resetDownloadView();
  };
});
document.getElementById('btn-back-from-download').onclick = () => {
  showView('welcome');
};

function resetDownloadView() {
  document.getElementById('download-progress').classList.add('hidden');
  document.getElementById('download-done').classList.add('hidden');
  document.getElementById('download-failed').classList.add('hidden');
  // The share panel hides, but its delete countdown keeps running in the
  // background so the temp file still disappears on schedule.
  document.getElementById('download-shared').classList.add('hidden');
  // Stays off until yt-dlp exists (the warning offers a one-click install).
  document.getElementById('btn-download').disabled = !ytdlpStatus.available;
}

let ytdlpStatus = { available: false, version: null, managed: false };
let ytdlpLatestChecked = false;

// Compare two date-based yt-dlp versions ("2026.08.19", "2026.08.19.123456").
// Returns -1 if a<b, 0 if equal, 1 if a>b.
function cmpYtdlpVer(a, b) {
  const pa = String(a).split(/[.\-]/).map(n => parseInt(n, 10) || 0);
  const pb = String(b).split(/[.\-]/).map(n => parseInt(n, 10) || 0);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const x = pa[i] || 0, y = pb[i] || 0;
    if (x < y) return -1;
    if (x > y) return 1;
  }
  return 0;
}

// yt-dlp breaks whenever a site changes its internals; these error signatures
// almost always mean "your yt-dlp is too old" — so we nudge the user to update.
function looksLikeOutdatedYtdlp(errStr) {
  return /nsig|sign in to confirm|unable to extract|player response|HTTP Error 403|precondition check failed|update to the latest|requested format is not available|not available on this|Postprocessing/i
    .test(String(errStr));
}

function flagYtdlpMaybeOutdated() {
  if (!ytdlpStatus.available) return;
  const row = document.getElementById('dl-ytdlp');
  const badge = document.getElementById('dl-ytdlp-badge');
  row.classList.remove('hidden');
  badge.textContent = 'may be outdated — click Update';
  badge.classList.remove('hidden');
}

async function refreshYtdlpStatus() {
  try {
    const s = await invoke('ytdlp_status');
    ytdlpStatus = s;
    const warn = document.getElementById('dl-warning');
    const info = document.getElementById('dl-info');
    const btn  = document.getElementById('btn-download');
    const row  = document.getElementById('dl-ytdlp');
    if (s.available) {
      warn.classList.add('hidden');
      info.classList.remove('hidden');
      btn.disabled = false;
      info.innerHTML =
        `Supports YouTube, Twitter/X, Reddit, TikTok, Vimeo, and 1000+ other sites. ` +
        `<span class="dl-info-save">Files save to your Downloads folder.</span>`;
      document.getElementById('dl-ytdlp-ver').textContent = `yt-dlp v${s.version || '?'}`;
      row.classList.remove('hidden');
      // non-blocking: flags if a newer release exists (Settings can turn it off)
      if (settings.ytdlpAutoCheck !== false) checkYtdlpLatest();
    } else {
      warn.classList.remove('hidden');
      info.classList.add('hidden');
      row.classList.add('hidden');
      btn.disabled = true;
    }
  } catch (e) {
    console.error('ytdlp_status failed:', e);
  }
}

// Ask GitHub for the latest release and show a badge if the local copy is older.
async function checkYtdlpLatest(force) {
  if (ytdlpLatestChecked && !force) return;
  ytdlpLatestChecked = true;
  const badge = document.getElementById('dl-ytdlp-badge');
  try {
    const latest = await invoke('ytdlp_latest');
    if (ytdlpStatus.version && cmpYtdlpVer(ytdlpStatus.version, latest) < 0) {
      badge.textContent = `update → ${latest}`;
      badge.classList.remove('hidden');
    } else {
      badge.classList.add('hidden');
    }
  } catch (e) {
    console.debug('yt-dlp latest check failed:', e);
  }
}

async function doYtdlpUpdate() {
  const btn = document.getElementById('btn-ytdlp-update');
  const statusEl = document.getElementById('dl-ytdlp-status');
  if (btn.disabled) return;
  btn.disabled = true;
  statusEl.textContent = 'Updating…';
  try {
    const newVer = await invoke('ytdlp_update');
    statusEl.textContent = '';
    showToast('yt-dlp updated', `Now on v${newVer}`, 'ok');
    ytdlpLatestChecked = false;
    await refreshYtdlpStatus();
  } catch (err) {
    statusEl.textContent = '';
    showToast('yt-dlp update failed', String(err), 'fail');
  } finally {
    btn.disabled = false;
  }
}
document.getElementById('btn-ytdlp-update').onclick = doYtdlpUpdate;

// yt-dlp missing (e.g. the portable exe on a fresh PC): one-click install of
// just yt-dlp into the app's own bin folder (ytdlp_update handles "missing").
document.getElementById('btn-ytdlp-install').onclick = async () => {
  const btn = document.getElementById('btn-ytdlp-install');
  const st = document.getElementById('ytdlp-install-status');
  if (btn.disabled) return;
  btn.disabled = true;
  st.textContent = 'Downloading yt-dlp…';
  try {
    const ver = await invoke('ytdlp_update');
    st.textContent = '';
    showToast('yt-dlp installed', `v${ver} — you can download videos now`, 'ok');
    await refreshYtdlpStatus();
    // Re-read the pasted URL now that yt-dlp exists
    const url = document.getElementById('download-url').value.trim();
    if (/^https?:\/\//i.test(url)) { lastUrlFetched = null; dlTrimLoadedFor = null; fetchUrlInfo(url); }
  } catch (err) {
    st.textContent = '';
    showToast('yt-dlp install failed', String(err), 'fail');
  } finally {
    btn.disabled = false;
  }
};

// ============================================================
//  Save-to directory + cookies browser (both persisted)
// ============================================================
const SAVE_DIR_KEY = 'nibble.download.saveDir';
const COOKIES_BROWSER_KEY = 'nibble.download.cookiesBrowser';
const savedirInput = document.getElementById('download-savedir');
const browserSelect = document.getElementById('download-browser');
savedirInput.value = localStorage.getItem(SAVE_DIR_KEY) || '';
// Default quality (Settings → Downloads); the last pick becomes the default.
{
  const q = document.getElementById('download-quality');
  if ([...q.options].some((o) => o.value === settings.dlQuality)) q.value = settings.dlQuality;
  q.addEventListener('change', () => { settings.dlQuality = q.value; saveSettings(); });
}
// "Auto" is the default: cookies are only used when a video actually needs
// them (logged-in YouTube sessions break public videos). One-time switch for
// people who had picked a browser before Auto existed.
const COOKIES_AUTO_MIGRATED = 'nibble.cookies.autoMigrated';
try {
  if (!localStorage.getItem(COOKIES_AUTO_MIGRATED)) {
    localStorage.setItem(COOKIES_BROWSER_KEY, 'auto');
    localStorage.setItem(COOKIES_AUTO_MIGRATED, '1');
  }
} catch (e) { /* noop */ }
browserSelect.value = localStorage.getItem(COOKIES_BROWSER_KEY) ?? 'auto';
if (!browserSelect.value) browserSelect.value = localStorage.getItem(COOKIES_BROWSER_KEY) === '' ? '' : 'auto';
// Cookies source that actually worked for the current URL (from ytdlp_info).
let urlCookies = { url: null, value: null };
function cookiesFor(url) {
  return urlCookies.url === url ? urlCookies.value : (browserSelect.value || null);
}

savedirInput.addEventListener('change', () => {
  localStorage.setItem(SAVE_DIR_KEY, savedirInput.value);
});
browserSelect.addEventListener('change', () => {
  localStorage.setItem(COOKIES_BROWSER_KEY, browserSelect.value);
  // If a URL is already entered, re-fetch its info with the new cookies setting
  const url = document.getElementById('download-url').value.trim();
  if (url && /^https?:\/\//i.test(url)) {
    lastUrlFetched = null;
    dlTrimLoadedFor = null;   // also re-resolve the preview with new cookies
    fetchUrlInfo(url);
  }
});
document.getElementById('btn-pick-savedir').onclick = async () => {
  try {
    const folder = await openDialog({
      directory: true,
      multiple: false,
      defaultPath: savedirInput.value || undefined,
    });
    if (folder) {
      savedirInput.value = folder;
      localStorage.setItem(SAVE_DIR_KEY, folder);
    }
  } catch (e) {
    console.error('folder pick failed:', e);
  }
};

// ============================================================
//  URL info preview (thumbnail + title + duration)
// ============================================================
let urlDebounceTimer = null;
let lastUrlFetched = null;

function fmtDuration(secs) {
  if (!Number.isFinite(secs) || secs <= 0) return '';
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = Math.floor(secs % 60);
  const pad = (n) => String(n).padStart(2, '0');
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

document.getElementById('download-url').addEventListener('input', (e) => {
  const url = e.target.value.trim();
  clearTimeout(urlDebounceTimer);
  const preview = document.getElementById('dl-preview');
  if (!url || !/^https?:\/\//i.test(url)) {
    preview.classList.add('hidden');
    lastUrlFetched = null;
    resetDlTrim();
    return;
  }
  urlDebounceTimer = setTimeout(() => fetchUrlInfo(url), 700);
});

// Enter in the URL field starts the download (matches the trim time inputs,
// which already submit on Enter via their change handlers).
document.getElementById('download-url').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') {
    e.preventDefault();
    const btn = document.getElementById('btn-download');
    if (!btn.disabled) btn.click();
  }
});

async function fetchUrlInfo(url) {
  if (lastUrlFetched === url) return;
  lastUrlFetched = url;

  const preview = document.getElementById('dl-preview');
  const titleEl = document.getElementById('dl-prev-title');
  const infoEl  = document.getElementById('dl-prev-info');
  const thumbEl = document.getElementById('dl-thumb');

  preview.classList.remove('hidden');
  titleEl.textContent = 'Fetching info…';
  infoEl.textContent = '';
  thumbEl.removeAttribute('src');

  try {
    const cookiesBrowser = browserSelect.value || null;
    const info = await invoke('ytdlp_info', { url, cookiesBrowser });
    // Race guard: if user kept typing, ignore this stale response
    if (lastUrlFetched !== url) return;
    urlCookies = { url, value: info.cookies_used ?? null };

    titleEl.textContent = info.title || '(no title)';
    const parts = [];
    if (info.site)     parts.push(info.site);
    if (info.uploader) parts.push(info.uploader);
    if (info.duration_s) parts.push(fmtDuration(info.duration_s));
    if (info.cookies_used) {
      parts.push(`cookies: ${info.cookies_used}`);
    } else if (cookiesBrowser && cookiesBrowser !== 'auto') {
      parts.push('no cookies needed');
    }
    infoEl.textContent = parts.join('  ·  ');

    if (info.thumbnail) {
      thumbEl.src = info.thumbnail;
    }

    // Resolve a live preview + duration for the trim editor (separate yt-dlp
    // call so the info above shows instantly without waiting on stream resolve).
    loadDlPreview(url);
  } catch (err) {
    if (lastUrlFetched !== url) return;
    titleEl.textContent = 'Could not fetch info';
    const errStr = String(err);
    // Hint at the cookies-browser fix when yt-dlp reports an auth requirement.
    // errStr is user-controlled (yt-dlp output), so escape it before inserting
    // into innerHTML — otherwise a crafted error message could inject HTML.
    if (/authentication is required|account credentials|sign in|login required|cookies/i.test(errStr)) {
      infoEl.innerHTML =
        `<strong>Auth required.</strong> Pick a browser in <em>Use cookies from</em> ` +
        `(make sure you're logged in there) and yt-dlp will use that session.<br>` +
        `<span class="muted">${escapeHtml(errStr)}</span>`;
    } else if (looksLikeOutdatedYtdlp(errStr)) {
      flagYtdlpMaybeOutdated();
      infoEl.innerHTML =
        `<strong>Couldn't read this URL.</strong> yt-dlp is often out of date when this ` +
        `happens — click <em>Update yt-dlp</em> below and try again.<br>` +
        `<span class="muted">${escapeHtml(errStr)}</span>`;
    } else {
      infoEl.textContent = errStr;
    }
  }
}

// ============================================================
//  Download trim editor — live preview + dual-handle slider.
//  Self-contained (own state + IDs) so it never collides with the local-file
//  trim editor above. Lets the user download only a section of a URL video.
//  Default range = full clip → downloads everything unless a handle is moved.
// ============================================================
let dlTrimDurationMs = 0;
let dlTrimStartMs = 0;
let dlTrimEndMs = 0;
let dlTrimLoadedFor = null;   // url we already resolved a preview for
let dlAudioSynced = false;    // preview audio comes from the separate <audio>

function clearDlAudio() {
  dlAudioSynced = false;
  const a = document.getElementById('dl-trim-audio');
  a.pause?.();
  a.removeAttribute('src');
  a.load?.();
}

function dlSetTrimRange(startMs, endMs, durationMs) {
  dlTrimDurationMs = durationMs;
  dlTrimStartMs = Math.max(0, Math.min(startMs, durationMs));
  dlTrimEndMs   = Math.max(dlTrimStartMs, Math.min(endMs, durationMs));
  const pctStart = durationMs ? (dlTrimStartMs / durationMs) : 0;
  const pctEnd   = durationMs ? (dlTrimEndMs   / durationMs) : 1;
  document.getElementById('dl-trim-handle-start').style.left = `${pctStart * 100}%`;
  document.getElementById('dl-trim-handle-end').style.left   = `${pctEnd * 100}%`;
  const range = document.getElementById('dl-trim-range');
  range.style.left  = `${pctStart * 100}%`;
  range.style.width = `${(pctEnd - pctStart) * 100}%`;
  const inS = document.getElementById('dl-trim-input-start');
  const inE = document.getElementById('dl-trim-input-end');
  if (document.activeElement !== inS) inS.value = fmtTime(dlTrimStartMs);
  if (document.activeElement !== inE) inE.value = fmtTime(dlTrimEndMs);
  document.getElementById('dl-trim-duration').textContent =
    `selection: ${fmtTime(dlTrimEndMs - dlTrimStartMs)}  /  total: ${fmtTime(dlTrimDurationMs)}`;
}

// True only when the user has narrowed the range to a real subset of the clip
// (a small epsilon avoids treating full-range rounding as a trim).
function dlHasTrim() {
  if (document.getElementById('dl-trim').classList.contains('hidden')) return false;
  if (!(dlTrimDurationMs > 0)) return false;
  return dlTrimStartMs > 50 || dlTrimEndMs < dlTrimDurationMs - 50;
}

function resetDlTrim() {
  dlTrimLoadedFor = null;
  dlTrimDurationMs = 0; dlTrimStartMs = 0; dlTrimEndMs = 0;
  const wrap = document.getElementById('dl-trim-video-wrap');
  const v = document.getElementById('dl-trim-video');
  v.pause?.();
  v.removeAttribute('src');
  v.load?.();
  clearDlAudio();
  wrap.classList.remove('error', 'playing', 'loading');
  document.getElementById('dl-trim').classList.add('hidden');
}

async function loadDlPreview(url) {
  if (dlTrimLoadedFor === url) return;
  dlTrimLoadedFor = url;

  const panel = document.getElementById('dl-trim');
  const wrap  = document.getElementById('dl-trim-video-wrap');
  const v     = document.getElementById('dl-trim-video');
  panel.classList.remove('hidden');
  wrap.classList.add('loading');
  wrap.classList.remove('error', 'playing');
  v.removeAttribute('src');
  v.load?.();
  clearDlAudio();
  dlSetTrimRange(0, 0, 0);
  document.getElementById('dl-trim-playhead').classList.remove('visible');

  try {
    const prev = await invoke('ytdlp_preview', { url, cookiesBrowser: cookiesFor(url) });
    // Race guard: user changed the URL while we were resolving.
    if (dlTrimLoadedFor !== url) return;

    if (prev.duration_s && prev.duration_s > 0) {
      const durMs = prev.duration_s * 1000;
      dlSetTrimRange(0, durMs, durMs);
    }
    if (prev.stream_id) {
      // Streams go through the backend's `nstream` proxy (chunked ranges, no
      // Referer) — see serve_preview_stream in lib.rs.
      const src = (id) => window.__TAURI__.core.convertFileSrc(id, 'nstream');
      // Video-only stream (YouTube): its audio plays from a synced <audio>.
      dlAudioSynced = !!prev.audio_id;
      v.muted = dlAudioSynced;
      if (dlAudioSynced) document.getElementById('dl-trim-audio').src = src(prev.audio_id);
      v.src = src(prev.stream_id);
      v.currentTime = 0;
      v.pause();
    } else {
      // No directly-playable stream → keep the slider usable via manual input.
      wrap.classList.add('error');
    }
  } catch (err) {
    if (dlTrimLoadedFor !== url) return;
    console.warn('ytdlp_preview failed:', err);
    wrap.classList.add('error');
  } finally {
    if (dlTrimLoadedFor === url) wrap.classList.remove('loading');
  }
}

function setupDlTrimSlider() {
  const slider = document.getElementById('dl-trim-slider');
  const hStart = document.getElementById('dl-trim-handle-start');
  const hEnd   = document.getElementById('dl-trim-handle-end');
  const video  = document.getElementById('dl-trim-video');
  const wrap   = document.getElementById('dl-trim-video-wrap');
  const overlay = document.getElementById('dl-trim-play-overlay');

  // Coalesced seeking: dragging fires dozens of positions per second, but
  // every seek is a network fetch. Keep at most ONE seek in flight; when it
  // lands, jump straight to the latest requested position.
  let pendingSeek = null;
  function seekTo(t) {
    if (video.readyState < 1) return;
    if (video.seeking) { pendingSeek = t; return; }
    pendingSeek = null;
    video.currentTime = t;
  }
  video.addEventListener('seeked', () => {
    if (pendingSeek != null) { const t = pendingSeek; pendingSeek = null; video.currentTime = t; }
  });

  function startDrag(handle) {
    return (ev) => {
      ev.preventDefault();
      const wasPlaying = !video.paused;
      video.pause();
      const rect = slider.getBoundingClientRect();
      function onMove(e) {
        const x = (e.clientX ?? e.touches?.[0]?.clientX) - rect.left;
        const pct = Math.max(0, Math.min(1, x / rect.width));
        const ms = pct * dlTrimDurationMs;
        if (handle === 'start') {
          dlSetTrimRange(Math.min(ms, dlTrimEndMs - 100), dlTrimEndMs, dlTrimDurationMs);
          seekTo(dlTrimStartMs / 1000);
        } else {
          dlSetTrimRange(dlTrimStartMs, Math.max(ms, dlTrimStartMs + 100), dlTrimDurationMs);
          seekTo(Math.max(0, (dlTrimEndMs - 500)) / 1000);
        }
        drawPlayhead();
      }
      function onUp() {
        window.removeEventListener('pointermove', onMove);
        window.removeEventListener('pointerup', onUp);
        // It was playing → keep playing from the (new) start of the selection
        if (wasPlaying) {
          seekTo(dlTrimStartMs / 1000);
          video.play().catch(() => {});
        }
      }
      window.addEventListener('pointermove', onMove);
      window.addEventListener('pointerup', onUp);
    };
  }
  hStart.addEventListener('pointerdown', startDrag('start'));
  hEnd.addEventListener('pointerdown', startDrag('end'));

  document.getElementById('dl-trim-input-start').addEventListener('change', (e) => {
    const ms = parseTime(e.target.value);
    if (!Number.isFinite(ms)) return;
    dlSetTrimRange(ms, dlTrimEndMs, dlTrimDurationMs);
    seekTo(dlTrimStartMs / 1000);
  });
  document.getElementById('dl-trim-input-end').addEventListener('change', (e) => {
    const ms = parseTime(e.target.value);
    if (!Number.isFinite(ms)) return;
    dlSetTrimRange(dlTrimStartMs, ms, dlTrimDurationMs);
  });

  slider.addEventListener('click', (e) => {
    if (e.target.classList.contains('trim-handle')) return;
    const rect = slider.getBoundingClientRect();
    const pct = (e.clientX - rect.left) / rect.width;
    const ms = pct * dlTrimDurationMs;
    seekTo(ms / 1000);
  });

  window.addEventListener('resize', () => {
    if (dlTrimDurationMs > 0) dlSetTrimRange(dlTrimStartMs, dlTrimEndMs, dlTrimDurationMs);
    if (video.paused) drawPlayhead();
  });

  overlay.addEventListener('click', () => {
    if (video.paused) {
      const tMs = video.currentTime * 1000;
      if (tMs < dlTrimStartMs - 50 || tMs > dlTrimEndMs) {
        video.currentTime = dlTrimStartMs / 1000;
      }
      video.play().catch(err => console.error('play failed:', err));
    } else {
      video.pause();
    }
  });
  // ---- Separate audio track (video-only streams): follow the video ----
  const audio = document.getElementById('dl-trim-audio');
  // The audio element only follows while playing — scrubbing a paused video
  // must not drag a second stream along (that doubled every seek's traffic).
  function syncAudio(play) {
    if (!dlAudioSynced) return;
    if (!play) { audio.pause(); return; }
    if (Math.abs(audio.currentTime - video.currentTime) > 0.05) audio.currentTime = video.currentTime;
    audio.play().catch(() => {});
  }
  video.addEventListener('playing', () => syncAudio(true));
  video.addEventListener('waiting', () => syncAudio(false));   // buffering
  video.addEventListener('seeked',  () => { if (!video.paused) syncAudio(true); });

  // ---- Playhead: per-frame while playing (timeupdate only fires ~4×/s) ----
  const playhead = document.getElementById('dl-trim-playhead');
  let rafId = 0;
  function drawPlayhead() {
    const tMs = video.currentTime * 1000;
    if (!video.paused && tMs > dlTrimEndMs) {
      video.currentTime = dlTrimStartMs / 1000;   // loop the selection
    }
    if (dlTrimDurationMs > 0) {
      const pct = Math.max(0, Math.min(1, tMs / dlTrimDurationMs));
      playhead.style.transform = `translateX(${pct * slider.clientWidth}px)`;
    }
    // Keep the separate audio from drifting
    if (dlAudioSynced && !video.paused && !audio.paused && !audio.seeking &&
        Math.abs(audio.currentTime - video.currentTime) > 0.1) {
      audio.currentTime = video.currentTime;
    }
    rafId = video.paused ? 0 : requestAnimationFrame(drawPlayhead);
  }

  video.addEventListener('play', () => {
    wrap.classList.add('playing');
    if (!rafId) rafId = requestAnimationFrame(drawPlayhead);
  });
  video.addEventListener('pause', () => {
    wrap.classList.remove('playing');
    syncAudio(false);
    drawPlayhead();
  });
  video.addEventListener('loadedmetadata', () => {
    // Prefer the real media duration over yt-dlp's (rounded) seconds.
    if (Number.isFinite(video.duration) && video.duration > 0) {
      const durMs = video.duration * 1000;
      // Only re-fit the range if it was still at the (full) default.
      const wasFull = dlTrimStartMs <= 50 && Math.abs(dlTrimEndMs - dlTrimDurationMs) <= 50;
      dlTrimDurationMs = durMs;
      if (wasFull) dlSetTrimRange(0, durMs, durMs);
      else dlSetTrimRange(dlTrimStartMs, dlTrimEndMs, durMs);
    }
    document.getElementById('dl-trim-playhead').classList.add('visible');
  });
  video.addEventListener('error', () => {
    if (video.getAttribute('src')) wrap.classList.add('error');
  });
  // Seeks while paused (handle drags, clicks on the slider)
  video.addEventListener('timeupdate', () => { if (video.paused) drawPlayhead(); });
}
setupDlTrimSlider();

document.getElementById('btn-download').onclick = async () => {
  const url = document.getElementById('download-url').value.trim();
  if (!url) {
    showToast('No URL', 'Paste a video URL first.', 'fail');
    return;
  }
  const quality = document.getElementById('download-quality').value;

  // Reset + show progress
  resetDownloadView();
  downloadInProgress = true;
  const dlProgress = document.getElementById('download-progress');
  dlProgress.classList.remove('hidden');
  document.getElementById('btn-download').disabled = true;
  document.getElementById('dl-progress-fill').style.width = '0%';
  // Start in indeterminate "warming up" mode — yt-dlp can take 1-30s to
  // connect, extract metadata, and emit the first [download] line.
  dlProgress.querySelector('.progress-bar').classList.add('indeterminate');
  document.querySelector('.dl-name').textContent = 'Connecting…';
  ['dl-percent', 'dl-speed', 'dl-eta', 'dl-size'].forEach(id => {
    document.getElementById(id).textContent = '–';
  });
  // Auto-scroll so the user sees the progress bar without manual scrolling.
  // requestAnimationFrame waits for layout reflow (the panel just un-hid),
  // otherwise scrollIntoView measures the old layout and the bar lands offscreen.
  requestAnimationFrame(() => {
    dlProgress.scrollIntoView({ behavior: 'smooth', block: 'center' });
  });

  const outputDir = shareMode ? null : (savedirInput.value.trim() || null);
  const cookiesBrowser = cookiesFor(url);
  const playlist = document.getElementById('download-playlist').checked;
  const writeSubs = document.getElementById('download-subs').checked;
  // Trim range → seconds, only when the user actually narrowed the selection.
  const startS = dlHasTrim() ? dlTrimStartMs / 1000 : null;
  const endS   = dlHasTrim() ? dlTrimEndMs   / 1000 : null;
  try {
    const share = shareMode;
    if (share) {
      // The backend wipes the previous share before starting; drop its timer so
      // it can't later delete THIS share's file.
      clearInterval(shareTimer);
      shareTimer = null;
      sharePath = null;
    }
    const fitMb = shareMode && quality !== 'audio' ? (Number(fitSelect.value) || null) : null;
    await invoke('download_url', {
      url, quality, outputDir, cookiesBrowser,
      playlist: share ? false : playlist,
      writeSubs: share ? false : writeSubs,
      startS, endS, share, fitMb,
    });
  } catch (err) {
    // Backend already emitted download-done with the failure details,
    // so we let the listener show the UI. Only log here.
    console.error('download_url error:', err);
  }
};

listen('download-progress', (event) => {
  const p = event.payload;
  // First real progress event → switch from indeterminate to determinate
  const dlBar = document.getElementById('download-progress')
    .querySelector('.progress-bar');
  if (dlBar.classList.contains('indeterminate')) {
    dlBar.classList.remove('indeterminate');
    const name = document.querySelector('.dl-name');
    // Keep a stage label like "Downloading only the selected part…"
    if (name.textContent === 'Connecting…' || name.textContent.startsWith('Preparing')) name.textContent = 'Downloading…';
  }
  if (p.percent != null) {
    document.getElementById('dl-progress-fill').style.width = p.percent + '%';
    document.getElementById('dl-percent').textContent = p.percent.toFixed(1) + '%';
  }
  if (p.speed != null) document.getElementById('dl-speed').textContent = p.speed;
  if (p.eta   != null) document.getElementById('dl-eta').textContent   = p.eta;
  if (p.size  != null) document.getElementById('dl-size').textContent  = p.size;
});

// Post-download phases (e.g. cutting the trimmed clip) — show the stage name
// and switch the bar back to "working" until download-done arrives.
listen('download-stage', (event) => {
  const bar = document.getElementById('download-progress').querySelector('.progress-bar');
  bar.classList.add('indeterminate');
  document.querySelector('.dl-name').textContent = String(event.payload ?? 'Processing…');
  ['dl-speed', 'dl-eta'].forEach((id) => { document.getElementById(id).textContent = '–'; });
});

document.getElementById('btn-dl-cancel').onclick = async () => {
  const btn = document.getElementById('btn-dl-cancel');
  btn.disabled = true;
  document.querySelector('.dl-name').textContent = 'Cancelling…';
  try { await invoke('cancel_download'); } catch (e) { console.debug('cancel_download failed:', e); }
};

listen('download-done', (event) => {
  const p = event.payload;
  downloadInProgress = false;
  document.getElementById('download-progress').classList.add('hidden');
  document.getElementById('btn-download').disabled = false;
  document.getElementById('btn-dl-cancel').disabled = false;
  if (!p.ok && p.error === 'Cancelled') {
    showToast('Download cancelled', 'Nothing was kept.', 'ok', 2500);
    return;
  }
  if (p.ok && p.shared) {
    // Quick share: file is on the clipboard; show the countdown panel.
    document.getElementById('share-file').textContent =
      `${p.file_name || p.file_path || 'clip'}${p.size_mb ? ` · ${p.size_mb} MB` : ''}`;
    document.getElementById('download-shared').classList.remove('hidden');
    startShareCountdown(p.file_path);
    showToast('Copied to clipboard', 'Paste it with Ctrl+V — e.g. into Discord', 'ok');
    renderHistory();
  } else if (p.ok) {
    lastDownloadedPath = p.file_path;
    document.getElementById('dl-output-name').textContent = p.file_name || p.file_path || '(unknown)';
    document.getElementById('download-done').classList.remove('hidden');
    showToast('Download complete', p.file_name || p.file_path || '', 'ok');
  } else {
    const err = p.error || 'unknown error';
    document.getElementById('dl-error').textContent = err;
    document.getElementById('download-failed').classList.remove('hidden');
    showToast('Download failed', err, 'fail');
    // Most "download failed" cases on big sites are a stale yt-dlp — surface the
    // one-click update right next to the error.
    if (looksLikeOutdatedYtdlp(err)) flagYtdlpMaybeOutdated();
  }
});

document.getElementById('btn-dl-folder').onclick = async () => {
  if (!lastDownloadedPath) return;
  try {
    await invoke('reveal_in_explorer', { path: lastDownloadedPath });
  } catch (e) {
    console.error('reveal failed:', e);
    showToast('Could not open folder', String(e), 'fail');
  }
};
document.getElementById('btn-dl-convert').onclick = () => {
  if (!lastDownloadedPath) return;
  // Stage the downloaded file and go to picker
  stagedFiles = [{
    path: lastDownloadedPath,
    name: lastDownloadedPath.split(/[\\/]/).pop(),
  }];
  renderStagedFiles();
  showView('picker');
};
document.getElementById('btn-dl-retry').onclick = () => {
  resetDownloadView();
};

// ============================================================
//  Init
// ============================================================
// Welcome hardware line: "NVIDIA GeForce RTX 5080 · NVENC AV1". Falls back to
// the CPU when no GPU was detected. Full CPU/RAM details go in the tooltip.
async function applyHardwareToWelcome() {
  const el = document.getElementById('welcome-hw-text');
  const wrap = document.getElementById('welcome-hw');
  if (!el) return;
  if (!hardwareInfo) { el.textContent = 'Hardware detection unavailable'; return; }
  let line = '';
  if (hardwareInfo.gpu_name) {
    line = hardwareInfo.gpu_name;
    if (hardwareInfo.nvenc_av1) line += ' · NVENC AV1';
    else if (hardwareInfo.nvenc_available) line += ' · NVENC';
  } else if (hardwareInfo.cpu_name) {
    line = `${hardwareInfo.cpu_name.replace(/\s+/g, ' ')} · CPU encoding`;
  }
  el.textContent = line || 'Hardware detection unavailable';
  // Under the system line: the signed-in Windows user ("ZIP BURTOWSKI")
  document.getElementById('welcome-user').textContent = hardwareInfo.user_name || '';
  const tip = [];
  if (hardwareInfo.cpu_name)  tip.push(hardwareInfo.cpu_name.replace(/\s+/g, ' '));
  if (hardwareInfo.cpu_cores) tip.push(`${hardwareInfo.cpu_cores} cores`);
  if (hardwareInfo.ram_gb)    tip.push(`${hardwareInfo.ram_gb} GB RAM`);
  if (hardwareInfo.os_name)   tip.push(hardwareInfo.os_name);
  wrap.title = tip.join(' · ');
}

// Portable build? (exe named *portable* → data lives next to the exe)
let appInfo = null;
invoke('app_info')
  .then((info) => {
    appInfo = info;
    if (info.portable) {
      document.getElementById('about-portable')?.classList.remove('hidden');
    }
  })
  .catch(() => {});

(async function init() {
  // (settings were loaded at the top of the file) Keep the backend's
  // window-geometry flag in step with the setting (e.g. after a reset).
  invoke('set_remember_window', { enabled: !!settings.rememberWindow }).catch(() => {});
  // Everything starts in parallel: hardware detection spawns PowerShell /
  // nvidia-smi (~0.5–1 s), so history + presets must not wait for it.
  const hw = invoke('detect_hardware')
    .then((info) => { hardwareInfo = info; })
    .catch((e) => console.debug('hardware detect failed:', e))
    .then(applyHardwareToWelcome);
  await Promise.all([
    hw,
    renderHistory(),
    renderPresets(),
    refreshYtdlpStatus(),
    checkFfmpeg(),
  ]);
})();

// ffmpeg is required for everything. If it's missing, show a persistent banner
// on the welcome screen with install guidance (dismissible for the session).
async function checkFfmpeg() {
  const banner = document.getElementById('dep-banner');
  if (!banner) return;
  try {
    const s = await invoke('ffmpeg_status');
    if (s.ffmpeg && s.ffprobe) {
      banner.classList.add('hidden');
      return;
    }
    const msg = document.getElementById('dep-banner-msg');
    msg.innerHTML = 'Nibble needs <strong>ffmpeg</strong> to convert anything. ' +
      'Click below to download it (and yt-dlp for URL downloads) automatically — no manual setup.';
    banner.classList.remove('hidden');
  } catch (e) {
    console.debug('ffmpeg check failed:', e);
  }
}

document.getElementById('dep-banner-close')?.addEventListener('click', () => {
  document.getElementById('dep-banner')?.classList.add('hidden');
});

// "Install automatically" — fetches ffmpeg + ffprobe + yt-dlp into the app's
// data folder so users don't have to install anything by hand.
document.getElementById('dep-install-btn')?.addEventListener('click', async () => {
  const btn = document.getElementById('dep-install-btn');
  const statusEl = document.getElementById('dep-install-status');
  if (btn.disabled) return;
  btn.disabled = true;
  statusEl.textContent = 'Starting…';
  try {
    await invoke('install_dependencies');
    statusEl.textContent = '';
    showToast('Dependencies installed', 'ffmpeg + yt-dlp are ready to use', 'ok');
    // Re-check; hides the banner if ffmpeg is now found.
    await checkFfmpeg();
    await refreshYtdlpStatus();
  } catch (err) {
    statusEl.textContent = '';
    showToast('Install failed', String(err), 'fail');
    btn.disabled = false;
  }
});

// ============================================================
//  Screenshots — region capture (capture.rs + capture.html). Settings live
//  here; the backend gets them via set_capture_config (also (re)registers
//  the global shortcut).
// ============================================================
function fmtHotkey(h) {
  return h ? hkLabel(h).replace(/ \+ /g, '+') : '';
}
async function pushCaptureConfig(announce = false) {
  const hk = settings.shotHotkey ?? 'ctrl+shift+a';
  const action = settings.shotAction || 'copy';
  const rk = settings.recHotkey ?? 'ctrl+shift+r';
  const hint = document.getElementById('set-shot-hotkey-hint');
  const recHint = document.getElementById('set-rec-hotkey-hint');
  // Shortcuts list (?) shows whatever is set in Settings
  for (const [id, combo] of [['sp-key-shot', hk], ['sp-key-rec', rk]]) {
    const dt = document.getElementById(id);
    if (!combo) {
      dt.innerHTML = '<span class="muted">Off</span>';
    } else {
      const k = document.createElement('kbd');
      k.textContent = fmtHotkey(combo);
      dt.replaceChildren(k);
    }
  }
  try {
    await invoke('set_capture_config', {
      hotkey: hotkeysPaused ? '' : hk,
      copy: action !== 'save',
      save: action !== 'copy',
      dir: settings.shotDir || '',
      popup: settings.shotPopup !== false,
      sound: settings.shotSound !== false,
      recHotkey: hotkeysPaused ? '' : rk,
      format: settings.shotFormat || 'png',
    });
    if (hotkeysPaused) return;
    hint.textContent = hk ? 'Works anywhere, even when Nibble is in the background.' : 'No shortcut — use Screen capture on the home screen.';
    recHint.textContent = rk ? 'Opens the capture screen in Video mode. Press it again to stop.' : 'No shortcut — switch to Video in the capture screen.';
    hint.classList.remove('fail');
    recHint.classList.remove('fail');
    if (announce) showToast('Shortcut saved', 'Works anywhere, even when Nibble is in the background.', 'ok', 2000);
  } catch (err) {
    // The error names the combination that failed — show it under that one
    // The backend lists each failure as "<combo> is …" joined by " · "
    const parts = String(err).split(' · ');
    const failedFor = (combo) => combo && parts.find((p) => p.toLowerCase().startsWith(combo + ' '));
    const okText = [
      [hint, hk, 'Works anywhere, even when Nibble is in the background.'],
      [recHint, rk, 'Opens the capture screen in Video mode. Press it again to stop.'],
    ];
    for (const [el, combo, fine] of okText) {
      const f = failedFor(combo);
      el.textContent = f ? `${hkLabel(combo)} is already used by another app — pick another one.` : (combo ? fine : el.textContent);
      el.classList.toggle('fail', !!f);
    }
    if (announce) {
      const mine = parts.find((p) => p.toLowerCase().startsWith((hkRecording?.dataset.setting === 'recHotkey' ? rk : hk) + ' '));
      if (mine) showToast('Shortcut unavailable', 'Another app already uses it — pick another one.', 'fail');
      else showToast('Shortcut saved', 'Works anywhere, even when Nibble is in the background.', 'ok', 2000);
    }
  }
}
// Home: the Recent Activity panel ends where the System block's last line
// ends (left column is centred, so this depends on the window height).
function alignActivityBottom() {
  if (activeViewName() !== 'welcome') return;
  const right = document.querySelector('.welcome-right');
  const user = document.getElementById('welcome-user');
  const ref = user.textContent.trim() && getComputedStyle(user).display !== 'none' ? user : document.getElementById('welcome-hw');
  const z = parseFloat(document.documentElement.style.zoom) || 1;
  const rb = right.getBoundingClientRect().bottom - parseFloat(getComputedStyle(right).paddingBottom) * z;
  const gap = Math.max(0, (rb - ref.getBoundingClientRect().bottom) / z);
  right.style.setProperty('--ra-gap', gap.toFixed(1) + 'px');
}
window.addEventListener('resize', () => requestAnimationFrame(alignActivityBottom));
// The System block fills in after hardware detection → measure again then
{
  const again = () => { requestAnimationFrame(alignActivityBottom); setTimeout(alignActivityBottom, 800); };
  const mo = new MutationObserver(again);
  for (const id of ['welcome-hw', 'welcome-user']) {
    mo.observe(document.getElementById(id), { childList: true, characterData: true, subtree: true, attributes: true });
  }
}

// ============================================================
//  Captures — a browser for screenshots and recordings (like ShareX's
//  history). Opened from the Screen capture tile; capturing itself runs
//  through the shortcuts.
// ============================================================
let capFiles = [], capFilter = 'all';
let capView = localStorage.getItem('nibble.capView') || 'medium';
function applyCapView() {
  if (!['large', 'medium', 'small', 'list'].includes(capView)) capView = 'medium';
  document.getElementById('cap-grid').dataset.view = capView;
  document.querySelectorAll('#cap-views button').forEach((b) => b.classList.toggle('active', b.dataset.v === capView));
}
function capRecDir() {
  try { return JSON.parse(localStorage.getItem('nibble.rec') || '{}').dir || ''; } catch (e) { return ''; }
}
function fmtSize(b) {
  return b >= 1048576 ? `${(b / 1048576).toFixed(b >= 10485760 ? 0 : 1)} MB` : `${Math.max(1, Math.round(b / 1024))} KB`;
}
async function loadCaptures() {
  try {
    capFiles = await invoke('list_captures', { shotDir: settings.shotDir || '', recDir: capRecDir() });
  } catch (e) {
    capFiles = [];
    showToast('Could not read your captures', String(e), 'fail');
  }
  renderCaptures();
}
function renderCaptures() {
  const grid = document.getElementById('cap-grid');
  const isVid = (f) => f.kind !== 'image';
  document.getElementById('cap-n-all').textContent = capFiles.length || '';
  document.getElementById('cap-n-image').textContent = capFiles.filter((f) => !isVid(f)).length || '';
  document.getElementById('cap-n-video').textContent = capFiles.filter(isVid).length || '';
  const shown = capFiles.filter((f) => capFilter === 'all' || (capFilter === 'video' ? isVid(f) : !isVid(f)));
  document.getElementById('cap-empty').classList.toggle('hidden', shown.length > 0);
  grid.replaceChildren();
  shown.forEach((f, i) => {
    const card = document.createElement('div');
    card.className = 'cap-card';
    card.style.setProperty('--i', String(Math.min(i, 16)));
    const media = document.createElement('div');
    media.className = 'cap-media';
    const src = convertFileSrc(f.path);
    if (f.kind === 'video') {
      const v = document.createElement('video');
      v.muted = true; v.loop = true; v.playsInline = true; v.preload = 'metadata';
      v.src = src + '#t=0.3';
      card.addEventListener('mouseenter', () => v.play().catch(() => {}));
      card.addEventListener('mouseleave', () => { v.pause(); });
      media.append(v);
    } else {
      const img = document.createElement('img');
      img.loading = 'lazy'; img.decoding = 'async'; img.draggable = false; img.alt = '';
      img.src = src;
      media.append(img);
    }
    // Type badge: what it is at a glance (screenshot / video / GIF + extension)
    const badge = document.createElement('span');
    badge.className = 'cap-badge ' + f.kind;
    badge.title = f.kind === 'video' ? 'Video recording' : f.kind === 'gif' ? 'GIF recording' : 'Screenshot';
    const icon = f.kind === 'video' ? 'i-video' : f.kind === 'gif' ? 'i-gif' : 'i-camera';
    badge.innerHTML = `<svg class="ic"><use href="#${icon}"/></svg><b></b>`;
    badge.querySelector('b').textContent = (f.name.split('.').pop() || '').toUpperCase();
    media.append(badge);
    const acts = document.createElement('div');
    acts.className = 'cap-acts';
    const act = (icon, title, fn, cls = '') => {
      const b = document.createElement('button');
      b.className = 'cap-act ' + cls;
      b.title = title;
      b.setAttribute('aria-label', title);
      b.innerHTML = `<svg class="ic"><use href="#${icon}"/></svg>`;
      b.addEventListener('click', (e) => { e.stopPropagation(); fn(b); });
      acts.append(b);
    };
    const gone = () => { showToast('File not found', 'It was moved or deleted.', 'fail'); loadCaptures(); };
    act('i-external', 'Open', () => invoke('open_file', { path: f.path }).catch(gone));
    act('i-folder', 'Show in folder', () => invoke('open_file', { path: f.path, reveal: true }).catch(gone));
    act('i-clipboard', 'Copy to clipboard', async (b) => {
      try {
        await invoke('copy_to_clipboard', { path: f.path });
        b.querySelector('use').setAttribute('href', '#i-check');
        b.classList.add('done');
        setTimeout(() => { b.classList.remove('done'); b.querySelector('use').setAttribute('href', '#i-clipboard'); }, 1400);
      } catch (e) { if (String(e) === 'missing') gone(); else showToast('Could not copy', String(e), 'fail'); }
    });
    const permanent = settings.deleteMode === 'permanent';
    act('i-trash', permanent ? 'Delete permanently' : 'Move to the Recycle Bin', async () => {
      if (permanent && !confirm(`Delete "${f.name}" permanently?\nThis can't be undone.`)) return;
      try {
        await invoke('delete_file', { path: f.path, permanent });
        card.classList.add('removing');
        setTimeout(() => { capFiles = capFiles.filter((x) => x.path !== f.path); renderCaptures(); }, 220);
      } catch (e) { showToast('Could not delete the file', String(e), 'fail'); }
    }, 'danger');
    media.append(acts);
    const info = document.createElement('div');
    info.className = 'cap-info';
    const name = document.createElement('div');
    name.className = 'cap-name';
    name.textContent = f.name;
    name.title = f.path;
    const meta = document.createElement('div');
    meta.className = 'cap-meta';
    meta.textContent = [ago(new Date(f.mtime).toISOString()), fmtSize(f.size), f.clipboard ? 'clipboard' : ''].filter(Boolean).join(' · ');
    info.append(name, meta);
    card.append(media, info);
    card.addEventListener('click', () => invoke('open_file', { path: f.path }).catch(gone));
    grid.append(card);
  });
}
function openCaptures() {
  showView('captures');
  loadCaptures();
}
function closeCaptures() {
  document.querySelectorAll('#cap-grid video').forEach((v) => v.pause());
  showView('welcome');
  renderHistory();
}
document.getElementById('btn-go-shot').onclick = openCaptures;
document.getElementById('btn-back-from-captures').onclick = closeCaptures;
document.querySelectorAll('#cap-filter button').forEach((b) => (b.onclick = () => {
  capFilter = b.dataset.f;
  document.querySelectorAll('#cap-filter button').forEach((x) => x.classList.toggle('active', x === b));
  renderCaptures();
}));
document.querySelectorAll('#cap-views button').forEach((b) => (b.onclick = () => {
  capView = b.dataset.v;
  try { localStorage.setItem('nibble.capView', capView); } catch (e) {}
  applyCapView();
}));
applyCapView();
// Any new history row (download, watcher, screenshot, conversion) → refresh
let histTimer = 0;
listen('history-changed', () => {
  clearTimeout(histTimer);
  histTimer = setTimeout(() => {
    renderHistory();
    if (activeViewName() === 'captures') loadCaptures();   // a new shot / recording appears right away
  }, 150);
});
// Files deleted outside the app drop out of the list when you come back
window.addEventListener('focus', () => renderHistory());

listen('recording-done', (event) => {
  const p = event.payload || {};
  if (!p.ok) {
    if (p.error) showToast(p.kind === 'gif' ? 'GIF recording failed' : 'Recording failed', p.error, 'fail', 8000);
    return;
  }
  // A safety limit ended it — always say so (the file is saved)
  if (p.auto) {
    const r = loadRec();
    const why = p.auto === 'time' ? `it reached the ${r.maxMin >= 60 ? r.maxMin / 60 + ' h' : r.maxMin + ' min'} limit`
      : p.auto === 'size' ? `it reached the ${r.maxMb >= 1024 ? r.maxMb / 1024 + ' GB' : r.maxMb + ' MB'} limit`
      : 'the disk is almost full';
    showToast('Recording stopped automatically', `Saved — ${why}. Change the limits in Settings → Recordings.`, 'ok', 9000);
  }
  if (settings.shotPopup !== false) return;   // the preview popup reports it
  const t = showToast(p.kind === 'gif' ? 'GIF saved' : 'Recording saved', 'Click to show the file', 'ok', 4500);
  if (t && p.path) t.addEventListener('click', () => invoke('reveal_in_explorer', { path: p.path }).catch(() => {}));
});

listen('screenshot-done', (event) => {
  const p = event.payload || {};
  if (!p.ok) {
    showToast('Screenshot failed', p.error || 'unknown error', 'fail');
    return;
  }
  renderHistory();
  if (settings.shotPopup !== false) return;   // the preview popup reports it
  const what = p.copied && p.path ? 'Copied and saved' : p.copied ? 'Copied to clipboard' : 'Saved';
  const t = showToast(`Screenshot · ${p.width}×${p.height}`,
    p.path ? `${what} — click to show the file` : `${what} — paste it with Ctrl+V`, 'ok', 4500);
  if (t && p.path) {
    t.addEventListener('click', () => invoke('reveal_in_explorer', { path: p.path }).catch(() => {}));
  }
});
pushCaptureConfig();

// ============================================================
//  First-run welcome + setup check (modal). Shown once per profile (a
//  portable copy has its own profile → once per portable folder); reopen from
//  Settings → System. Checks ffmpeg + yt-dlp and installs what's missing.
// ============================================================
const ONBOARD_KEY = 'nibble.onboarded.v1';
const onboard = document.getElementById('onboard');
let onboardBusy = false;

function obSetRow(tool, state) {
  document.getElementById(`ob-row-${tool}`).dataset.state = state;
}
async function obCheck() {
  obSetRow('ffmpeg', 'checking');
  obSetRow('ytdlp', 'checking');
  // Small minimum so the check reads as a check, not a flicker
  const minDelay = new Promise((r) => setTimeout(r, 650));
  const [ff, yt] = await Promise.all([
    invoke('ffmpeg_status').catch(() => ({})),
    invoke('ytdlp_status').catch(() => ({})),
    minDelay,
  ]);
  obSetRow('ffmpeg', ff.ffmpeg && ff.ffprobe ? 'ready' : 'missing');
  await new Promise((r) => setTimeout(r, 140));   // let the two ticks land one after the other
  obSetRow('ytdlp', yt.available ? 'ready' : 'missing');
  obUpdateNote();
}
function obUpdateNote() {
  const states = ['ffmpeg', 'ytdlp'].map((t) => document.getElementById(`ob-row-${t}`).dataset.state);
  const note = document.getElementById('ob-note');
  const start = document.getElementById('ob-start');
  start.disabled = onboardBusy;
  if (onboardBusy || states.includes('installing')) {
    note.textContent = 'Installing — this takes a few seconds…';
  } else if (states.includes('missing')) {
    note.textContent = 'Everything installs next to Nibble — no admin rights, nothing system-wide.';
  } else {
    note.textContent = 'All set — you can start right away.';
  }
  start.innerHTML = states.includes('missing') && !onboardBusy
    ? 'Skip for now<svg class="ic"><use href="#i-arrow-right"/></svg>'
    : 'Get started<svg class="ic"><use href="#i-arrow-right"/></svg>';
}
function showOnboarding() {
  onboard.classList.remove('hidden', 'closing');
  obCheck();
}
function closeOnboarding() {
  if (onboardBusy) return;
  try { localStorage.setItem(ONBOARD_KEY, '1'); } catch (e) { /* noop */ }
  onboard.classList.add('closing');
  setTimeout(() => onboard.classList.add('hidden'), 260);
  // The app's own banners / download view / settings follow the new state
  checkFfmpeg();
  refreshYtdlpStatus();
  if (activeViewName() === 'settings') refreshToolsStatus();
}
document.getElementById('ob-start').onclick = closeOnboarding;
// Keys while the welcome is open: Esc / Enter close it (when idle).
document.addEventListener('keydown', (e) => {
  if (onboard.classList.contains('hidden')) return;
  if (e.key === 'Escape' || (e.key === 'Enter' && !e.target.closest?.('.ob-install'))) {
    e.preventDefault();
    closeOnboarding();
  }
});
onboard.querySelectorAll('.ob-install').forEach((btn) => {
  btn.onclick = async () => {
    const tool = btn.dataset.tool;
    onboardBusy = true;
    obSetRow(tool, 'installing');
    obUpdateNote();
    try {
      if (tool === 'ytdlp') {
        await invoke('ytdlp_update');          // installs it when missing
      } else {
        // ffmpeg (+ yt-dlp in one go if that's missing too)
        if (document.getElementById('ob-row-ytdlp').dataset.state === 'missing') obSetRow('ytdlp', 'installing');
        await invoke('install_dependencies');
      }
    } catch (err) {
      showToast(`${tool === 'ytdlp' ? 'yt-dlp' : 'ffmpeg'} install failed`, String(err), 'fail');
    } finally {
      onboardBusy = false;
      await obCheck();
    }
  };
});
document.getElementById('set-show-welcome').onclick = showOnboarding;

// Settings → System → Required tools: live status line
async function refreshToolsStatus() {
  const el = document.getElementById('set-tools-status');
  el.textContent = 'Checking…';
  const [ff, yt] = await Promise.all([
    invoke('ffmpeg_status').catch(() => ({})),
    invoke('ytdlp_status').catch(() => ({})),
  ]);
  const ok = (b) => (b ? '<span class="ok">✓</span>' : '<span class="fail">✗ missing</span>');
  const ffVer = ff.version ? ff.version.split('-')[0] : '';   // "8.1.1-full_build-…" → "8.1.1"
  el.innerHTML = `ffmpeg ${ok(ff.ffmpeg && ff.ffprobe)}${ffVer ? ` <span class="muted">${escapeHtml(ffVer)}</span>` : ''}` +
    `  ·  yt-dlp ${ok(yt.available)}${yt.version ? ` <span class="muted">${escapeHtml(yt.version)}</span>` : ''}`;
}

// Every "ZIPEEK" opens the author's GitHub in the default browser.
document.querySelectorAll('.author-link').forEach((a) => {
  a.addEventListener('click', (e) => {
    e.stopPropagation();
    invoke('open_author_page').catch((err) => showToast('Could not open the browser', String(err), 'fail'));
  });
});
// The installer reports its steps — show them under the checklist
listen('dep-progress', (event) => {
  if (!onboard.classList.contains('hidden') && onboardBusy) {
    document.getElementById('ob-note').textContent = String(event.payload ?? '');
  }
});
try {
  if (!localStorage.getItem(ONBOARD_KEY)) showOnboarding();
} catch (e) { /* storage blocked → skip the welcome */ }

// Live progress text from the backend during the download/extract steps.
listen('dep-progress', (event) => {
  const statusEl = document.getElementById('dep-install-status');
  if (statusEl) statusEl.textContent = String(event.payload ?? '');
});
