// Prospect web client: one ES module, no dependencies, no build step. The contract is
// docs/API.md. Untrusted text only ever goes through textContent; only http(s) links are
// rendered, always with rel="noopener noreferrer".

const BRAND = '#1d6b57';
const SVGNS = 'http://www.w3.org/2000/svg';
const doc = document;
const $ = (sel, root = doc) => root.querySelector(sel);
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const reduced = matchMedia('(prefers-reduced-motion: reduce)');
const plural = (n, one, many) => `${n} ${n === 1 ? one : many}`;

/* ------------------------------------------------------------------ DOM helpers */

const PROPS = new Set(['value', 'checked', 'disabled', 'hidden', 'open']);
function h(tag, props, ...kids) {
  const el = doc.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v == null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k === 'text') el.textContent = v;
    else if (k === 'on') for (const [ev, fn] of Object.entries(v)) el.addEventListener(ev, fn);
    else if (PROPS.has(k)) el[k] = v;
    else el.setAttribute(k, v === true ? '' : String(v));
  }
  return add(el, kids);
}
function add(el, kids) {
  for (const k of kids.flat(9)) if (k != null && k !== false) el.append(k instanceof Node ? k : String(k));
  return el;
}
function svgUse(id, cls) {
  const s = doc.createElementNS(SVGNS, 'svg');
  s.setAttribute('class', cls);
  s.setAttribute('aria-hidden', 'true');
  s.setAttribute('focusable', 'false');
  if (cls === 'logo') s.setAttribute('viewBox', '0 0 32 32');
  const u = doc.createElementNS(SVGNS, 'use');
  u.setAttribute('href', '#' + id);
  s.append(u);
  return s;
}
const icon = (name, cls) => svgUse('i-' + name, cls ? 'i ' + cls : 'i');
let uidN = 0;
const uid = (p = 'u') => `${p}${++uidN}`;

function Btn(label, { kind = 'primary', type = 'button', icon: ic, after, small, block, onClick, attrs } = {}) {
  const b = h('button', { type, class: `btn btn-${kind}${small ? ' btn-sm' : ''}${block ? ' btn-block' : ''}`, ...attrs },
    ic && !after ? icon(ic) : null, h('span', { text: label }), ic && after ? icon(ic) : null);
  if (onClick) b.addEventListener('click', onClick);
  return b;
}
function setLabel(btn, text) { const s = btn.querySelector('span:not(.spinner)'); if (s) s.textContent = text; }

// Runs fn while showing a spinner on the button; ignores clicks while busy (without blurring it).
async function busy(btn, fn) {
  if (btn.getAttribute('aria-busy') === 'true') return;
  btn.setAttribute('aria-busy', 'true');
  const sp = h('span', { class: 'spinner', 'aria-hidden': 'true' });
  btn.append(sp);
  try { return await fn(); } finally { btn.removeAttribute('aria-busy'); sp.remove(); }
}

function safeHref(url) {
  if (typeof url !== 'string' || !url.trim()) return null;
  try {
    const u = new URL(url.trim());
    return u.protocol === 'https:' || u.protocol === 'http:' ? u.href : null;
  } catch { return null; }
}
function outLink(url, kids, cls) {
  const href = safeHref(url);
  return href ? h('a', { href, target: '_blank', rel: 'noopener noreferrer', class: cls }, kids) : h('span', { class: cls }, kids);
}
function hostOf(url) { try { return new URL(url).hostname.replace(/^www\./, ''); } catch { return url; } }
function appLink(path, kids, cls = 'link') {
  return h('a', { href: path, class: cls, on: { click: (e) => {
    if (e.metaKey || e.ctrlKey || e.shiftKey || e.altKey || e.button) return;
    e.preventDefault();
    go(path);
  } } }, kids);
}
function Wordmark() { return appLink('/', [svgUse('logo', 'logo'), h('span', { text: 'Prospect' })], 'wordmark'); }

/* ------------------------------------------------------------------ API */

class ApiError extends Error {
  constructor(status, code, message, field) { super(message); Object.assign(this, { status, code, field }); }
}
async function api(method, path, body) {
  const opts = { method, credentials: 'same-origin', headers: { Accept: 'application/json' } };
  if (method !== 'GET') { opts.headers['Content-Type'] = 'application/json'; opts.body = JSON.stringify(body || {}); }
  let res;
  try { res = await fetch(path, opts); } catch {
    throw new ApiError(0, 'network', "We can't reach Prospect right now. Check your connection and try again.");
  }
  let data = null;
  if (res.status !== 204) { try { data = await res.json(); } catch { data = null; } }
  if (!res.ok) {
    const e = (data && data.error) || {};
    const err = new ApiError(res.status, e.code || 'server_error', e.message || 'Something went wrong on our side. Please try again.', e.field || null);
    // A 401 that names a field is a wrong password, not an expired session.
    if (res.status === 401 && !err.field && S.me && !path.startsWith('/api/auth/')) sessionEnded();
    throw err;
  }
  return data;
}

/* ------------------------------------------------------------------ state + formatting */

const S = {
  meta: null, me: null, items: [], saved: [], counts: { active: 0, saved: 0, new: 0 },
  lastRefresh: null, nextRefresh: null, itemsLoaded: false, notes: null,
  view: 'all', category: 'all', tab: 'opportunities', notice: null,
};
let screen = null;

const rtf = new Intl.RelativeTimeFormat('en', { numeric: 'auto' });
function relTime(unix) {
  const diff = unix - Date.now() / 1000;
  const abs = Math.abs(diff);
  if (abs < 60) return diff < 0 ? 'just now' : 'in a moment';
  let unit = 'minute', size = 60;
  for (const [u, s] of [['hour', 3600], ['day', 86400], ['week', 604800], ['month', 2629800], ['year', 31557600]]) if (abs >= s) { unit = u; size = s; }
  return rtf.format(Math.round(diff / size), unit);
}
function deadlineInfo(ymd) {
  const m = /^(\d{4})-(\d{2})-(\d{2})/.exec(ymd || '');
  if (!m) return null;
  const due = new Date(+m[1], +m[2] - 1, +m[3]);
  const today = new Date(); today.setHours(0, 0, 0, 0);
  const days = Math.round((due - today) / 86400000);
  const date = due.toLocaleDateString('en-GB', { day: 'numeric', month: 'long', year: due.getFullYear() === today.getFullYear() ? undefined : 'numeric' });
  if (days < 0) return { text: 'Closed', level: 'urgent', date };
  if (days === 0) return { text: 'Closes today', level: 'urgent', date };
  if (days === 1) return { text: 'Closes tomorrow', level: 'urgent', date };
  if (days <= 45) return { text: `Closes in ${days} days`, level: days <= 3 ? 'urgent' : days <= 10 ? 'soon' : '', date };
  return { text: `Closes ${date}`, level: '', date };
}
const initials = (u) => { const w = String((u && u.business_name) || '?').split(/\s+/).filter((x) => /[A-Za-z0-9]/.test(x)); return ((w[0] || '?')[0] + (w[1] ? w[1][0] : '')).toUpperCase(); };
const typeLabel = (id) => (S.meta.opportunity_types.find((t) => t.id === id) || { label: 'Opportunity' }).label;

// Keeps the theme accent readable: darkens it until white text on it passes WCAG AA.
function readableAccent(hex) {
  if (!/^#[0-9a-f]{6}$/i.test(hex || '')) return BRAND;
  let rgb = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16));
  const lum = (c) => c.map((v) => v / 255).map((v) => (v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4)).reduce((a, v, i) => a + v * [0.2126, 0.7152, 0.0722][i], 0);
  for (let k = 0; k < 20 && 1.05 / (lum(rgb) + 0.05) < 4.6; k++) rgb = rgb.map((v) => Math.round(v * 0.92));
  return '#' + rgb.map((v) => v.toString(16).padStart(2, '0')).join('');
}
function applyAccent(hex) { doc.documentElement.style.setProperty('--accent', readableAccent(hex || BRAND)); }
const photoUrl = (t) => (t && typeof t.image_url === 'string' && (/^\/(?!\/)/.test(t.image_url) || /^https:\/\//i.test(t.image_url)) ? t.image_url : null);

/* ------------------------------------------------------------------ feedback */

function toast(message, { action, onAction, error, timeout = 5200 } = {}) {
  const box = $('#toasts');
  const el = h('div', { class: 'toast' + (error ? ' is-error' : '') }, h('span', { text: message }));
  let timer = 0;
  const close = () => { clearTimeout(timer); el.classList.add('is-leaving'); setTimeout(() => el.remove(), 260); };
  if (action) el.append(h('button', { type: 'button', text: action, on: { click: () => { close(); if (onAction) onAction(); } } }));
  box.append(el);
  while (box.children.length > 3) box.firstElementChild.remove();
  const arm = (ms) => { clearTimeout(timer); timer = setTimeout(close, ms); };
  arm(action ? Math.max(timeout, 8000) : timeout);
  el.addEventListener('mouseenter', () => clearTimeout(timer));
  el.addEventListener('mouseleave', () => arm(3000));
  el.addEventListener('focusin', () => clearTimeout(timer));
  return { close };
}
function announce(msg) { const a = $('#announcer'); if (a) a.textContent = msg; }

/* ------------------------------------------------------------------ form pieces */

function Field({ name, label, type = 'text', value = '', hint, placeholder, autocomplete, optional, textarea, rows, maxlength, extra, inputmode }) {
  const id = uid('f');
  const hintId = hint ? id + '-h' : null;
  const input = textarea
    ? h('textarea', { class: 'textarea', id, name, rows: rows || 3, placeholder, maxlength, 'aria-describedby': hintId })
    : h('input', { class: 'input', id, name, type, placeholder, autocomplete, maxlength, inputmode, 'aria-describedby': hintId,
      spellcheck: type === 'email' || type === 'password' ? 'false' : null, autocapitalize: type === 'email' ? 'off' : null });
  input.value = value || '';
  const lab = h('label', { class: 'label', for: id }, label, optional ? h('span', { class: 'optional', text: ' (optional)' }) : null);
  const el = h('div', { class: 'field' }, extra ? h('div', { class: 'label-row' }, lab, extra) : lab, input,
    hint ? h('p', { class: 'hint', id: hintId, text: hint }) : null);
  return { el, input };
}
function PasswordField(opts) {
  const f = Field({ ...opts, type: 'password' });
  const btn = h('button', { type: 'button', class: 'icon-btn', 'aria-label': 'Show password', 'aria-pressed': 'false' }, icon('eye'));
  btn.addEventListener('click', () => {
    const show = f.input.type === 'password';
    f.input.type = show ? 'text' : 'password';
    btn.setAttribute('aria-pressed', String(show));
    btn.replaceChildren(icon(show ? 'eye-off' : 'eye'));
  });
  const wrap = h('div', { class: 'pw' });
  f.input.replaceWith(wrap);
  wrap.append(f.input, btn);
  return f;
}
function Picks({ name, type, label, options, value = [], hint, liveHint, segment }) {
  const hintId = uid('h');
  const hintEl = hint ? h('p', { class: 'hint pick-hint', id: hintId, text: hint }) : null;
  const inputs = [];
  const wrap = h('div', { class: segment ? 'segment' : 'picks' }, options.map((o) => {
    const input = h('input', { type, name, value: o.id, checked: value.includes(o.id) });
    inputs.push(input);
    const lab = h('label', { class: 'pick' }, input, h('span', { text: o.label }));
    if (liveHint && o.hint) {
      const show = () => { hintEl.textContent = `${o.label}: ${o.hint}`; };
      const reset = () => { hintEl.textContent = hint; };
      lab.addEventListener('mouseenter', show); lab.addEventListener('mouseleave', reset);
      input.addEventListener('focus', show); input.addEventListener('blur', reset);
    }
    return lab;
  }));
  const el = h('fieldset', { class: 'field', 'data-field': name, 'aria-describedby': hint ? hintId : null },
    h('legend', { class: 'label', text: label }), wrap, hintEl);
  return { el, get value() { return inputs.filter((i) => i.checked).map((i) => i.value); } };
}
const squash = (s) => String(s || '').replace(/\s+/g, ' ').trim();
function normUrl(v) {
  if (/^https?:\/\//i.test(v)) return safeHref(v) ? v : '';
  return /^[\w-]+(\.[\w-]+)+([/?#].*)?$/.test(v) ? 'https://' + v : '';
}
function TagInput({ name, label, values = [], placeholder, hint, optional, max = 20, maxLen = 60, normalize }) {
  let tags = [...(values || [])];
  const id = uid('t');
  const list = h('ul', { class: 'tags-list' });
  const input = h('input', { class: 'tags-input', id, type: 'text', placeholder, autocomplete: 'off', enterkeyhint: 'enter', 'aria-describedby': hint ? id + '-h' : null });
  const box = h('div', { class: 'tags', 'data-field': name }, list, input);
  const errSlot = h('div');
  box.addEventListener('click', (e) => { if (e.target === box) input.focus(); });
  function commit() {
    const raw = input.value;
    if (!raw.trim()) return true;
    let ok = true;
    for (let part of raw.split(/[,\n]/)) {
      part = squash(part).slice(0, maxLen);
      if (!part) continue;
      const v = normalize ? normalize(part) : part;
      if (!v) { ok = false; continue; }
      if (tags.length < max && !tags.some((t) => t.toLowerCase() === v.toLowerCase())) tags.push(v);
    }
    errSlot.replaceChildren(ok ? '' : h('p', { class: 'field-error' }, icon('alert'), h('span', { text: 'That doesn’t look like a web address. Try something like example.com.' })));
    if (ok) input.value = '';
    render();
    return ok;
  }
  function render() {
    list.replaceChildren(...tags.map((t, i) => h('li', { class: 'tag' }, h('span', { text: t }),
      h('button', { type: 'button', 'aria-label': `Remove ${t}`, on: { click: () => { tags.splice(i, 1); render(); input.focus(); } } }, icon('x')))));
  }
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' || (e.key === ',' && input.value.trim())) { e.preventDefault(); commit(); }
    else if (e.key === 'Backspace' && !input.value && tags.length) { tags.pop(); render(); }
  });
  input.addEventListener('blur', commit);
  render();
  const el = h('div', { class: 'field' },
    h('label', { class: 'label', for: id }, label, optional ? h('span', { class: 'optional', text: ' (optional)' }) : null),
    box, errSlot, hint ? h('p', { class: 'hint', id: id + '-h', text: hint }) : null);
  return { el, get value() { commit(); return [...tags]; } };
}

function clearErrors(form) {
  form.querySelectorAll('.field-error, .form-error.is-api').forEach((e) => e.remove());
  form.querySelectorAll('[aria-invalid]').forEach((e) => {
    e.removeAttribute('aria-invalid');
    const d = (e.getAttribute('aria-describedby') || '').split(' ').filter((x) => x && !x.endsWith('-err')).join(' ');
    if (d) e.setAttribute('aria-describedby', d); else e.removeAttribute('aria-describedby');
  });
  form.querySelectorAll('.is-invalid').forEach((e) => e.classList.remove('is-invalid'));
}
// Shows an API error next to the field it names (error.field), or at the top of the form.
function showError(form, err, slot) {
  clearErrors(form);
  const f = err.field && CSS.escape(err.field);
  const target = f && (form.querySelector(`[data-field="${f}"]`) || form.querySelector(`[name="${f}"]`));
  if (target) {
    const fieldEl = target.closest('.field') || target.parentElement;
    const input = target.matches('input, textarea') ? target : target.querySelector('input, textarea');
    const id = (input && input.id ? input.id : uid('e')) + '-err';
    fieldEl.append(h('p', { class: 'field-error', id }, icon('alert'), h('span', { text: err.message })));
    if (target.classList.contains('tags')) target.classList.add('is-invalid');
    if (input) {
      input.setAttribute('aria-invalid', 'true');
      input.setAttribute('aria-describedby', [input.getAttribute('aria-describedby'), id].filter(Boolean).join(' '));
      input.focus();
    }
  } else {
    const box = h('div', { class: 'form-error is-api', role: 'alert' }, icon('alert'), h('span', { text: err.message }));
    (slot || form).replaceChildren(box);
  }
}
const Notice = (text) => h('div', { class: 'notice', role: 'status' }, icon('check'), h('span', { text }));

/* ------------------------------------------------------------------ split-flap board */

const FLAPS = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ';
function Board(n, cls = '') {
  const el = h('div', { class: ('board ' + cls).trim(), 'aria-hidden': 'true' });
  const cells = [];
  for (let i = 0; i < n; i++) {
    const parts = ['top', 'bottom', 'flap-top', 'flap-bottom'].map((c) => { const t = h('span'); return [h('span', { class: c }, t), t]; });
    el.append(h('span', { class: 'cell' }, parts.map((p) => p[0])));
    cells.push({ ch: ' ', top: parts[0][1], bot: parts[1][1], ft: parts[2][0], ftT: parts[2][1], fb: parts[3][0], fbT: parts[3][1], seq: 0 });
  }
  let timer = 0, alive = true;
  const paint = (c, ch) => { c.ch = ch; c.top.textContent = c.bot.textContent = c.ftT.textContent = c.fbT.textContent = ch; };
  paint.all = (t) => cells.forEach((c, i) => paint(c, t[i]));
  function flipOnce(c, next) {
    const cur = c.ch;
    c.top.textContent = next; c.ftT.textContent = cur; c.bot.textContent = cur; c.fbT.textContent = next;
    const a = c.ft.animate([{ transform: 'rotateX(0deg)' }, { transform: 'rotateX(-90deg)' }], { duration: 85, easing: 'ease-in', fill: 'forwards' });
    const b = c.fb.animate([{ transform: 'rotateX(90deg)' }, { transform: 'rotateX(0deg)' }], { duration: 85, delay: 85, easing: 'ease-out', fill: 'forwards' });
    return b.finished.catch(() => {}).then(() => { paint(c, next); a.cancel(); b.cancel(); });
  }
  async function flipTo(c, target, delay) {
    const my = ++c.seq;
    if (delay) await wait(delay);
    if (my !== c.seq || target === c.ch) return;
    const hops = c.ch === ' ' || target === ' ' ? 1 : 1 + Math.floor(Math.random() * 3);
    for (let k = 0; k < hops && alive && my === c.seq; k++) await flipOnce(c, FLAPS[Math.floor(Math.random() * 26)]);
    if (alive && my === c.seq) await flipOnce(c, target);
  }
  function show(text, instant) {
    let t = String(text).toUpperCase().slice(0, n);
    const pad = Math.floor((n - t.length) / 2);
    t = ' '.repeat(pad) + t + ' '.repeat(n - t.length - pad);
    if (instant || reduced.matches || doc.hidden) { cells.forEach((c) => c.seq++); paint.all(t); return; }
    cells.forEach((c, i) => flipTo(c, t[i], i * 38));
  }
  function cycle(words, { every = 2600, end } = {}) {
    clearTimeout(timer);
    const list = words.filter(Boolean);
    if (end && reduced.matches) return show(end, true);
    let i = 0;
    const step = () => {
      if (!alive) return;
      if (end && i >= list.length) return show(end);
      show(list[i % list.length]);
      i++;
      timer = setTimeout(step, every);
    };
    step();
  }
  return { el, show, cycle, stop() { alive = false; clearTimeout(timer); }, pause() { clearTimeout(timer); } };
}
// Up to 12 characters: whole words only.
function boardWord(s) {
  let out = '';
  for (const w of String(s || '').toUpperCase().split(/\s+/)) {
    if (!w) continue;
    const next = out ? out + ' ' + w : w;
    if (next.length > 12) break;
    out = next;
  }
  return out;
}
const SHORT = { sales: 'SALES LEADS', partners: 'PARTNERS', funding: 'FUNDING', regulation: 'REGULATION', market: 'MARKETS',
  visibility: 'EVENTS', talent: 'TALENT', operations: 'SUPPLIERS', other: 'OPENINGS' };
function searchWords() {
  const p = (S.me && S.me.profile) || {};
  const types = ((p.seeking && p.seeking.length) ? p.seeking : ['sales', 'partners', 'funding', 'market', 'visibility']).map((id) => SHORT[id] || boardWord(id));
  const own = [p.industry, ...(p.interests || []).slice(0, 2), (p.location || '').split(',')[0]].map(boardWord).filter(Boolean);
  const out = [];
  for (let i = 0; i < Math.max(types.length, own.length); i++) { if (types[i]) out.push(types[i]); if (own[i]) out.push(own[i]); }
  return out;
}

function Feed() {
  const el = h('ol', { class: 'feed' });
  let shown = [];
  function update(run, finalMsg) {
    const log = run && run.log && run.log.length ? run.log : run && run.progress && run.progress.message ? [run.progress] : [];
    let steps = log.map((s) => s.message).filter(Boolean);
    if (finalMsg) steps = [...steps, finalMsg];
    const recent = steps.slice(-4);
    if (recent.join('\n') === shown.join('\n')) return;
    const prevLast = shown[shown.length - 1];
    el.replaceChildren(...recent.map((msg, i) => {
      const last = i === recent.length - 1;
      const current = last && !finalMsg;
      const li = h('li', { class: current ? 'is-current' : '' }, current ? h('span', { class: 'feed-dot' }) : icon('check'), h('span', { text: msg }));
      if (last && msg !== prevLast) li.classList.add('is-entering');
      return li;
    }));
    shown = recent;
  }
  return { el, update };
}
function Meter() {
  const fill = h('i');
  const bar = h('div', { class: 'progress', role: 'progressbar', 'aria-label': 'Progress', 'aria-valuemin': '0', 'aria-valuemax': '100' }, fill);
  const pct = h('span', { class: 'meter-pct' });
  const el = h('div', { class: 'meter' }, bar, pct);
  function update(p) {
    const known = typeof p === 'number' && p > 0;
    bar.classList.toggle('is-indeterminate', !known);
    if (known) { bar.setAttribute('aria-valuenow', String(p)); fill.style.setProperty('--p', String(Math.min(p, 100) / 100)); } else bar.removeAttribute('aria-valuenow');
    pct.textContent = known ? `${p}%` : '';
  }
  return { el, bar, update };
}

// The background of the hero and the stages: a gradient of the accent, with the theme photo
// fading in over it once it has loaded.
const seenPhotos = new Set();
function Media() {
  const scrim = h('div', { class: 'media-scrim' });
  const el = h('div', { class: 'media' }, h('div', { class: 'media-gradient' }), scrim);
  let current = null, img = null;
  function set(url) {
    if (url === current) return;
    current = url;
    if (!url) { if (img) img.remove(); img = null; return; }
    const next = h('img', { class: 'media-photo', alt: '', decoding: 'async' });
    next.addEventListener('load', () => {
      const old = img;
      img = next;
      if (seenPhotos.has(url) && !old) { next.classList.add('is-in'); next.style.setProperty('transition', 'none'); }
      else requestAnimationFrame(() => requestAnimationFrame(() => next.classList.add('is-in')));
      seenPhotos.add(url);
      if (old) setTimeout(() => old.remove(), 1800);
    }, { once: true });
    next.addEventListener('error', () => next.remove(), { once: true });
    next.src = url;
    scrim.before(next);
  }
  return { el, set };
}

/* ------------------------------------------------------------------ dialogs */

function openDialog({ cls, title, body, onClose }) {
  const opener = doc.activeElement;
  const id = uid('d');
  const closeBtn = h('button', { type: 'button', class: 'icon-btn', 'aria-label': 'Close' }, icon('x'));
  const dlg = h('dialog', { class: cls, 'aria-labelledby': id },
    h('div', { class: 'dialog-box' },
      h('div', { class: 'dialog-head' }, h('h2', { class: 'h3', id, text: title }), closeBtn),
      h('div', { class: 'dialog-body' }, body)));
  closeBtn.addEventListener('click', () => dlg.close());
  dlg.addEventListener('click', (e) => { if (e.target === dlg) dlg.close(); });
  dlg.addEventListener('close', () => {
    dlg.remove();
    if (!doc.querySelector('dialog[open]')) doc.documentElement.classList.remove('has-dialog');
    if (onClose) onClose();
    if (opener && opener.isConnected) opener.focus();
  });
  doc.body.append(dlg);
  doc.documentElement.classList.add('has-dialog');
  dlg.showModal();
  return dlg;
}

/* ------------------------------------------------------------------ routing */

function go(path, replace) {
  if (location.pathname + location.search !== path) history[replace ? 'replaceState' : 'pushState'](null, '', path);
  route();
}
function mount(name, build) {
  if (screen && screen.destroy) screen.destroy();
  const s = build();
  s.name = name;
  $('#app').replaceChildren(s.el);
  screen = s;
  window.scrollTo(0, 0);
  const f = s.focus ? s.focus() : $('h1', s.el);
  if (f) f.focus({ preventScroll: true });
}
function show(name, build, update) {
  if (screen && screen.name === name) { if (update) update(screen); return; }
  mount(name, build);
}
let routeSeq = 0;
async function route() {
  const seq = ++routeSeq;
  const path = location.pathname;
  const fix = (p) => { if (location.pathname !== p || (p !== '/' && location.search)) history.replaceState(null, '', p); };
  if (path === '/reset') return show('auth', () => AuthScreen('reset'), (s) => s.setMode('reset'));
  if (!S.me) {
    const mode = path === '/signup' ? 'signup' : path === '/forgot' ? 'forgot' : 'signin';
    if (!['/', '/signup', '/forgot'].includes(path)) fix('/');
    applyAccent(BRAND);
    return show('auth', () => AuthScreen(mode), (s) => s.setMode(mode));
  }
  applyAccent(S.me.theme && S.me.theme.accent);
  if (S.me.run) startPolling();
  const ob = S.me.user.onboarding;
  if (ob === 'researching') { fix('/'); return show('research', ResearchScreen); }
  if (ob !== 'ready') { fix('/'); return show('review', ReviewScreen); }
  const wantSettings = path === '/settings';
  S.view = path === '/saved' ? 'saved' : 'all';
  if (!['/', '/saved'].includes(path)) fix('/');
  const qs = new URLSearchParams(location.search);
  S.category = qs.get('category') || 'all';
  S.tab = S.view === 'saved' ? 'opportunities' : qs.get('tab') || 'opportunities';
  if (!S.itemsLoaded) {
    try { await loadItems(); } catch (e) { if (e.status !== 401) toast(e.message, { error: true }); }
    if (seq !== routeSeq || !S.me) return;
  }
  show('dashboard', DashboardScreen, (s) => s.update('route'));
  if (wantSettings && !doc.querySelector('dialog.drawer[open]')) openSettings();
}
addEventListener('popstate', route);

async function loadItems() {
  const [all, saved] = await Promise.all([api('GET', '/api/opportunities'), api('GET', '/api/opportunities?view=saved')]);
  S.items = all.items || [];
  S.saved = saved.items || [];
  S.counts = all.counts || { active: 0, saved: 0, new: 0 };
  S.lastRefresh = all.last_refresh || null;
  S.nextRefresh = all.next_refresh || null;
  S.itemsLoaded = true;
}
async function refreshMe() {
  const before = S.me && S.me.theme;
  S.me = await api('GET', '/api/me');
  applyAccent(S.me.theme && S.me.theme.accent);
  const t = S.me.theme || {};
  if (!before || before.image_url !== t.image_url || before.status !== t.status || before.label !== t.label) {
    if (screen && screen.update) screen.update('theme');
  }
}
function resetState() {
  stopPolling();
  Object.assign(S, { me: null, items: [], saved: [], counts: { active: 0, saved: 0, new: 0 }, lastRefresh: null, nextRefresh: null, itemsLoaded: false, notes: null, view: 'all', category: 'all', tab: 'opportunities' });
  doc.querySelectorAll('dialog[open]').forEach((d) => d.close());
  applyAccent(BRAND);
}
function sessionEnded() {
  if (!S.me) return;
  resetState();
  S.notice = 'You were signed out. Sign in again to carry on.';
  go('/', true);
}
async function signOut() {
  try { await api('POST', '/api/auth/logout', {}); } catch { /* signing out locally either way */ }
  resetState();
  go('/', true);
}
async function enter() {
  S.me = await api('GET', '/api/me');
  go('/', true);
}

/* ------------------------------------------------------------------ polling */

let pollTimer = 0, pollBusy = false, pollCount = 0, lastStage = '';
function startPolling() { if (!pollTimer && !pollBusy) pollTimer = setTimeout(pollOnce, 1500); }
function stopPolling() { clearTimeout(pollTimer); pollTimer = 0; }
async function pollOnce() {
  pollTimer = 0;
  if (!S.me) return;
  pollBusy = true;
  const before = S.me.run;
  try {
    const r = await api('GET', '/api/runs/current');
    if (!S.me) return;
    S.me.run = r.run;
    S.me.last_run = r.last_run;
    pollCount++;
    if (r.run) {
      const stage = r.run.progress && r.run.progress.stage;
      if (stage && stage !== lastStage) { lastStage = stage; announce(r.run.progress.message || ''); }
      if ((!S.me.theme || S.me.theme.status !== 'ready') && pollCount % 2 === 0) await refreshMe();
      if (screen && screen.update) screen.update('progress');
    } else if (before) {
      lastStage = '';
      await afterRun(before);
    }
  } catch { /* a dropped poll is retried below */ } finally {
    pollBusy = false;
    if (S.me && S.me.run && !pollTimer) pollTimer = setTimeout(pollOnce, doc.hidden ? 5000 : 1500);
  }
}
async function afterRun(run) {
  await refreshMe();
  if (S.me.user.onboarding !== 'ready' || !screen || screen.name !== 'dashboard') return route();
  if (run.kind === 'discover') { try { await loadItems(); } catch { /* keep what we have */ } }
  const done = S.me.last_run && S.me.last_run.id === run.id ? S.me.last_run : run;
  if (screen && screen.update) screen.update('run-ended', done);
}

/* ------------------------------------------------------------------ auth */

const AUTH_WORDS = ['TENDERS', 'PARTNERS', 'GRANTS', 'CUSTOMERS', 'INCENTIVES', 'MARKETS', 'SUPPLIERS', 'AWARDS', 'TALENT'];
function AuthScreen(mode) {
  const board = Board(12, 'auth-board');
  const panel = h('main', { class: 'auth-panel', id: 'main' });
  const el = h('div', { class: 'auth' },
    h('section', { class: 'auth-brand on-dark' },
      h('div', null, Wordmark()),
      h('div', null,
        h('p', { class: 'auth-statement', text: 'Prospect your market. Find the opportunities worth acting on.' }),
        board.el,
        h('p', { class: 'auth-caption', text: 'Give us your business name and website. We research your products, customers, competitors, market and regulation from public sources, then find the leads, partners, funding and openings worth acting on.' })),
      h('p', { class: 'auth-foot', text: 'Every finding links to its source. Connected tools are read-only, always.' })),
    panel);
  board.cycle(AUTH_WORDS, { every: 2300, end: 'PROSPECT' });
  const forms = { signin: SignInForm, signup: SignUpForm, forgot: ForgotForm, reset: ResetForm };
  function setMode(m) {
    panel.replaceChildren((forms[m] || SignInForm)());
    const f = $('h1', panel);
    if (f && screen && screen.el === el) f.focus({ preventScroll: true });
  }
  setMode(mode);
  return { el, setMode, destroy: board.stop };
}
function SignInForm() {
  const slot = h('div');
  const email = Field({ name: 'email', label: 'Email', type: 'email', autocomplete: 'email', inputmode: 'email' });
  const pw = PasswordField({ name: 'password', label: 'Password', autocomplete: 'current-password', extra: appLink('/forgot', 'Forgot password?') });
  const submit = Btn('Sign in', { type: 'submit', block: true });
  const form = h('form', { class: 'form auth-form', novalidate: true },
    h('h1', { class: 'h1', tabindex: '-1', text: 'Welcome back' }),
    h('p', { class: 'lede', text: 'Sign in to your business account.' }),
    S.notice ? Notice(S.notice) : null, slot, email.el, pw.el, submit,
    h('p', { class: 'auth-switch' }, 'New here? ', appLink('/signup', 'Create a business account')));
  S.notice = null;
  form.addEventListener('submit', (e) => {
    e.preventDefault();
    busy(submit, async () => {
      try { await api('POST', '/api/auth/login', { email: email.input.value.trim(), password: pw.input.value }); await enter(); }
      catch (err) { showError(form, err, slot); }
    });
  });
  return form;
}
const withScheme = (v) => (v && !/^https?:\/\//i.test(v) ? 'https://' + v : v);
function SignUpForm() {
  const slot = h('div');
  const name = Field({ name: 'business_name', label: 'Business name', autocomplete: 'organization', maxlength: 120 });
  const site = Field({ name: 'website', label: 'Website', type: 'url', autocomplete: 'url', inputmode: 'url', placeholder: 'acme.co.za' });
  const email = Field({ name: 'email', label: 'Work email', type: 'email', autocomplete: 'email', inputmode: 'email' });
  const pw = PasswordField({ name: 'password', label: 'Password', autocomplete: 'new-password', hint: 'At least 8 characters.' });
  const hints = Field({ name: 'hints', label: 'Anything that helps us research', optional: true, maxlength: 300, placeholder: 'e.g. Catering company in Cape Town' });
  const submit = Btn('Create account', { type: 'submit', block: true });
  const form = h('form', { class: 'form auth-form', novalidate: true },
    h('h1', { class: 'h1', tabindex: '-1', text: 'Create your business account' }),
    h('p', { class: 'lede', text: 'Your business name and website are all we need. We research the rest.' }),
    slot, name.el, site.el, email.el, pw.el, hints.el, submit,
    h('p', { class: 'auth-switch' }, 'Already have an account? ', appLink('/', 'Sign in')));
  form.addEventListener('submit', (e) => {
    e.preventDefault();
    busy(submit, async () => {
      try {
        await api('POST', '/api/auth/signup', { business_name: name.input.value.trim(), website: withScheme(site.input.value.trim()),
          email: email.input.value.trim(), password: pw.input.value, hints: hints.input.value.trim() });
        await enter();
      } catch (err) { showError(form, err, slot); }
    });
  });
  return form;
}
function ForgotForm() {
  const slot = h('div');
  const email = Field({ name: 'email', label: 'Email', type: 'email', autocomplete: 'email', inputmode: 'email' });
  const submit = Btn('Send reset link', { type: 'submit', block: true });
  const form = h('form', { class: 'form auth-form', novalidate: true },
    h('h1', { class: 'h1', tabindex: '-1', text: 'Reset your password' }),
    h('p', { class: 'lede', text: 'Enter the account email and we’ll send a link to choose a new password.' }),
    slot, email.el, submit, h('p', { class: 'auth-switch' }, appLink('/', 'Back to sign in')));
  form.addEventListener('submit', (e) => {
    e.preventDefault();
    busy(submit, async () => {
      try {
        await api('POST', '/api/auth/forgot', { email: email.input.value.trim() });
        form.replaceChildren(h('div', { class: 'auth-sent' },
          h('h1', { class: 'h1', tabindex: '-1', text: 'Check your inbox' }),
          h('p', { class: 'lede', text: 'If there’s an account for that address, a reset link is on its way. It works for one hour.' }),
          h('p', { class: 'auth-switch' }, appLink('/', 'Back to sign in'))));
        $('h1', form).focus();
      } catch (err) { showError(form, err, slot); }
    });
  });
  return form;
}
function ResetForm() {
  const token = new URLSearchParams(location.search).get('token');
  if (!token) {
    return h('div', { class: 'form auth-form' },
      h('h1', { class: 'h1', tabindex: '-1', text: 'This link is incomplete' }),
      h('p', { class: 'lede', text: 'Reset links only work when they’re opened in full. Ask for a new one and use the link in the email.' }),
      appLink('/forgot', 'Send me a new link', 'btn btn-primary btn-block'));
  }
  const slot = h('div');
  const pw = PasswordField({ name: 'password', label: 'New password', autocomplete: 'new-password', hint: 'At least 8 characters.' });
  const submit = Btn('Save and sign in', { type: 'submit', block: true });
  const form = h('form', { class: 'form auth-form', novalidate: true },
    h('h1', { class: 'h1', tabindex: '-1', text: 'Choose a new password' }),
    h('p', { class: 'lede', text: 'You’ll be signed in straight away, and other sessions will be signed out.' }),
    slot, pw.el, submit, h('p', { class: 'auth-switch' }, appLink('/forgot', 'Need a new link?')));
  form.addEventListener('submit', (e) => {
    e.preventDefault();
    busy(submit, async () => {
      try { await api('POST', '/api/auth/reset', { token, password: pw.input.value }); await enter(); }
      catch (err) { showError(form, err, slot); }
    });
  });
  return form;
}

/* ------------------------------------------------------------------ onboarding */

function AppBar(dark) {
  return h('header', { class: 'appbar wrap' }, Wordmark(),
    Btn('Sign out', { kind: dark ? 'glass' : 'quiet', small: true, onClick: signOut }));
}
const bizName = () => (S.me && S.me.user && S.me.user.business_name) || 'your business';
function ResearchScreen() {
  const board = Board(12), feed = Feed(), meter = Meter();
  const el = h('div', { class: 'stage on-dark' },
    h('div', { class: 'media' }, h('div', { class: 'media-gradient' }), h('div', { class: 'media-scrim' })),
    AppBar(true),
    h('main', { class: 'stage-body', id: 'main' },
      h('p', { class: 'eyebrow', text: 'Researching your business' }),
      board.el,
      h('h1', { class: 'stage-title', tabindex: '-1', text: `Getting to know ${bizName()}.` }),
      feed.el, meter.el),
    h('p', { class: 'stage-foot', text: 'This takes about a minute. You’ll review everything before we use it.' }));
  board.cycle([boardWord(bizName()), 'WEBSITE', 'PRODUCTS', 'CUSTOMERS', 'COMPETITORS', 'MARKET', 'REGULATION', 'NEWS'], { every: 2400 });
  const update = () => { const run = S.me.run; feed.update(run); meter.update(run && run.progress ? run.progress.percent : 0); };
  update();
  startPolling();
  return { el, update, destroy: board.stop };
}
const INTEL_TABS = [
  ['overview', 'Overview', (i) => (i.overview && i.overview.summary ? 1 : 0)],
  ['offerings', 'Products & services', (i) => (i.offerings || []).length],
  ['segments', 'Customers', (i) => (i.segments || []).length],
  ['competitors', 'Competitors', (i) => (i.competitors || []).length],
  ['trends', 'Market', (i) => (i.trends || []).length],
  ['regulation', 'Regulation', (i) => (i.regulation || []).length],
  ['news', 'News', (i) => (i.news || []).length],
];
const FOUND = { offerings: ['product or service', 'products & services'], segments: ['customer segment', 'customer segments'], competitors: ['competitor', 'competitors'],
  trends: ['market trend', 'market trends'], regulation: ['regulation', 'regulations'], news: ['news item', 'news items'] };
function ReviewScreen() {
  const p = S.me.profile || {};
  const intel = S.me.intel || {};
  const last = S.me.last_run;
  const failed = last && last.kind === 'research' && last.status === 'failed' ? last.error || 'We couldn’t finish researching your business.' : null;
  const form = ProfileForm(p, {
    submitLabel: 'Looks right, start prospecting',
    note: 'You can change all of this later.',
    onSaved: (r) => {
      Object.assign(S.me, { profile: r.profile, user: r.user, run: r.run });
      S.itemsLoaded = false;
      S.items = []; S.saved = [];
      if (r.run) startPolling();
      go('/', true);
    },
  });
  const hasIntel = intel.overview && intel.overview.summary;
  const el = h('div', { class: 'page profile-page' }, AppBar(),
    h('main', { class: 'page-main', id: 'main' }, h('div', { class: 'wrap' }, h('div', { class: 'pf' },
      h('div', { class: 'pf-head' },
        h('h1', { class: 'h1', tabindex: '-1', text: hasIntel ? 'Does this look right?' : 'Tell us about your business' }),
        h('p', { class: 'lede', text: hasIntel ? `We researched ${bizName()} from public sources. Check the profile, change anything, then we’ll start prospecting.` : 'Fill in the essentials and we’ll take it from there.' }),
        failed ? h('div', { class: 'form-error', role: 'alert' }, icon('alert'), h('span', { text: failed })) : null,
        hasIntel ? FoundNote(intel) : null),
      form))));
  return { el };
}
function confidenceLine(c) {
  c = Math.max(0, Math.min(1, Number(c) || 0));
  const level = c >= 0.75 ? 3 : c >= 0.45 ? 2 : 1;
  const words = ['', 'Low confidence: please check carefully.', 'Medium confidence.', 'High confidence.'][level];
  return h('p', { class: 'confidence' },
    h('span', { class: 'bars', 'aria-hidden': 'true' }, [1, 2, 3].map((i) => h('i', { class: i <= level ? 'on' : '' }))),
    h('span', { text: `${words} ${Math.round(c * 100)}% sure these facts hold.` }));
}
function FoundNote(intel) {
  const o = intel.overview || {};
  const counts = Object.entries(FOUND).map(([k, [one, many]]) => [(intel[k] || []).length, one, many]).filter(([n]) => n);
  return h('section', { class: 'research-note', 'aria-label': 'What research found' },
    h('p', { class: 'note-title', text: 'Here’s what we found. Change anything below, then continue.' }),
    h('blockquote', { text: o.summary }),
    counts.length ? h('ul', { class: 'found' }, counts.map(([n, one, many]) => h('li', null, h('b', { text: String(n) }), ' ', n === 1 ? one : many))) : null,
    confidenceLine(intel.confidence),
    (o.sources || []).length ? h('div', null, h('p', { class: 'hint', text: 'Sources' }), h('ul', { class: 'sources' }, o.sources.slice(0, 8).map((src) => h('li', null, SourceLink(src))))) : null);
}
function Section(num, title, ...kids) {
  return h('section', { class: 'pf-section' },
    h('div', { class: 'pf-section-head' }, h('span', { class: 'pf-num', text: num }), h('h2', { class: 'h2', text: title })), kids);
}
function ProfileForm(p, { submitLabel, note, onSaved }) {
  const M = S.meta;
  const slot = h('div');
  const org = Field({ name: 'organization', label: 'Business name', value: p.organization || bizName(), maxlength: 120, autocomplete: 'organization' });
  const site = Field({ name: 'website', label: 'Website', type: 'url', value: p.website || (S.me.user && S.me.user.website) || '', maxlength: 300, inputmode: 'url' });
  const headline = Field({ name: 'headline', label: 'What does your business do?', value: p.headline, maxlength: 200, placeholder: 'e.g. Catering for weddings and corporate events in Cape Town' });
  const industry = Field({ name: 'industry', label: 'Industry', value: p.industry, maxlength: 80, placeholder: 'e.g. Catering and events' });
  const stage = Picks({ name: 'stage', type: 'radio', label: 'Stage', options: M.stages, value: [p.stage] });
  const location = Field({ name: 'location', label: 'Where are you based?', value: p.location, placeholder: 'City, country', maxlength: 120, autocomplete: 'off' });
  const reach = Picks({ name: 'reach', type: 'radio', label: 'Where do you sell?', options: M.reach, value: [p.reach || 'national'], segment: true });
  const seekHint = 'Pick the areas that matter. Leave them all off and we’ll look across every area.';
  const seeking = Picks({ name: 'seeking', type: 'checkbox', label: 'Focus areas', options: M.opportunity_types, value: p.seeking || [], hint: seekHint, liveHint: true });
  const interests = TagInput({ name: 'interests', label: 'Markets and topics to watch', values: p.interests, placeholder: 'Type one and press Enter', hint: 'Segments, regions or themes, like corporate events or export to the EU.' });
  const skills = TagInput({ name: 'skills', label: 'Products, services and strengths', values: p.skills, placeholder: 'e.g. event catering' });
  const goals = Field({ name: 'goals', label: 'Goals for the next year', textarea: true, rows: 3, value: p.goals, maxlength: 1000, placeholder: 'e.g. Win more corporate contracts and open a second kitchen' });
  const avoid = Field({ name: 'avoid', label: 'Anything to avoid?', optional: true, value: p.avoid, maxlength: 600, placeholder: 'e.g. Nothing that needs a liquor licence' });
  const links = TagInput({ name: 'links', label: 'Other links', optional: true, values: p.links, max: 10, maxLen: 300, normalize: normUrl, placeholder: 'LinkedIn, catalogue or case studies' });
  const langs = TagInput({ name: 'languages', label: 'Languages you work in', optional: true, values: p.languages, max: 8, maxLen: 40, placeholder: 'e.g. English, isiXhosa' });
  const submit = Btn(submitLabel, { type: 'submit' });
  const more = h('details', { open: !!((p.links && p.links.length) || (p.languages && p.languages.length)) },
    h('summary', null, 'More details', h('span', { class: 'optional', text: ' (optional)' }), icon('chevron')),
    h('div', null, links.el, langs.el));
  const form = h('form', { class: 'pf-form', novalidate: true }, slot,
    Section('01', 'Your business', h('div', { class: 'row-2' }, org.el, site.el), headline.el, h('div', { class: 'row-2' }, industry.el, location.el), stage.el, reach.el),
    Section('02', 'What to look for', seeking.el, interests.el, skills.el),
    Section('03', 'Goals', goals.el, avoid.el, more),
    h('div', { class: 'pf-foot' }, h('p', { class: 'hint', text: note }), submit));
  form.addEventListener('submit', (e) => {
    e.preventDefault();
    busy(submit, async () => {
      const profile = {
        organization: org.input.value.trim(), website: withScheme(site.input.value.trim()), headline: headline.input.value.trim(),
        industry: industry.input.value.trim(), stage: stage.value[0] || '', location: location.input.value.trim(), reach: reach.value[0] || '',
        seeking: seeking.value, interests: interests.value, skills: skills.value, goals: goals.input.value.trim(), avoid: avoid.input.value.trim(),
        links: links.value, languages: langs.value, research: p.research || null,
      };
      try { onSaved(await api('PUT', '/api/profile', { profile })); }
      catch (err) { showError(form, err, slot); if (!err.field) slot.scrollIntoView({ block: 'center', behavior: reduced.matches ? 'auto' : 'smooth' }); }
    });
  });
  return form;
}

/* ------------------------------------------------------------------ business intelligence tabs */

function fmtDate(ymd, short) {
  const m = /^(\d{4})-(\d{2})-(\d{2})/.exec(ymd || '');
  if (!m) return '';
  return new Date(+m[1], +m[2] - 1, +m[3]).toLocaleDateString('en-GB', { day: 'numeric', month: short ? 'short' : 'long', year: 'numeric' });
}
function SourceLink(src) {
  if (!src || !safeHref(src.url)) return null;
  return h('span', { class: 'src' }, outLink(src.url, [h('span', { text: src.title || hostOf(src.url) }), icon('out')]),
    src.date ? h('span', { class: 'src-date', text: fmtDate(src.date, true) }) : null);
}
const srcFoot = (src) => { const l = SourceLink(src); return l ? h('footer', { class: 'icard-foot' }, l) : null; };
const pill = (text, kind) => h('span', { class: `pill pill-${kind}`, text });

function IntelPanel(tab, { onTab }) {
  const intel = S.me.intel || {};
  const panels = { overview: OverviewPanel, offerings: OfferingsPanel, segments: SegmentsPanel, competitors: CompetitorsPanel, trends: MarketPanel, regulation: RegulationPanel, news: NewsPanel };
  return (panels[tab] || OverviewPanel)(intel, onTab);
}
function OverviewPanel(intel, onTab) {
  const o = intel.overview || {};
  const facts = [['Industry', o.industry], ['Headquarters', o.headquarters], ['Founded', o.founded], ['Size', o.size], ['Business model', o.business_model]].filter(([, v]) => v);
  const counts = INTEL_TABS.slice(1).map(([id, label, n]) => [id, label, n(intel)]).filter(([, , n]) => n);
  return h('div', { class: 'intel' },
    h('section', { class: 'lead-card' },
      h('p', { class: 'eyebrow', text: 'At a glance' }),
      h('p', { class: 'lead-text', text: o.summary }),
      (o.sources || []).length ? h('div', { class: 'lead-sources' }, o.sources.slice(0, 4).map(SourceLink)) : null),
    facts.length || (o.markets || []).length ? h('div', { class: 'snapshot' },
      facts.map(([k, v]) => h('div', { class: 'snap' }, h('p', { class: 'snap-k', text: k }), h('p', { class: 'snap-v', text: v }))),
      (o.markets || []).length ? h('div', { class: 'snap' }, h('p', { class: 'snap-k', text: 'Markets' }), h('ul', { class: 'snap-tags' }, o.markets.map((m) => h('li', { text: m })))) : null) : null,
    counts.length ? h('div', null, h('h3', { class: 'set-title intel-sub', text: 'What we found' }),
      h('div', { class: 'jump' }, counts.map(([id, label, n]) => h('button', { type: 'button', class: 'jump-card', on: { click: () => onTab(id) } },
        h('b', { text: String(n) }), h('span', { text: label }), icon('arrow'))))) : null);
}
function OfferingsPanel(intel) {
  return h('div', { class: 'igrid' }, intel.offerings.map((x) => h('article', { class: 'icard' },
    h('h3', { class: 'icard-title', text: x.name }),
    x.description ? h('p', { class: 'icard-text', text: x.description }) : null,
    h('div', { class: 'icard-meta' },
      x.audience ? h('span', { class: 'fact' }, icon('pin'), h('span', { text: `For ${x.audience.charAt(0).toLowerCase()}${x.audience.slice(1)}` })) : null,
      x.pricing ? h('span', { class: 'fact' }, icon('value'), h('span', { text: x.pricing })) : null),
    srcFoot(x.source))));
}
function SegmentsPanel(intel) {
  return h('div', { class: 'igrid' }, intel.segments.map((x) => h('article', { class: 'icard' },
    h('div', { class: 'seg-icon', 'aria-hidden': 'true', text: (x.name || '?').trim()[0].toUpperCase() }),
    h('h3', { class: 'icard-title', text: x.name }),
    x.description ? h('p', { class: 'icard-text', text: x.description }) : null,
    (x.examples || []).length ? h('div', { class: 'examples' }, h('p', { class: 'snap-k', text: 'Named customers' }),
      h('ul', { class: 'snap-tags' }, x.examples.map((e) => h('li', { text: e })))) : null,
    srcFoot(x.source))));
}
const OVERLAP = { high: ['High overlap', 'hi', 0], medium: ['Medium overlap', 'mid', 1], low: ['Low overlap', 'lo', 2] };
function Bullseye(comps) {
  const NS = SVGNS, mk = (tag, attrs, text) => { const e = doc.createElementNS(NS, tag); for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, String(v)); if (text) e.textContent = text; return e; };
  const svg = mk('svg', { viewBox: '0 0 520 340', class: 'bullseye', role: 'img', 'aria-label': 'Competitors placed by how closely they overlap with your business' });
  const cx = 260, cy = 170, radii = [62, 112, 158];
  [2, 1, 0].forEach((i) => svg.append(mk('circle', { cx, cy, r: radii[i], class: `ring ring-${i}` })));
  ['High', 'Medium', 'Low'].forEach((t, i) => svg.append(mk('text', { x: cx, y: cy - radii[i] + 15, class: 'ring-label', 'text-anchor': 'middle' }, t)));
  svg.append(mk('circle', { cx, cy, r: 30, class: 'you' }));
  svg.append(mk('text', { x: cx, y: cy + 5, class: 'you-label', 'text-anchor': 'middle' }, 'You'));
  const byRing = [[], [], []];
  for (const c of comps) byRing[(OVERLAP[c.overlap] || OVERLAP.low)[2]].push(c);
  byRing.forEach((list, ring) => list.forEach((c, k) => {
    const a = (-150 + ring * 37 + (360 / Math.max(list.length, 1)) * k) * Math.PI / 180;
    const r = ring === 0 ? 46 + 16 : radii[ring] - 22;
    const x = cx + Math.cos(a) * r, y = cy + Math.sin(a) * r * 0.92;
    svg.append(mk('circle', { cx: x, cy: y, r: 7, class: `dot dot-${ring}` }));
    const right = Math.cos(a) >= 0;
    svg.append(mk('text', { x: x + (right ? 13 : -13), y: y + 4, class: 'dot-label', 'text-anchor': right ? 'start' : 'end' }, c.name.length > 22 ? c.name.slice(0, 21) + '…' : c.name));
  }));
  return svg;
}
function CompetitorsPanel(intel) {
  const comps = intel.competitors;
  const tally = ['high', 'medium', 'low'].map((k) => [k, comps.filter((c) => (OVERLAP[c.overlap] ? c.overlap : 'low') === k).length]).filter(([, n]) => n);
  return h('div', { class: 'intel' },
    h('section', { class: 'sit' },
      h('div', { class: 'sit-text' },
        h('p', { class: 'eyebrow', text: 'Where you sit' }),
        h('h3', { class: 'h2', text: `${plural(comps.length, 'competitor', 'competitors')}, by how closely they overlap with you` }),
        h('p', { class: 'icard-text', text: 'The closer to the centre, the more they compete for the same customers with a similar offer.' }),
        h('ul', { class: 'sit-tally' }, tally.map(([k, n]) => h('li', null, h('i', { class: `key key-${OVERLAP[k][1]}` }), h('b', { text: String(n) }), ` ${OVERLAP[k][0].toLowerCase()}`)))),
      Bullseye(comps)),
    h('div', { class: 'igrid' }, comps.map((c) => {
      const ov = OVERLAP[c.overlap] || OVERLAP.low;
      return h('article', { class: 'icard' },
        h('div', { class: 'icard-row' }, h('h3', { class: 'icard-title', text: c.name }), pill(ov[0], ov[1])),
        safeHref(c.website) ? outLink(c.website, [hostOf(c.website), icon('out')], 'icard-link') : null,
        c.summary ? h('p', { class: 'icard-text', text: c.summary }) : null,
        c.difference ? h('div', { class: 'diff' }, h('p', { class: 'snap-k', text: 'How you differ' }), h('p', { text: c.difference })) : null,
        srcFoot(c.source));
    })));
}
const IMPACT = { tailwind: ['Tailwind', 'good', 'up'], headwind: ['Headwind', 'bad', 'down'], mixed: ['Mixed', 'mid', 'steer'] };
function MarketPanel(intel) {
  const trends = intel.trends;
  const n = (k) => trends.filter((t) => (IMPACT[t.impact] ? t.impact : 'mixed') === k).length;
  const parts = ['tailwind', 'headwind', 'mixed'].filter((k) => n(k));
  const bar = h('div', { class: 'balance', 'aria-hidden': 'true' }, parts.map((k) => { const s = h('i', { class: `bal-${IMPACT[k][1]}` }); s.style.setProperty('flex-grow', String(n(k))); return s; }));
  return h('div', { class: 'intel' },
    h('section', { class: 'balance-card' },
      h('p', { class: 'eyebrow', text: 'Market balance' }),
      h('p', { class: 'balance-text' }, parts.map((k, i) => [i ? ' · ' : '', h('b', { text: String(n(k)) }), ` ${IMPACT[k][0].toLowerCase()}${n(k) === 1 || k === 'mixed' ? '' : 's'}`])),
      bar),
    h('div', { class: 'igrid' }, trends.map((t) => {
      const im = IMPACT[t.impact] || IMPACT.mixed;
      return h('article', { class: `icard trend trend-${im[1]}` },
        h('div', { class: 'icard-row' }, h('span', { class: `impact impact-${im[1]}` }, icon(im[2]), im[0])),
        h('h3', { class: 'icard-title', text: t.title }),
        t.summary ? h('p', { class: 'icard-text', text: t.summary }) : null,
        srcFoot(t.source));
    })));
}
const STATUS = { proposed: 'Proposed', consultation: 'In consultation', passed: 'Passed', 'in force': 'In force' };
const REG_IMPACT = { risk: ['Risk', 'bad'], opportunity: ['Opportunity', 'good'], both: ['Risk and opportunity', 'mid'] };
function whenText(ymd) {
  const d = deadlineInfo(ymd);
  if (!d) return '';
  const m = /^(\d{4})-(\d{2})-(\d{2})/.exec(ymd);
  const days = Math.round((new Date(+m[1], +m[2] - 1, +m[3]) - new Date().setHours(0, 0, 0, 0)) / 86400000);
  return days > 0 ? `Takes effect in ${days} day${days === 1 ? '' : 's'}` : days === 0 ? 'Takes effect today' : 'Already in effect';
}
function RegulationPanel(intel) {
  const regs = intel.regulation.slice().sort((a, b) => (a.effective ? 0 : 1) - (b.effective ? 0 : 1) || String(a.effective).localeCompare(String(b.effective)));
  return h('ol', { class: 'timeline' }, regs.map((r) => {
    const im = REG_IMPACT[r.impact] || REG_IMPACT.both;
    return h('li', { class: 'tl-item' },
      h('div', { class: 'tl-when' }, h('b', { text: r.effective ? fmtDate(r.effective, true) : 'Date not set' }), h('span', { text: r.effective ? whenText(r.effective) : STATUS[r.status] || '' })),
      h('div', { class: `tl-node tl-${(r.status || '').replace(' ', '-')}`, 'aria-hidden': 'true' }),
      h('article', { class: 'icard tl-card' },
        h('div', { class: 'icard-row tl-pills' }, pill(STATUS[r.status] || r.status || 'Status unknown', 'status'), pill(im[0], im[1]), r.jurisdiction ? h('span', { class: 'tl-place' }, icon('pin'), r.jurisdiction) : null),
        h('h3', { class: 'icard-title', text: r.title }),
        r.summary ? h('p', { class: 'icard-text', text: r.summary }) : null,
        srcFoot(r.source)));
  }));
}
function NewsPanel(intel) {
  const news = intel.news.slice().sort((a, b) => String((b.source || {}).date || '').localeCompare(String((a.source || {}).date || '')));
  return h('ol', { class: 'timeline timeline-news' }, news.map((n) => {
    const d = (n.source || {}).date;
    const m = /^(\d{4})-(\d{2})-(\d{2})/.exec(d || '');
    const dt = m ? new Date(+m[1], +m[2] - 1, +m[3]) : null;
    return h('li', { class: 'tl-item' },
      h('div', { class: 'tl-when' }, dt ? [h('b', { text: dt.toLocaleDateString('en-GB', { day: 'numeric', month: 'short' }) }), h('span', { text: String(dt.getFullYear()) })] : h('b', { text: 'Undated' })),
      h('div', { class: 'tl-node', 'aria-hidden': 'true' }),
      h('article', { class: 'icard tl-card' },
        h('h3', { class: 'icard-title', text: n.title }),
        n.summary ? h('p', { class: 'icard-text', text: n.summary }) : null,
        srcFoot(n.source)));
  }));
}

/* ------------------------------------------------------------------ AI model + connectors */

function modelLabel(ai) {
  ai = ai || (S.me && S.me.ai) || {};
  if (ai.provider === 'bedrock') return 'Claude on Bedrock';
  const m = (S.meta.ai_models || []).find((x) => x.provider === ai.provider && x.id === ai.model);
  return m ? m.label : 'DeepSeek Flash';
}
function openModelPanel() {
  const ai = S.me.ai || { provider: 'deepseek', model: 'deepseek-flash', bedrock: null };
  const b = ai.bedrock || {};
  const models = S.meta.ai_models || [];
  const name = uid('model');
  const slot = h('div');
  const result = h('div', { class: 'test-result', role: 'status' });
  const keyOf = (m) => `${m.provider}:${m.id}`;
  const current = ai.provider === 'bedrock' ? 'bedrock:' : `deepseek:${ai.model}`;
  const radios = models.map((m) => h('input', { type: 'radio', name, value: keyOf(m), checked: keyOf(m) === current }));
  const region = Field({ name: 'region', label: 'Region', value: b.region || 'us-east-1', maxlength: 30, placeholder: 'us-east-1' });
  const model = Field({ name: 'model', label: 'Model ID or inference profile ARN', value: b.model || '', maxlength: 300, placeholder: 'anthropic.claude-… or arn:aws:bedrock:…' });
  const authName = uid('auth');
  const authApi = h('input', { type: 'radio', name: authName, value: 'api_key', checked: b.auth !== 'access_keys' });
  const authKeys = h('input', { type: 'radio', name: authName, value: 'access_keys', checked: b.auth === 'access_keys' });
  const keep = b.key_hint ? `Saved: ${b.key_hint}. Leave blank to keep it.` : null;
  const apiKey = PasswordField({ name: 'api_key', label: 'Bedrock API key', autocomplete: 'off', hint: b.auth !== 'access_keys' ? keep : null });
  const akid = Field({ name: 'access_key_id', label: 'Access key ID', autocomplete: 'off', hint: b.auth === 'access_keys' ? keep : null });
  const secret = PasswordField({ name: 'secret_access_key', label: 'Secret access key', autocomplete: 'off' });
  const session = PasswordField({ name: 'session_token', label: 'Session token', optional: true, autocomplete: 'off' });
  const apiBox = h('div', { class: 'form' }, apiKey.el);
  const keysBox = h('div', { class: 'form' }, akid.el, secret.el, session.el);
  const bedrockBox = h('div', { class: 'set-form' }, region.el, model.el,
    h('fieldset', { class: 'field' }, h('legend', { class: 'label', text: 'Sign in with' }),
      h('div', { class: 'segment' }, h('label', { class: 'pick' }, authApi, h('span', { text: 'API key' })), h('label', { class: 'pick' }, authKeys, h('span', { text: 'Access keys' })))),
    apiBox, keysBox);
  const sync = () => {
    const pick = radios.find((r) => r.checked);
    bedrockBox.hidden = !pick || !pick.value.startsWith('bedrock');
    apiBox.hidden = !authApi.checked;
    keysBox.hidden = authApi.checked;
  };
  [...radios, authApi, authKeys].forEach((r) => r.addEventListener('change', () => { sync(); result.replaceChildren(); }));
  sync();
  const save = Btn('Save', { type: 'submit' });
  const test = Btn('Test connection', { kind: 'secondary' });
  const form = h('form', { class: 'form', novalidate: true },
    h('p', { class: 'hint', text: 'This model plans, judges and writes every run. Web search always uses DeepSeek’s Search API.' }),
    slot,
    h('div', { class: 'radios' }, models.map((m, i) => h('label', { class: 'radio radio-tall' }, radios[i],
      h('span', null, h('b', { text: m.label }), h('small', { text: m.note }))))),
    bedrockBox, result,
    h('div', { class: 'dialog-actions' }, test, save));
  async function persist() {
    const pick = radios.find((r) => r.checked);
    if (!pick) throw new ApiError(422, 'validation', 'Choose a model.', 'provider');
    let body;
    if (pick.value.startsWith('bedrock')) {
      const bed = { region: region.input.value.trim(), model: model.input.value.trim() };
      if (authApi.checked) { if (apiKey.input.value.trim()) bed.api_key = apiKey.input.value.trim(); }
      else {
        if (akid.input.value.trim()) bed.access_key_id = akid.input.value.trim();
        if (secret.input.value.trim()) bed.secret_access_key = secret.input.value.trim();
        if (session.input.value.trim()) bed.session_token = session.input.value.trim();
      }
      body = { provider: 'bedrock', bedrock: bed };
    } else body = { provider: 'deepseek', model: pick.value.split(':')[1] };
    const r = await api('PUT', '/api/settings/ai', body);
    S.me.ai = r;
    [apiKey, akid, secret, session].forEach((f) => { f.input.value = ''; });
    document.querySelectorAll('.model-chip-label').forEach((x) => { x.textContent = modelLabel(r); });
    return r;
  }
  const fail = (err) => { if (err.field) err.field = err.field.split('.').pop(); showError(form, err, slot); };
  form.addEventListener('submit', (e) => {
    e.preventDefault();
    busy(save, async () => {
      try { await persist(); clearErrors(form); dlg.close(); toast(`Runs now use ${modelLabel()}.`); } catch (err) { fail(err); }
    });
  });
  test.addEventListener('click', () => busy(test, async () => {
    result.replaceChildren();
    try {
      await persist();
      clearErrors(form);
      const r = await api('POST', '/api/settings/ai/test', {});
      result.replaceChildren(h('p', { class: r.ok ? 'test-ok' : 'form-error' }, icon(r.ok ? 'verified' : 'alert'), h('span', { text: r.message || (r.ok ? 'Connected.' : 'That didn’t work.') })));
    } catch (err) { fail(err); }
  }));
  const dlg = openDialog({ cls: 'modal modal-sm', title: 'AI model', body: form });
}

const CONN_ICON = { slack: 'S', microsoft: 'M', confluence: 'C' };
const FROM = { slack: 'From your Slack', microsoft: 'From your Outlook', confluence: 'From your Confluence' };
function ConnectorsSection() {
  const list = h('div', { class: 'conns' }, h('div', { class: 'skeleton skeleton-sm' }));
  const log = h('div', { hidden: true });
  const logBtn = h('button', { type: 'button', class: 'set-link', 'aria-expanded': 'false' }, icon('clock'), h('span', { text: 'Access log' }));
  logBtn.addEventListener('click', async () => {
    const open = log.hidden;
    log.hidden = !open;
    logBtn.setAttribute('aria-expanded', String(open));
    if (!open) return;
    log.replaceChildren(h('p', { class: 'hint', text: 'Loading…' }));
    try {
      const r = await api('GET', '/api/connectors/audit?limit=100');
      const ev = r.events || [];
      log.replaceChildren(ev.length ? h('ol', { class: 'audit' }, ev.map((e) => h('li', { class: `audit-${e.action}` },
        h('div', { class: 'audit-top' }, h('b', { text: `${(e.method || '').toUpperCase()} ${e.action === 'blocked' ? 'blocked' : e.status || ''}`.trim() }), h('span', { text: relTime(e.at) })),
        h('p', { class: 'audit-target', text: e.target }),
        e.note ? h('p', { class: 'hint', text: e.note }) : null)))
        : h('p', { class: 'hint', text: 'Nothing has been read yet. Every request a connector makes will be listed here.' }));
    } catch (err) { log.replaceChildren(h('p', { class: 'form-error' }, icon('alert'), h('span', { text: err.message }))); }
  });
  async function load() {
    try {
      const r = await api('GET', '/api/connectors');
      const conns = new Map((r.connections || []).map((c) => [c.provider, c]));
      list.replaceChildren(...(r.available || []).map((a) => ConnectorCard(a, conns.get(a.provider), load)));
    } catch (err) { list.replaceChildren(h('p', { class: 'form-error' }, icon('alert'), h('span', { text: err.message }))); }
  }
  load();
  return h('section', { class: 'set-section' },
    h('h3', { class: 'set-title', text: 'Connected tools' }),
    h('p', { class: 'hint', text: 'Let prospecting read your own tools for internal opportunities. Access is read-only, and every request is logged.' }),
    list, h('div', { class: 'set-links' }, logBtn), log);
}
function ConnectorCard(a, conn, reload) {
  const head = h('div', { class: 'conn-head' }, h('span', { class: `conn-icon conn-${a.provider}`, 'aria-hidden': 'true', text: CONN_ICON[a.provider] || a.label[0] }),
    h('div', null, h('b', { text: a.label }), h('p', { class: 'hint', text: a.reads })));
  const body = h('div', { class: 'conn-body' });
  const card = h('article', { class: 'conn' + (conn ? ' is-on' : '') }, head, body);
  if (conn) {
    const acct = conn.account || {};
    const badge = conn.enforcement === 'app' ? 'Read-only · enforced by Prospect' : 'Read-only · scopes verified';
    const off = Btn('Disconnect', { kind: 'quiet', small: true });
    off.addEventListener('click', () => busy(off, async () => {
      try { await api('DELETE', `/api/connectors/${a.provider}`, {}); toast(`${a.label} disconnected. Its credentials are deleted.`); reload(); }
      catch (err) { toast(err.message, { error: true }); }
    }));
    add(body, [
      h('p', { class: 'conn-acct' }, icon('verified'), h('span', { text: [acct.name, acct.user].filter(Boolean).join(' · ') || 'Connected' })),
      h('p', { class: 'ro-badge' }, icon('eye'), badge),
      conn.status !== 'ok' ? h('p', { class: 'form-error' }, icon('alert'), h('span', { text: conn.error || (conn.status === 'needs_reconnect' ? 'This connection needs to be reconnected.' : 'This connection has a problem.') })) : null,
      h('div', { class: 'conn-actions' }, h('span', { class: 'hint', text: conn.last_used_at ? `Last read ${relTime(conn.last_used_at)}` : `Connected ${relTime(conn.connected_at)}` }), off)]);
    return card;
  }
  if (a.oauth) {
    const go2 = Btn(`Connect ${a.label}`, { kind: 'secondary', small: true });
    go2.addEventListener('click', () => busy(go2, async () => {
      try { const r = await api('POST', `/api/connectors/${a.provider}/start`, {}); if (r && safeHref(new URL(r.url, location.href).href)) location.href = r.url; }
      catch (err) { toast(err.message, { error: true }); }
    }));
    body.append(h('div', { class: 'conn-actions' }, h('span', { class: 'hint', text: 'You’ll sign in and approve read-only access.' }), go2));
  } else if (a.token) {
    const slot = h('div');
    const fields = a.provider === 'confluence'
      ? [Field({ name: 'site', label: 'Confluence site', placeholder: 'acme.atlassian.net' }), Field({ name: 'email', label: 'Atlassian account email', type: 'email' }), PasswordField({ name: 'token', label: 'API token', autocomplete: 'off' })]
      : [PasswordField({ name: 'token', label: `${a.label} token`, autocomplete: 'off', hint: a.provider === 'slack' ? 'A user token (xoxp-…) with search:read. Tokens that can write are refused.' : null })];
    const submit = Btn('Connect', { type: 'submit', small: true });
    const form = h('form', { class: 'set-form', hidden: true, novalidate: true }, slot, fields.map((f) => f.el), h('div', { class: 'actions' }, submit));
    const open = Btn('Connect with a token', { kind: 'secondary', small: true });
    open.addEventListener('click', () => { form.hidden = !form.hidden; if (!form.hidden) fields[0].input.focus(); });
    form.addEventListener('submit', (e) => {
      e.preventDefault();
      busy(submit, async () => {
        const body2 = {};
        fields.forEach((f) => { body2[f.input.name] = f.input.value.trim(); });
        if (body2.site) body2.site = withScheme(body2.site);
        try { await api('POST', `/api/connectors/${a.provider}/token`, body2); toast(`${a.label} is connected, read-only.`); reload(); }
        catch (err) { showError(form, err, slot); }
      });
    });
    body.append(h('div', { class: 'conn-actions' }, h('span', { class: 'scopes', text: (a.scopes || []).join(' · ') }), open), form);
  } else {
    body.append(h('p', { class: 'hint conn-admin' }, 'Your administrator can enable this.'));
  }
  return card;
}

/* ------------------------------------------------------------------ dashboard */

function DashboardScreen() {
  const media = Media();
  const board = Board(12), feed = Feed(), meter = Meter();
  const avatar = h('button', { type: 'button', class: 'avatar', 'aria-label': 'Settings and account', 'aria-haspopup': 'dialog', text: initials(S.me.user) });
  avatar.addEventListener('click', openSettings);
  const modelChip = h('button', { type: 'button', class: 'model-chip', 'aria-haspopup': 'dialog', title: 'AI model' },
    h('span', { class: 'model-dot', 'aria-hidden': 'true' }), h('span', { class: 'sr-only', text: 'AI model: ' }), h('span', { class: 'model-chip-label', text: modelLabel() }), icon('chevron'));
  modelChip.addEventListener('click', openModelPanel);
  const eyebrow = h('p', { class: 'eyebrow' });
  const title = h('h1', { class: 'display', tabindex: '-1' });
  const summary = h('p', { class: 'hero-summary' });
  const refreshBtn = Btn('Prospect again', { kind: 'glass', icon: 'refresh', onClick: refreshNow });
  const heroInner = h('div', { class: 'hero-inner wrap' }, eyebrow, title, summary, h('div', { class: 'hero-actions' }, refreshBtn));
  const stageTitle = h('h1', { class: 'stage-title', tabindex: '-1' });
  const stage = h('div', { class: 'hero-stage' }, h('p', { class: 'eyebrow', text: 'Prospecting' }), board.el, stageTitle, feed.el, meter.el,
    h('p', { class: 'stage-foot', text: 'This usually takes a minute or two. You can close this tab; we’ll keep going.' }));
  const credit = h('a', { class: 'credit', target: '_blank', rel: 'noopener noreferrer', hidden: true });
  const hero = h('header', { class: 'hero on-dark' }, media.el, h('div', { class: 'appbar wrap' }, Wordmark(), h('div', { class: 'appbar-actions' }, modelChip, avatar)), heroInner, stage, credit);

  const viewAll = h('button', { type: 'button', 'aria-pressed': 'true', on: { click: () => setView('all') } }, 'For you', h('span', { class: 'count' }));
  const viewSaved = h('button', { type: 'button', 'aria-pressed': 'false', on: { click: () => setView('saved') } }, 'Saved', h('span', { class: 'count' }));
  const steerId = uid('steer');
  const steerBtn = Btn('Steer', { kind: 'quiet', icon: 'steer', small: true, attrs: { 'aria-expanded': 'false', 'aria-controls': steerId }, onClick: () => toggleSteer() });
  const toolbar = h('div', { class: 'toolbar' }, h('div', { class: 'views', role: 'group', 'aria-label': 'Which opportunities' }, viewAll, viewSaved), h('div', { class: 'toolbar-actions' }, steerBtn));
  const chips = h('div', { class: 'chips', role: 'group', 'aria-label': 'Filter by kind' });
  const refreshSlot = h('div');
  const steerSlot = h('div', { id: steerId });
  const stateSlot = h('div');
  const list = h('div', { class: 'list' });
  const foot = h('footer', { class: 'foot wrap' },
    h('p', { text: 'Prospect researches public sources and the tools you connect, read-only. Every finding links to its source; check the details there before you act.' }),
    h('p', null, h('button', { type: 'button', text: 'Settings', on: { click: openSettings } }), ' · ', h('button', { type: 'button', text: 'Sign out', on: { click: signOut } })));
  const tabsEl = h('div', { class: 'tabs', role: 'tablist', 'aria-label': 'Sections' });
  const oppEl = h('div', { role: 'tabpanel', 'aria-labelledby': 'tab-opportunities' }, toolbar, chips, steerSlot, stateSlot, list);
  const intelEl = h('div', { role: 'tabpanel', hidden: true });
  const sheet = h('main', { class: 'sheet', id: 'main' }, h('div', { class: 'wrap' }, tabsEl, refreshSlot, oppEl, intelEl));
  tabsEl.addEventListener('keydown', (e) => {
    if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(e.key)) return;
    const tabs = [...tabsEl.children];
    let i = tabs.indexOf(doc.activeElement);
    if (i < 0) return;
    i = e.key === 'Home' ? 0 : e.key === 'End' ? tabs.length - 1 : (i + (e.key === 'ArrowRight' ? 1 : -1) + tabs.length) % tabs.length;
    e.preventDefault();
    tabs[i].focus();
    tabs[i].click();
  });
  const el = h('div', { class: 'dash' }, hero, sheet, foot);

  let phase = null, finishing = false, refreshBar = null, refreshMeter = null, refreshMsg = null, refreshLabel = null, steerOpen = false;

  const viewItems = () => (S.view === 'saved' ? S.saved : S.items);
  function computePhase() {
    if (S.items.length) return 'ready';
    const run = S.me.run;
    if (run && run.kind === 'discover') return 'loading';
    const last = S.me.last_run;
    if (last && last.kind === 'discover' && last.status === 'failed') return 'failed';
    return 'empty';
  }

  function renderHero() {
    const theme = S.me.theme || {};
    media.set(photoUrl(theme));
    const href = safeHref(theme.source_url);
    credit.hidden = !theme.credit || !photoUrl(theme);
    credit.textContent = theme.credit || '';
    if (href) credit.setAttribute('href', href); else credit.removeAttribute('href');
    hero.classList.toggle('is-stage', phase === 'loading');
    el.classList.toggle('is-loading', phase === 'loading');
    if (phase === 'loading') {
      stageTitle.textContent = `Prospecting for ${bizName()}.`;
      const run = S.me.run;
      if (!finishing) { feed.update(run); meter.update(run && run.progress ? run.progress.percent : 0); }
      return;
    }
    eyebrow.textContent = (S.me.profile && S.me.profile.industry) || theme.label || 'Your business';
    title.textContent = bizName();
    summary.replaceChildren(...summaryParts());
    const running = S.me.run && S.me.run.kind === 'discover';
    setLabel(refreshBtn, running ? 'Prospecting…' : phase === 'failed' ? 'Try again' : 'Prospect again');
    refreshBtn.disabled = !!running;
  }
  function summaryParts() {
    const n = S.items.length;
    const out = [];
    const sep = () => out.push(' · ');
    if (n) {
      out.push(h('b', { text: plural(n, 'opportunity', 'opportunities') }), ' for your business');
      const fresh = S.items.filter((i) => i.is_new).length;
      if (fresh && fresh < n) { sep(); out.push(h('b', { text: `${fresh} new` })); }
    } else out.push(phase === 'failed' ? 'Your first search didn’t finish' : 'Nothing to show just yet');
    if (S.lastRefresh) { sep(); out.push(`refreshed ${relTime(S.lastRefresh)}`); }
    if (S.nextRefresh) { sep(); out.push(`next refresh ${relTime(S.nextRefresh)}`); }
    else if (S.me.settings && S.me.settings.frequency === 'off') { sep(); out.push('refreshes when you ask'); }
    return out;
  }

  function renderTabs() {
    const intel = S.me.intel || {};
    const avail = [['opportunities', 'Opportunities', S.items.length], ...INTEL_TABS.map(([id, label, n]) => [id, label, n(intel)]).filter(([, , n]) => n)];
    if (!avail.some(([id]) => id === S.tab)) S.tab = 'opportunities';
    tabsEl.replaceChildren(...avail.map(([id, label, n]) => {
      const sel = S.tab === id;
      return h('button', { type: 'button', role: 'tab', class: 'tab', id: 'tab-' + id, 'aria-selected': String(sel), tabindex: sel ? '0' : '-1', on: { click: () => setTab(id) } },
        label, id === 'overview' ? null : h('span', { class: 'count', text: String(n) }));
    }));
    tabsEl.hidden = avail.length < 2;
  }
  function setTab(id) {
    if (S.tab === id) return;
    S.tab = id;
    if (id !== 'opportunities') S.view = 'all';
    history.pushState(null, '', id === 'opportunities' ? '/' : `/?tab=${encodeURIComponent(id)}`);
    renderSheet();
    const t = $('#tab-' + id, tabsEl);
    if (t) t.focus({ preventScroll: true });
  }
  async function refreshIntelligence() {
    try {
      const r = await api('POST', '/api/intel/refresh', {});
      S.me.run = r.run;
      startPolling();
      render();
      toast('Researching your business again. The tabs update when it’s done.');
    } catch (err) { toast(err.message, { error: true }); }
  }
  function renderIntel() {
    const intel = S.me.intel || {};
    const busyRun = !!S.me.run;
    const btn = Btn(S.me.run && S.me.run.kind === 'research' ? 'Researching…' : 'Refresh intelligence', { kind: 'secondary', small: true, icon: 'refresh', onClick: refreshIntelligence });
    btn.disabled = busyRun;
    const tab = INTEL_TABS.find(([id]) => id === S.tab);
    const meta = [intel.updated_at ? `Researched ${relTime(intel.updated_at)}` : '', intel.confidence ? `${Math.round(intel.confidence * 100)}% confidence` : ''].filter(Boolean).join(' · ');
    intelEl.setAttribute('aria-labelledby', 'tab-' + S.tab);
    intelEl.replaceChildren(h('div', { class: 'intel-head' }, h('div', null, h('h2', { class: 'h2', text: tab ? tab[1] : 'Overview' }), meta ? h('p', { class: 'hint', text: meta }) : null), btn),
      IntelPanel(S.tab, { onTab: setTab }));
  }
  function renderSheet(animate) {
    renderTabs();
    const intelTab = S.tab !== 'opportunities';
    oppEl.hidden = intelTab;
    intelEl.hidden = !intelTab;
    if (intelTab) { renderIntel(); return; }
    const ready = phase === 'ready';
    toolbar.hidden = chips.hidden = !ready;
    viewAll.setAttribute('aria-pressed', String(S.view === 'all'));
    viewSaved.setAttribute('aria-pressed', String(S.view === 'saved'));
    viewAll.lastChild.textContent = String(S.items.length);
    viewSaved.lastChild.textContent = String(S.saved.length);
    if (!ready) {
      list.replaceChildren();
      steerSlot.replaceChildren();
      steerOpen = false;
      stateSlot.replaceChildren(phase === 'loading' ? '' : phase === 'failed' ? FailedState() : EmptyState());
      return;
    }
    renderChips();
    renderList(animate);
  }
  function renderChips() {
    const items = viewItems();
    const counts = new Map();
    for (const it of items) counts.set(it.category, (counts.get(it.category) || 0) + 1);
    if (S.category !== 'all' && !counts.has(S.category)) S.category = 'all';
    const types = S.meta.opportunity_types.filter((t) => counts.has(t.id));
    const chip = (id, label, n, full) => h('button', { type: 'button', class: 'chip', title: full, 'aria-label': `${full || label}, ${n}`, 'aria-pressed': String(S.category === id), on: { click: () => setCategory(id) } },
      label, h('span', { class: 'count', 'aria-hidden': 'true', text: String(n) }));
    chips.replaceChildren(chip('all', 'All', items.length), ...types.map((t) => chip(t.id, t.label.split(/ & |, /)[0], counts.get(t.id), t.label)));
    chips.hidden = types.length < 2;
  }
  function renderList(animate) {
    const items = viewItems().filter((it) => S.category === 'all' || it.category === S.category);
    if (S.view === 'saved' && !items.length) {
      stateSlot.replaceChildren(h('div', { class: 'state' }, h('div', { class: 'state-icon' }, icon('bookmark')),
        h('h2', { class: 'h2', text: 'Nothing saved yet' }),
        h('p', { text: 'Tap the bookmark on anything you want to come back to. Saved opportunities stay here until you remove them.' })));
    } else stateSlot.replaceChildren();
    const feature = S.view === 'all' && S.category === 'all' && items.length >= 3;
    const showNew = S.items.some((i) => i.is_new) && S.items.some((i) => !i.is_new);
    list.replaceChildren(...items.map((it, i) => {
      const c = Card(it, { feature: feature && i === 0, showNew });
      if (animate) { c.classList.add('is-entering'); c.style.setProperty('--d', `${Math.min(i, 9) * 60}ms`); }
      return c;
    }));
  }
  function transition(fn) {
    if (!doc.startViewTransition || reduced.matches) return fn();
    doc.startViewTransition(fn);
  }

  function renderRefresh() {
    const run = S.me.run;
    const show = phase !== 'loading' && run;
    if (!show) { refreshSlot.replaceChildren(); refreshBar = null; return; }
    if (!refreshBar) {
      refreshMeter = Meter();
      refreshMsg = h('span', { class: 'refresh-msg' });
      refreshLabel = h('b');
      refreshBar = h('div', { class: 'refresh-bar', role: 'status' },
        h('div', { class: 'refresh-row' }, h('span', { class: 'spinner', 'aria-hidden': 'true' }), refreshLabel, refreshMsg), refreshMeter.bar);
      refreshSlot.replaceChildren(refreshBar);
    }
    refreshLabel.textContent = run.kind === 'theme' ? 'New photo' : run.kind === 'research' ? 'Researching' : 'Prospecting';
    refreshMsg.textContent = (run.progress && run.progress.message) || 'Getting started';
    refreshMeter.update(run.progress ? run.progress.percent : 0);
  }

  function render(animate) {
    const was = phase;
    phase = computePhase();
    if (phase === 'loading' && was !== 'loading') board.cycle(searchWords(), { every: 2400 });
    if (phase !== 'loading') board.pause();
    renderHero();
    renderSheet(animate || (was === 'loading' && phase === 'ready'));
    renderRefresh();
  }

  async function finishLoading(run) {
    finishing = true;
    const n = S.items.length;
    board.pause();
    board.show(`${n} FOUND`);
    feed.update(run, `Found ${plural(n, 'opportunity', 'opportunities')} for you`);
    meter.update(100);
    announce(`Found ${plural(n, 'opportunity', 'opportunities')}.`);
    await wait(reduced.matches ? 0 : 1700);
    finishing = false;
    if (screen && screen.el === el) { render(true); title.focus({ preventScroll: true }); }
  }

  function update(reason, run) {
    if (finishing) return;
    if (reason === 'progress') {
      if (phase === 'loading') { feed.update(S.me.run); meter.update(S.me.run && S.me.run.progress ? S.me.run.progress.percent : 0); }
      if (computePhase() !== phase) render(); else { renderRefresh(); if (phase === 'ready') renderHero(); }
      return;
    }
    if (reason === 'theme') { renderHero(); return; }
    if (reason === 'route') { render(); return; }
    if (reason === 'run-ended' && run) {
      if (run.kind === 'research') {
        render();
        toast(run.status === 'failed' ? run.error || 'We couldn’t refresh your intelligence this time.' : 'Intelligence updated from the latest public sources.', { error: run.status === 'failed' });
        return;
      }
      if (phase === 'loading' && S.items.length) { finishLoading(run); return; }
      const was = phase;
      render();
      if (was === 'ready' && run.kind === 'discover') {
        if (run.status === 'failed') toast(run.error || 'That refresh didn’t finish.', { error: true, action: 'Try again', onAction: refreshNow });
        else if (run.summary) {
          const k = run.summary.new || 0;
          toast(k ? `${plural(k, 'new opportunity', 'new opportunities')} added to the top.` : 'You’re up to date. Nothing new since last time.');
        }
      } else if (run.kind === 'theme' && run.status === 'failed') toast(run.error || 'We couldn’t find a new photo this time.', { error: true });
      return;
    }
    render();
  }

  function setView(v) {
    if (S.view === v) return;
    history.pushState(null, '', v === 'saved' ? '/saved' : '/');
    S.tab = 'opportunities';
    S.view = v;
    S.category = 'all';
    transition(() => { renderSheet(); });
  }
  function setCategory(id) {
    S.category = id;
    const base = S.view === 'saved' ? '/saved' : '/';
    history.pushState(null, '', id === 'all' ? base : `${base}?category=${encodeURIComponent(id)}`);
    transition(() => { renderChips(); renderList(); });
  }

  async function refreshNow() {
    try {
      const r = await api('POST', '/api/runs', { kind: 'discover' });
      S.me.run = r.run;
      startPolling();
      render();
      announce('Prospecting for new opportunities.');
    } catch (err) { toast(err.message, { error: true }); }
  }

  /* --- card actions: optimistic, rolled back on error --- */
  function patch(id, fields) {
    for (const arr of [S.items, S.saved]) { const it = arr.find((x) => x.id === id); if (it) Object.assign(it, fields); }
  }
  function syncSaved(item) {
    const i = S.saved.findIndex((x) => x.id === item.id);
    if (item.status === 'saved' && i < 0) S.saved.unshift({ ...item });
    if (item.status !== 'saved' && i >= 0) S.saved.splice(i, 1);
  }
  function rerenderKeepFocus(id, action) {
    renderChips();
    renderList();
    viewAll.lastChild.textContent = String(S.items.length);
    viewSaved.lastChild.textContent = String(S.saved.length);
    renderHero();
    const btn = list.querySelector(`[data-id="${id}"] [data-action="${action}"]`);
    if (btn) btn.focus({ preventScroll: true });
  }
  async function toggleSave(item) {
    const prev = item.status;
    const next = prev === 'saved' ? 'active' : 'saved';
    patch(item.id, { status: next });
    syncSaved({ ...item, status: next });
    rerenderKeepFocus(item.id, 'save');
    try {
      const r = await api('POST', `/api/opportunities/${item.id}/save`, { saved: next === 'saved' });
      if (r && r.item) patch(item.id, { status: r.item.status });
    } catch (err) {
      patch(item.id, { status: prev });
      syncSaved({ ...item, status: prev });
      rerenderKeepFocus(item.id, 'save');
      toast(err.message, { error: true });
    }
  }
  async function vote(item, v) {
    const prev = item.vote || 0;
    const next = prev === v ? 0 : v;
    patch(item.id, { vote: next });
    rerenderKeepFocus(item.id, v > 0 ? 'up' : 'down');
    try { await api('POST', `/api/opportunities/${item.id}/vote`, { vote: next }); }
    catch (err) { patch(item.id, { vote: prev }); rerenderKeepFocus(item.id, v > 0 ? 'up' : 'down'); toast(err.message, { error: true }); }
  }
  function dismiss(item) {
    const where = { items: S.items.findIndex((x) => x.id === item.id), saved: S.saved.findIndex((x) => x.id === item.id) };
    const cards = [...list.children];
    const pos = cards.findIndex((c) => c.dataset.id === String(item.id));
    const neighbour = cards[pos + 1] || cards[pos - 1];
    if (where.items >= 0) S.items.splice(where.items, 1);
    if (where.saved >= 0) S.saved.splice(where.saved, 1);
    const restoreLocal = (it) => {
      if (where.items >= 0 && !S.items.some((x) => x.id === it.id)) S.items.splice(Math.min(where.items, S.items.length), 0, it);
      if (where.saved >= 0 && !S.saved.some((x) => x.id === it.id)) S.saved.splice(Math.min(where.saved, S.saved.length), 0, { ...it });
    };
    transition(() => { if (computePhase() !== phase) render(); else { renderChips(); renderList(); renderHero(); renderSheetCounts(); } });
    const focusTarget = neighbour && list.querySelector(`[data-id="${neighbour.dataset.id}"] .card-title a, [data-id="${neighbour.dataset.id}"] [data-action="save"]`);
    if (focusTarget) focusTarget.focus({ preventScroll: true });
    const req = api('POST', `/api/opportunities/${item.id}/dismiss`, {});
    const t = toast('Removed. We’ll show fewer like this.', {
      action: 'Undo',
      onAction: async () => {
        try {
          await req.catch(() => {});
          const r = await api('POST', `/api/opportunities/${item.id}/restore`, {});
          restoreLocal(r.item || item);
          transition(() => { render(); });
          const back = list.querySelector(`[data-id="${item.id}"] [data-action="save"]`);
          if (back) back.focus({ preventScroll: true });
        } catch (err) { toast(err.message, { error: true }); }
      },
    });
    req.catch((err) => { t.close(); restoreLocal(item); render(); toast(err.message, { error: true }); });
  }
  function renderSheetCounts() {
    viewAll.lastChild.textContent = String(S.items.length);
    viewSaved.lastChild.textContent = String(S.saved.length);
  }

  function Card(item, { feature, showNew }) {
    const saved = item.status === 'saved';
    const tid = `op-${item.id}`;
    const dl = deadlineInfo(item.deadline);
    const titleKids = [item.title, icon('out')];
    const iconBtn = (action, ic, label, pressed, fn) => {
      const b = h('button', { type: 'button', class: 'icon-btn', 'data-action': action, 'aria-label': label, 'data-tip': label, 'aria-pressed': pressed == null ? null : String(pressed) }, icon(ic));
      b.addEventListener('click', fn);
      return b;
    };
    const siteIcon = () => {
      const name = item.site_name || item.organization || hostOf(item.url) || '?';
      const mono = h('span', { class: 'mono', 'aria-hidden': 'true', text: name.trim()[0] ? name.trim()[0].toUpperCase() : '?' });
      let hue = 0;
      for (const ch of name) hue = (hue * 31 + ch.charCodeAt(0)) % 360;
      mono.style.setProperty('--h', String(hue));
      if (!/^https:\/\//i.test(item.site_icon || '')) return mono;
      const img = h('img', { class: 'site-icon', alt: '', width: '18', height: '18', loading: 'lazy', referrerpolicy: 'no-referrer' });
      img.addEventListener('error', () => img.replaceWith(mono), { once: true });
      img.src = item.site_icon;
      return img;
    };
    const fit = item.score >= 85 ? 3 : item.score >= 70 ? 2 : 0;
    const el = h('article', { class: 'card' + (feature ? ' is-feature' : ''), 'aria-labelledby': tid, 'data-id': String(item.id) },
      h('div', { class: 'card-top' },
        h('span', { class: 'card-cat', text: typeLabel(item.category) }),
        FROM[item.source] ? h('span', { class: 'badge badge-src' }, h('i', { class: `src-dot src-${item.source}`, 'aria-hidden': 'true' }), FROM[item.source]) : null,
        feature && item.score >= 80 ? h('span', { class: 'badge badge-quiet', text: 'Top pick' }) : null,
        showNew && item.is_new ? h('span', { class: 'badge', text: 'New' }) : null,
        h('div', { class: 'card-actions' },
          iconBtn('save', 'bookmark', saved ? 'Saved' : 'Save', saved, () => toggleSave(item)),
          iconBtn('dismiss', 'x', 'Not for me', null, () => dismiss(item)))),
      h('div', { class: 'card-body' },
        h('div', { class: 'card-main' },
          h('h3', { class: 'card-title', id: tid }, outLink(item.url, titleKids)),
          item.organization || item.site_name ? h('p', { class: 'card-org' }, siteIcon(), h('span', { text: item.organization || item.site_name })) : null,
          item.summary ? h('p', { class: 'card-summary', text: item.summary }) : null),
        h('div', { class: 'card-aside' },
          item.why ? h('p', { class: 'card-why' }, h('span', { class: 'sr-only', text: 'Why it fits you: ' }), item.why) : null,
          item.action ? h('p', { class: 'card-next' }, h('span', { class: 'card-next-label', text: 'Next step' }), h('span', { text: item.action })) : null)),
      h('div', { class: 'facts' },
        h('span', { class: 'fact' + (dl && dl.level ? ' is-' + dl.level : ''), title: dl ? dl.date : null }, icon(dl ? 'clock' : 'calendar'),
          h('span', { text: dl ? dl.text : 'No deadline listed' })),
        item.value ? h('span', { class: 'fact' }, icon('value'), h('span', { text: item.value })) : null,
        item.location ? h('span', { class: 'fact' }, icon('pin'), h('span', { text: item.location })) : null),
      h('div', { class: 'card-foot' },
        item.verified ? h('span', { class: 'signal signal-verified', title: 'We read the page and confirmed it’s open.' }, icon('verified'), 'Verified') : null,
        fit ? h('span', { class: 'signal' }, h('span', { class: 'fit', 'aria-hidden': 'true' }, [1, 2, 3].map((k) => h('i', { class: k <= fit ? 'on' : '' }))),
          fit === 3 ? 'Strong fit' : 'Good fit') : null,
        h('div', { class: 'votes', role: 'group', 'aria-label': 'Rate this suggestion' },
          iconBtn('up', 'up', 'More like this', item.vote === 1, () => vote(item, 1)),
          iconBtn('down', 'down', 'Less like this', item.vote === -1, () => vote(item, -1)))));
    if (doc.startViewTransition) el.style.setProperty('view-transition-name', tid);
    return el;
  }

  function EmptyState() {
    return h('div', { class: 'state' }, h('div', { class: 'state-icon' }, icon('search')),
      h('h2', { class: 'h2', text: 'Nothing worth acting on this round' }),
      h('p', { text: 'We searched widely, but nothing open right now fits your business well enough to show. Broadening your focus areas usually helps.' }),
      h('div', { class: 'state-actions' }, Btn('Edit your profile', { kind: 'secondary', icon: 'pencil', onClick: openProfileEditor }), Btn('Search again', { icon: 'refresh', onClick: refreshNow })));
  }
  function FailedState() {
    const last = S.me.last_run || {};
    return h('div', { class: 'state is-error', role: 'alert' }, h('div', { class: 'state-icon' }, icon('alert')),
      h('h2', { class: 'h2', text: 'This search didn’t finish' }),
      h('p', { text: last.error || 'Something went wrong on our side. Your business profile is saved, so trying again is quick.' }),
      h('div', { class: 'state-actions' }, Btn('Try again', { icon: 'refresh', onClick: refreshNow })));
  }

  /* --- steering --- */
  function toggleSteer(force) {
    steerOpen = force == null ? !steerOpen : force;
    steerBtn.setAttribute('aria-expanded', String(steerOpen));
    if (!steerOpen) { steerSlot.replaceChildren(); steerBtn.focus(); return; }
    steerSlot.replaceChildren(SteerPanel());
    const inp = steerSlot.querySelector('input[type="text"]');
    if (inp) inp.focus();
  }
  function SteerPanel() {
    const slot = h('div');
    const text = Field({ name: 'text', label: 'What should we look for, or skip?', placeholder: 'e.g. More remote roles, nothing academic', maxlength: 300 });
    text.el.querySelector('label').classList.add('sr-only');
    const refresh = h('input', { type: 'checkbox', name: 'refresh' });
    const addBtn = Btn('Add note', { type: 'submit', small: true });
    const notes = h('ul', { class: 'notes', 'aria-label': 'Your notes' });
    const renderNotes = () => notes.replaceChildren(...(S.notes || []).map((n) => h('li', { class: 'tag' }, h('span', { text: n.text }),
      h('button', { type: 'button', 'aria-label': `Remove note: ${n.text}`, on: { click: () => removeNote(n) } }, icon('x')))));
    async function removeNote(n) {
      const before = S.notes.slice();
      S.notes = S.notes.filter((x) => x.id !== n.id);
      renderNotes();
      try { await api('DELETE', `/api/steering/${n.id}`, {}); }
      catch (err) { S.notes = before; renderNotes(); toast(err.message, { error: true }); }
    }
    const form = h('form', { class: 'steer', novalidate: true, 'aria-label': 'Steer your results' },
      h('div', { class: 'steer-head' },
        h('div', null, h('h2', { class: 'h3', text: 'Steer your results' }), h('p', { text: 'Tell us what to look for or skip. We use it from the next refresh.' })),
        h('button', { type: 'button', class: 'icon-btn', 'aria-label': 'Close', on: { click: () => toggleSteer(false) } }, icon('x'))),
      slot,
      h('div', { class: 'steer-form' }, text.el, addBtn),
      h('label', { class: 'check' }, refresh, 'Refresh my results now'),
      notes);
    form.addEventListener('submit', (e) => {
      e.preventDefault();
      busy(addBtn, async () => {
        try {
          const r = await api('POST', '/api/steering', { text: text.input.value.trim(), refresh: refresh.checked });
          S.notes = [r.note, ...(S.notes || []).filter((n) => n.id !== r.note.id)];
          renderNotes();
          text.input.value = '';
          clearErrors(form);
          if (r.run) { S.me.run = r.run; startPolling(); render(); toast('Got it. Prospecting with your note now.'); }
          else toast('Got it. We’ll use that from the next refresh.');
        } catch (err) { showError(form, err, slot); }
      });
    });
    if (S.notes) renderNotes();
    else api('GET', '/api/steering').then((r) => { S.notes = r.notes || []; renderNotes(); }).catch(() => {});
    return form;
  }

  render(true);
  if (S.me.run) startPolling();
  return {
    el, update, destroy: board.stop,
    focus: () => (phase === 'loading' ? stageTitle : title),
  };
}

/* ------------------------------------------------------------------ settings + profile editing */

function openProfileEditor() {
  doc.querySelectorAll('dialog.drawer[open]').forEach((d) => d.close());
  let dlg = null;
  const form = ProfileForm(S.me.profile || {}, {
    submitLabel: 'Save changes',
    note: 'Saving starts a fresh search if anything important changed.',
    onSaved: (r) => {
      const started = r.run && (!S.me.run || S.me.run.id !== r.run.id);
      Object.assign(S.me, { profile: r.profile, user: r.user });
      if (r.run) { S.me.run = r.run; startPolling(); }
      dlg.close();
      toast(started ? 'Saved. We’re refreshing your opportunities to match.' : 'Saved.');
      if (screen && screen.update) screen.update('profile');
    },
  });
  dlg = openDialog({ cls: 'modal', title: 'Business profile', body: form });
}

function openSettings() {
  const me = S.me, M = S.meta;
  const flag = () => h('span', { class: 'saved-flag', 'aria-live': 'polite' });
  const flash = (f) => { f.textContent = 'Saved'; f.classList.add('is-on'); setTimeout(() => f.classList.remove('is-on'), 1600); };
  const nextLine = h('p', { class: 'hint' });
  const setNext = () => {
    const s = S.me.settings || {};
    nextLine.textContent = s.frequency === 'off' ? 'We’ll only search when you press Refresh.' : s.next_run_at ? `Next refresh ${relTime(s.next_run_at)}.` : '';
  };
  setNext();
  async function save(patchBody, f, revert) {
    try {
      const r = await api('PUT', '/api/settings', patchBody);
      S.me.settings = r.settings;
      S.nextRefresh = r.settings.next_run_at || null;
      setNext();
      flash(f);
      if (screen && screen.update) screen.update('settings');
    } catch (err) { revert(); toast(err.message, { error: true }); }
  }
  const freqFlag = flag();
  const freqName = uid('freq');
  const radios = h('div', { class: 'radios' }, M.frequencies.map((fq) => {
    const input = h('input', { type: 'radio', name: freqName, value: fq.id, checked: me.settings.frequency === fq.id });
    input.addEventListener('change', () => {
      const prev = S.me.settings.frequency;
      save({ frequency: fq.id }, freqFlag, () => { const r = radios.querySelector(`input[value="${CSS.escape(prev)}"]`); if (r) r.checked = true; });
    });
    return h('label', { class: 'radio' }, input, h('span', { text: fq.label }));
  }));
  const mailFlag = flag();
  const digest = h('input', { type: 'checkbox', role: 'switch', checked: !!me.settings.email_digest, id: uid('sw') });
  digest.addEventListener('change', () => save({ email_digest: digest.checked }, mailFlag, () => { digest.checked = !digest.checked; }));

  // Account forms, each tucked behind a link until needed.
  function Reveal(label, ic, build, danger) {
    const box = h('div', { hidden: true });
    const btn = h('button', { type: 'button', class: 'set-link' + (danger ? ' is-danger' : ''), 'aria-expanded': 'false' }, icon(ic), h('span', { text: label }));
    btn.addEventListener('click', () => {
      const open = box.hidden;
      box.hidden = !open;
      btn.setAttribute('aria-expanded', String(open));
      if (open) { box.replaceChildren(build(() => btn.click())); const i = box.querySelector('input'); if (i) i.focus(); }
    });
    return [btn, box];
  }
  const passwordForm = (close) => {
    const slot = h('div');
    const cur = PasswordField({ name: 'current_password', label: 'Current password', autocomplete: 'current-password' });
    const nw = PasswordField({ name: 'new_password', label: 'New password', autocomplete: 'new-password', hint: 'At least 8 characters.' });
    const b = Btn('Change password', { type: 'submit', small: true });
    const form = h('form', { class: 'set-form', novalidate: true }, slot, cur.el, nw.el, h('div', { class: 'actions' }, b));
    form.addEventListener('submit', (e) => {
      e.preventDefault();
      busy(b, async () => {
        try {
          await api('POST', '/api/account/password', { current_password: cur.input.value, new_password: nw.input.value });
          close();
          toast('Password changed. Any other devices have been signed out.');
        } catch (err) { showError(form, err, slot); }
      });
    });
    return form;
  };
  const deleteForm = () => {
    const slot = h('div');
    const pw = PasswordField({ name: 'password', label: 'Your password', autocomplete: 'current-password' });
    const b = Btn('Delete my account', { kind: 'danger', type: 'submit', small: true });
    const form = h('form', { class: 'set-form', novalidate: true }, slot,
      h('p', { class: 'hint', text: 'This permanently deletes the business account, its profile, intelligence, connections and every opportunity we found. It can’t be undone.' }),
      pw.el, h('div', { class: 'actions' }, b));
    form.addEventListener('submit', (e) => {
      e.preventDefault();
      busy(b, async () => {
        try {
          await api('DELETE', '/api/account', { password: pw.input.value });
          resetState();
          S.notice = 'The business account and everything in it has been deleted.';
          go('/', true);
        } catch (err) { showError(form, err, slot); }
      });
    });
    return form;
  };
  const whoAvatar = h('span', { class: 'avatar', 'aria-hidden': 'true', text: initials(me.user) });
  const whoName = h('b', { text: me.user.business_name || 'Your business' });
  const photoBtn = h('button', { type: 'button', class: 'set-link' }, icon('image'), h('span', { text: 'Find a new background photo' }));
  photoBtn.addEventListener('click', async () => {
    try {
      const r = await api('POST', '/api/theme/refresh', {});
      S.me.run = r.run;
      startPolling();
      if (screen && screen.update) screen.update('progress');
      toast('Looking for a new photo. It’ll fade in when it’s ready.');
    } catch (err) { toast(err.message, { error: true }); }
  });
  const editBtn = h('button', { type: 'button', class: 'set-link' }, icon('pencil'), h('span', { text: 'Edit business profile' }));
  editBtn.addEventListener('click', openProfileEditor);
  const outBtn = h('button', { type: 'button', class: 'set-link' }, icon('logout'), h('span', { text: 'Sign out' }));
  outBtn.addEventListener('click', signOut);
  const body = h('div', { class: 'set' },
    h('section', { class: 'set-section' },
      h('div', { class: 'label-row' }, h('h3', { class: 'set-title', text: 'Refresh schedule' }), freqFlag),
      h('p', { class: 'hint', text: 'How often should we prospect for new opportunities?' }), radios, nextLine),
    h('section', { class: 'set-section' },
      h('div', { class: 'label-row' }, h('h3', { class: 'set-title', text: 'Email' }), mailFlag),
      h('label', { class: 'switch', for: digest.id },
        h('span', { class: 'switch-text' }, h('b', { text: 'Email me new finds' }), h('span', { class: 'hint', text: 'A short digest when a refresh turns up something new.' })), digest),
      M.email_live ? null : h('p', { class: 'set-note' }, icon('mail'), h('span', { text: 'Email isn’t switched on for this server yet, so nothing will actually be sent for now.' }))),
    h('section', { class: 'set-section' },
      h('h3', { class: 'set-title', text: 'AI model' }),
      h('div', { class: 'set-model' }, h('div', null, h('b', { class: 'model-chip-label', text: modelLabel() }), h('p', { class: 'hint', text: 'Used for planning, judging and writing. Search always runs on DeepSeek.' })),
        Btn('Change', { kind: 'secondary', small: true, onClick: openModelPanel }))),
    ConnectorsSection(),
    h('section', { class: 'set-section' },
      h('h3', { class: 'set-title', text: 'Your page' }),
      h('div', { class: 'set-links' }, editBtn, photoBtn)),
    h('section', { class: 'set-section' },
      h('h3', { class: 'set-title', text: 'Account' }),
      h('div', { class: 'who' }, whoAvatar, h('div', null, whoName, h('span', { text: me.user.email }))),
      h('div', { class: 'set-links' }, ...Reveal('Change password', 'eye', passwordForm), outBtn, ...Reveal('Delete business account', 'x', deleteForm, true))));
  openDialog({ cls: 'drawer', title: 'Settings', body });
}

/* ------------------------------------------------------------------ boot */

const CONN_LABEL = { slack: 'Slack', microsoft: 'Microsoft 365', confluence: 'Confluence' };
function connectorReturn() {
  const qs = new URLSearchParams(location.search);
  const ok = qs.get('connected'), bad = qs.get('connector_error');
  if (ok == null && bad == null) return;
  qs.delete('connected'); qs.delete('connector_error');
  history.replaceState(null, '', location.pathname + (qs.toString() ? `?${qs}` : ''));
  setTimeout(() => {
    if (bad != null) toast(`That connection didn’t go through: ${bad.slice(0, 240)}`, { error: true });
    else toast(`${CONN_LABEL[ok] || 'Your tool'} is connected. Access is read-only.`);
  }, 400);
}
async function boot() {
  connectorReturn();
  try {
    const [meta, me] = await Promise.all([
      api('GET', '/api/meta'),
      api('GET', '/api/me').catch((e) => { if (e.status === 401) return null; throw e; }),
    ]);
    S.meta = meta;
    S.me = me;
    route();
  } catch (err) {
    const retry = Btn('Try again', { onClick: () => { $('#app').replaceChildren(); boot(); } });
    $('#app').replaceChildren(h('main', { class: 'state', id: 'main' }, h('div', { class: 'state-icon' }, icon('alert')),
      h('h1', { class: 'h2', text: 'We can’t load Prospect right now' }), h('p', { text: err.message }), h('div', { class: 'state-actions' }, retry)));
  }
}
doc.addEventListener('visibilitychange', () => { if (!doc.hidden && S.me && S.me.run && !pollBusy) { stopPolling(); pollOnce(); } });
boot();
