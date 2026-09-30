/* ===== Unified Novel Downloader – WebUI ===== */

let loginPromise = null;
// isDockerBuild：原唯一赋值点在状态页 refreshStatus()（读取 /api/status 的 docker_build），
// 该函数已随状态页死代码一并移除，这里保留显式默认值以维持既有分支结果（当前恒为 false，
// 等价于“非 Docker 构建”）。如需恢复 Docker 自更新限制，请重新接入 /api/status 后再赋值。
let isDockerBuild = false;
let lastIidWarningMessage = null;

function fetchWithCreds(url, opts) {
  return fetch(url, { credentials: 'same-origin', ...(opts || {}) });
}

// ── Theme ──────────────────────────────────────────────────────────

const THEME_KEY = 'tnd.theme';

function getStoredTheme() {
  try { return localStorage.getItem(THEME_KEY); } catch { return null; }
}

function applyTheme(theme) {
  if (theme === 'light' || theme === 'dark') {
    document.documentElement.setAttribute('data-theme', theme);
  } else {
    document.documentElement.removeAttribute('data-theme');
  }
  updateThemeButton(theme);
}

function updateThemeButton(theme) {
  const icon = document.getElementById('themeIcon');
  const label = document.getElementById('themeLabel');
  if (!icon) return;

  const isDark = theme === 'dark' ||
    (!theme && window.matchMedia('(prefers-color-scheme: dark)').matches);

  if (isDark) {
    icon.innerHTML = '<circle cx="12" cy="12" r="5"/><line x1="12" y1="1" x2="12" y2="3"/><line x1="12" y1="21" x2="12" y2="23"/><line x1="4.22" y1="4.22" x2="5.64" y2="5.64"/><line x1="18.36" y1="18.36" x2="19.78" y2="19.78"/><line x1="1" y1="12" x2="3" y2="12"/><line x1="21" y1="12" x2="23" y2="12"/><line x1="4.22" y1="19.78" x2="5.64" y2="18.36"/><line x1="18.36" y1="5.64" x2="19.78" y2="4.22"/>';
    if (label) label.textContent = '亮色模式';
  } else {
    icon.innerHTML = '<path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/>';
    if (label) label.textContent = '暗色模式';
  }
}

function toggleTheme() {
  const current = document.documentElement.getAttribute('data-theme');
  let next;
  if (current === 'dark') {
    next = 'light';
  } else if (current === 'light') {
    next = 'dark';
  } else {
    // auto → opposite of system
    next = window.matchMedia('(prefers-color-scheme: dark)').matches ? 'light' : 'dark';
  }
  try { localStorage.setItem(THEME_KEY, next); } catch {}
  applyTheme(next);
}

// Apply stored theme immediately
(function() {
  const stored = getStoredTheme();
  if (stored) applyTheme(stored);
})();

// ── Auth ───────────────────────────────────────────────────────────

function showLogin(show) {
  const modal = document.getElementById('loginModal');
  if (!modal) return;
  modal.classList.toggle('hidden', !show);
  document.body.style.overflow = show ? 'hidden' : '';
  if (show) {
    const inp = document.getElementById('loginPassword');
    if (inp) inp.focus();
  }
}

function showIidWarningModal(show, message = '') {
  const modal = document.getElementById('iidWarningModal');
  if (!modal) return;
  const body = document.getElementById('iidWarningBody');
  if (body && message) body.textContent = message;
  modal.classList.toggle('hidden', !show);
  document.body.style.overflow = show ? 'hidden' : '';
}

function maybeShowIidWarning(message) {
  const text = (message || '').toString().trim();
  if (!text || lastIidWarningMessage === text) return;
  lastIidWarningMessage = text;
  showIidWarningModal(true, text);
}

function maybeShowIidWarningFromError(message) {
  const text = (message || '').toString();
  const lower = text.toLowerCase();
  if (lower.includes('iid') || lower.includes('log.snssdk.com') || lower.includes('device_register')) {
    maybeShowIidWarning(text);
  }
}

async function requireLogin() {
  if (loginPromise) return loginPromise;

  showLogin(true);
  const msg = document.getElementById('loginMsg');
  if (msg) msg.textContent = '';

  loginPromise = new Promise((resolve, reject) => {
    const form = document.getElementById('loginForm');
    if (!form) { reject(new Error('login form missing')); return; }

    const handler = async (e) => {
      e.preventDefault();
      const pw = (document.getElementById('loginPassword')?.value || '').toString();
      try {
        const res = await fetchWithCreds('/api/login', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ password: pw })
        });
        if (!res.ok) { if (msg) msg.textContent = '密码错误'; return; }
        showLogin(false);
        form.removeEventListener('submit', handler);
        resolve(true);
      } catch (err) {
        if (msg) msg.textContent = String(err || 'login failed');
      }
    };
    form.addEventListener('submit', handler);
  }).finally(() => { loginPromise = null; });

  return loginPromise;
}

// ── HTTP Helper ────────────────────────────────────────────────────

async function j(url, opts) {
  const res = await fetchWithCreds(url, opts);
  if (res.status === 401) {
    await requireLogin();
    const res2 = await fetchWithCreds(url, opts);
    if (!res2.ok) {
      const text = await res2.text().catch(() => '');
      const message = `${res2.status} ${res2.statusText}${text ? `: ${text}` : ''}`;
      maybeShowIidWarningFromError(message);
      throw new Error(message);
    }
    const ct2 = res2.headers.get('content-type') || '';
    return ct2.includes('application/json') ? res2.json() : res2.text();
  }
  if (!res.ok) {
    const text = await res.text().catch(() => '');
    const message = `${res.status} ${res.statusText}${text ? `: ${text}` : ''}`;
    maybeShowIidWarningFromError(message);
    throw new Error(message);
  }
  const ct = res.headers.get('content-type') || '';
  return ct.includes('application/json') ? res.json() : res.text();
}

// ── Utilities ──────────────────────────────────────────────────────

function esc(s) {
  return (s ?? '').toString().replace(/[&<>"']/g, c =>
    ({ '&':'&amp;', '<':'&lt;', '>':'&gt;', '"':'&quot;', "'":'&#39;' }[c]));
}

function fmtBytes(n) {
  const x = Number(n || 0);
  if (!isFinite(x) || x <= 0) return '0 B';
  const k = 1024;
  const sizes = ['B','KB','MB','GB','TB'];
  const i = Math.floor(Math.log(x) / Math.log(k));
  return (x / Math.pow(k, i)).toFixed(i === 0 ? 0 : 1) + ' ' + sizes[i];
}

function fmtTime(ms) {
  const x = Number(ms || 0);
  if (!isFinite(x) || x <= 0) return '';
  return new Date(x).toLocaleString();
}

function encodePathSegments(path) {
  return (path || '').toString().split('/').map(seg => encodeURIComponent(seg)).join('/');
}

// ── 搜索结果高亮 / 书籍卡片 ────────────────────────────────────

function escRe(s) {
  return (s || '').toString().replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

// 将命中搜索词的片段在文本中高亮为 <mark>，其余文本安全转义。
// 分词粒度(B)：整词 + 中文 2-gram 优先命中；若整段都未命中，再用中文单字兜底。
function buildHighlightTerms(query) {
  const raw = (query ?? '').toString().trim();
  if (!raw) return { tokens: [], fallback: [] };
  const words = Array.from(new Set(raw.split(/\s+/).map(s => s.trim()).filter(Boolean)));
  const tokenSet = new Set();
  const charSet = new Set();
  for (const w of words) {
    tokenSet.add(w);
    if (/[\u4e00-\u9fff]/.test(w) && w.length >= 2) {
      for (let i = 0; i + 2 <= w.length; i++) tokenSet.add(w.slice(i, i + 2));
      for (const ch of w) if (/[\u4e00-\u9fff]/.test(ch)) charSet.add(ch);
    }
  }
  return { tokens: Array.from(tokenSet), fallback: Array.from(charSet) };
}

// 用给定词元在 raw 中标注 <mark>；长词优先避免重叠漏标；无任何命中返回 null（供上层兜底）。
function applyMark(raw, terms) {
  const uniq = Array.from(new Set((terms || []).filter(Boolean))).sort((a, b) => b.length - a.length);
  if (!uniq.length) return null;
  const re = new RegExp('(' + uniq.map(escRe).join('|') + ')', 'gi');
  let out = '';
  let last = 0;
  let m;
  let hit = false;
  while ((m = re.exec(raw)) !== null) {
    if (m[0].length === 0) { re.lastIndex++; continue; }
    hit = true;
    if (m.index > last) out += esc(raw.slice(last, m.index));
    out += '<mark>' + esc(m[0]) + '</mark>';
    last = m.index + m[0].length;
  }
  if (!hit) return null;
  out += esc(raw.slice(last));
  return out;
}

function highlight(text, query) {
  const raw = (text ?? '').toString();
  if (!raw) return '';
  const { tokens, fallback } = buildHighlightTerms(query);
  return applyMark(raw, tokens) ?? applyMark(raw, fallback) ?? esc(raw);
}

// 官方高亮透传：上游已在文本里用 <em> 标好命中片段（书旗 native_v3 的 displayBookName/desc 等）。
// 先整体转义，再把 &lt;em&gt;/&lt;/em&gt; 还原成 <mark>，其余标签保持转义，不存在注入面。
// 注意：官方源不再走前端自算 highlight()——接口没标就不高亮，避免展示上游没有的结果。
function emMark(text) {
  return esc(text ?? '')
    .replace(/&lt;em&gt;/g, '<mark>')
    .replace(/&lt;\/em&gt;/g, '</mark>');
}

// 封面 URL → 可直接用于 <img> 的地址（外部 URL 走服务端代理，本地/相对路径直用）。
function coverSrc(url) {
  const u = (url || '').toString().trim();
  if (!u) return '';
  if (u.startsWith('/')) return u;
  if (u.startsWith('http://') || u.startsWith('https://')) {
    return '/api/search-cover?url=' + encodeURIComponent(u);
  }
  return '';
}

function sourceBadge(bidStr) {
  const s = (bidStr ?? '').toString();
  if (s.startsWith('sq:')) return { text: '书旗', cls: 'tag-blue', icon: '/assets/sqnovel.png' };
  if (s.startsWith('qm:')) return { text: '七猫', cls: 'tag-orange', icon: '/assets/qmnovel.webp' };
  return { text: '番茄', cls: 'tag-green', icon: '/assets/fqnovel.webp' };
}

// 封面加载失败时用首字占位块替换。
function bookCoverError(el) {
  const d = document.createElement('div');
  d.className = 'book-card-cover cover-failed';
  d.textContent = (el && el.getAttribute('data-initial')) || '?';
  if (el && el.replaceWith) el.replaceWith(d);
}

// 统一书籍卡片：缩略图 + 书名 + 作者 + 简介 + 评分（+ 徽章 / 底栏）。
// o: {cover,title,author,desc,score,query,badges[],meta,foot,cardClass}
function bookCard(o) {
  o = o || {};
  const q = o.query || '';
  // official: 该源提供官方 <em> 高亮（书旗 native_v3），直显不重算。
  const hl = (t) => (o.official ? emMark(t) : highlight(t, q));
  const titleHtml = o.official ? (o.displayTitle || o.title || '') : (o.title || '');
  const cs = coverSrc(o.cover);
  const initial = esc((o.title || '?').toString().trim().slice(0, 1) || '?');
  const coverHtml = cs
    ? `<img class="book-card-cover" src="${esc(cs)}" alt="" loading="lazy" data-initial="${initial}" onerror="bookCoverError(this)">`
    : `<div class="book-card-cover cover-failed">${initial}</div>`;
  const coverFormat = o.coverFormat ? `<span class="cover-format-badge">${esc(o.coverFormat)}</span>` : '';
  const coverBox = `<div class="book-card-cover-box"><span class="cover-media">${coverHtml}${coverFormat}</span><span class="cover-badge-down">${o.coverBadgeDown || ''}</span>${o.coverBadge || ''}</div>`;
  const hasScore = o.score != null && o.score !== '' && !isNaN(Number(o.score));
  const scoreHtml = hasScore ? `<span class="book-card-score" title="评分">★ ${Number(o.score).toFixed(1)}</span>` : '';
  const badges = (o.badges || []).filter(b => b && (b.icon || b.text))
    .map(b => b.icon
      ? `<img class="src-badge" src="${esc(b.icon)}" alt="${esc(b.text || '')}" title="${esc(b.text || '')}">`
      : `<span class="tag ${b.cls || ''}">${esc(b.text)}</span>`).join('');
  const descHtml = o.desc
    ? `<div class="book-card-desc">${hl(o.desc)}</div>`
    : `<div class="book-card-desc book-card-desc-empty">暂无简介</div>`;
  const metaHtml = o.meta ? `<span class="book-card-extra">${esc(o.meta)}</span>` : '';
  // 作者为空则整行不渲染（零补全：不写“未知作者”这种看起来像数据的占位假值，如短剧无作者字段）。
  const authorTxt = (o.author || '').toString().trim();
  const authorHtml = authorTxt
    ? `<div class="book-card-author"><span class="book-card-author-text" title="${esc(authorTxt)}">${hl(authorTxt)}</span></div>`
    : '';
  // 标签胶囊：与分类同区（作者下方），不占用标题行。数据均为接口原字段（书旗 tags/cornerTagExt、番茄 tags）。
  const chips = (o.tagChips || []).filter(Boolean).map((t) => {
    const isObj = typeof t === 'object';
    const text = String((isObj ? t.text : t) ?? '');
    const cls = isObj ? (t.cls || '') : '';
    return `<span class="tag ${cls}">${esc(text)}</span>`;
  }).join('');
  const chipsHtml = chips ? `<span class="book-card-tags">${chips}</span>` : '';
  const footInner = (o.foot || '') + (o.scoreInFoot ? scoreHtml : '');
  const footHtml = footInner ? `<div class="book-card-foot">${footInner}</div>` : '';
  const dataAttrs = o.bookId
    ? ` data-bookid="${esc(o.bookId)}" data-cover="${esc(o.cover || '')}" data-title="${esc(o.title || '')}"`
    : '';
  // cardClass：任务卡用它挂 state-xxx 描边；不传时与原有卡片 DOM 完全一致。
  const cardCls = o.cardClass ? ` ${o.cardClass}` : '';
  return `
    <div class="book-card${cardCls}"${dataAttrs}>
      ${coverBox}
      <div class="book-card-body">
        <div class="book-card-title"><span class="book-card-title-text" title="${esc(o.title || '')}">${hl(titleHtml)}</span>${badges ? `<span class="book-card-badges">${badges}</span>` : ''}</div>
        ${authorHtml}
        ${(o.scoreInFoot ? '' : scoreHtml) || chipsHtml || metaHtml ? `<div class="book-card-meta">${(o.scoreInFoot ? '' : scoreHtml)}${chipsHtml}${metaHtml}</div>` : ''}
        ${descHtml}
        ${footHtml}
      </div>
    </div>`;
}

// 搜索结果卡片副信息：分类 · 字数 · 连载状态 · 在读人数（缺字段自动跳过）。
function fmtWordCount(n) {
  const x = Number(n);
  if (!isFinite(x) || x <= 0) return '';
  if (x >= 100000000) return (x / 100000000).toFixed(1).replace(/\.0$/, '') + '亿字';
  if (x >= 10000) return Math.round(x / 10000) + '万字';
  return x + '字';
}

function searchCardMeta(b) {
  b = b || {};
  const parts = [];
  if (b.category) parts.push(b.category);
  // 上游副标题（如短剧「都市修真·复仇、全60集」）原样直显，已含分类与集数。
  if (b.sub_title && String(b.sub_title).trim()) parts.push(String(b.sub_title).trim());
  const wc = fmtWordCount(b.word_count);
  if (wc) parts.push(wc);
  if (b.finished === true) parts.push('完结');
  else if (b.finished === false) parts.push('连载中');
  if (b.read_count_text) parts.push(b.read_count_text);
  else if (b.chapter_count) parts.push(b.chapter_count + (b.count_unit || '章'));
  return parts.join(' · ');
}

// ── 搜索工具栏：分类 tab + 筛选器 + 分页（均为无状态单请求）─────────────
// 搜索源注册表：新增一个源只改这里，其余逻辑均由数组推导，不再处处字面量。
const PROVIDER_REGISTRY = [
  { name: 'fanqie', label: '番茄', prefix: '' },
  { name: 'shuqi',  label: '书旗', prefix: 'sq:' },
  { name: 'qimao',  label: '七猫', prefix: 'qm:' },
];
const PROVIDER_ORDER = PROVIDER_REGISTRY.map(p => p.name);
const PROVIDER_LABELS = Object.fromEntries(PROVIDER_REGISTRY.map(p => [p.name, p.label]));

let searchState = null; // {q, tab, selected:Map(id->type), offset, tabs, rows, hasMore, nextOffset, mode}
// 各搜索源结果缓存与最近一次查询，用于结果展示后增量增/删搜索源。
let searchProviderCache = {};
let lastSearchQuery = '';
// 各 provider 的翻页游标与是否还有更多（加载更多：番茄按 offset、七猫/书旗按 page）。
let providerPages = {};
// 搜索源能力缓存：name -> { tabs: [], selector: {}|null }，由各源响应自报，不预设谁有分类/筛选。
let providerCaps = {};

// 吸收一次 /api/search 响应里的能力元数据（新格式 provider_meta[name]，兼容旧顶层字段）。
function absorbProviderMeta(name, data) {
  if (!name || !data) return;
  const m = (data.provider_meta && data.provider_meta[name])
    || ((data.tabs || data.selector) ? { tabs: data.tabs, selector: data.selector } : null);
  if (!m) return;
  providerCaps[name] = {
    tabs: Array.isArray(m.tabs) ? m.tabs.map(t => ({ tab_type: t.tab_type, title: t.title })) : [],
    selector: m.selector || null,
  };
}

// 当前勾选源的能力并集 → 工具栏模型（每个 tab/筛选项带 providers 归属）。
// 只勾七猫 → 只有七猫的筛选项；番茄+七猫 → 两者并集；一个支持分类/筛选的源都没勾 → 空模型。
function toolbarModelForSelection() {
  const metas = PROVIDER_ORDER.filter(n => providerCaps[n] && selectedProviderSet().has(n))
    .map(n => ({ name: n, meta: providerCaps[n] }));
  return buildToolbarModel(metas);
}

// 依据当前勾选源同步工具栏：能力变化则重建，已失效的分类/筛选项则剔除。
// 返回 { visible, pruned }；pruned=true 表示有条件被剪掉，调用方需按新条件重取数据。
function syncToolbar() {
  const bar = document.getElementById('searchToolbar');
  const model = toolbarModelForSelection();
  const realTabs = (model.tabs || []).filter(t => Number(t.tab_type) !== 1);
  const visible = !!lastSearchQuery && (realTabs.length > 0 || (model.rows || []).length > 0);
  if (!visible) {
    // 当前勾选源没有任何分类/筛选能力 → 整个工具栏隐藏（而非置灰残留）。
    if (bar) { bar.classList.add('hidden'); bar.innerHTML = ''; }
    searchState = null;
    refreshMoreButton();
    return { visible: false, pruned: false };
  }
  let pruned = false;
  const s = searchState;
  if (!s || s.q !== lastSearchQuery) {
    initSearchToolbar(lastSearchQuery, model);
  } else {
    s.tabs = model.tabs;
    s.rows = model.rows;
    if (s.tab && Number(s.tab) !== 1 && !model.tabs.some(t => Number(t.tab_type) === Number(s.tab))) {
      s.tab = 1;
      pruned = true;
    }
    for (const id of [...s.selected.keys()]) {
      const alive = model.rows.some(r => (r.items || []).some(it => it.selector_item_id === id));
      if (!alive) { s.selected.delete(id); pruned = true; }
    }
    s.mode = (Number(s.tab) === 1 && s.selected.size === 0) ? 'merged' : 'tab';
    // 筛选项已全部消失时重置展开态，避免残留一个没有入口的展开面板。
    if (!model.rows.length) s.filtersOpen = false;
    renderToolbar();
  }
  if (bar) bar.classList.remove('hidden');
  return { visible: true, pruned };
}

// 搜索区提示（复用 #searchHint，非空才占位），带自动清除避免残留误导。
let searchHintTimer = null;
function showSearchHint(msg, ms) {
  const hint = document.getElementById('searchHint');
  if (!hint) return;
  hint.textContent = msg;
  if (searchHintTimer) clearTimeout(searchHintTimer);
  if (ms > 0) {
    searchHintTimer = setTimeout(() => {
      if (hint.textContent === msg) hint.textContent = '';
    }, ms);
  }
}

function anyProviderMore() {
  const sel = searchState ? activeProvidersForState(searchState) : selectedProviderSet();
  return PROVIDER_ORDER.some(p => sel.has(p) && providerPages[p] && providerPages[p].hasMore);
}

// 刷新列表末尾“加载更多”按钮（tab 模式看番茄 hasMore；merged 模式看任一源 hasMore）。
function refreshMoreButton() {
  const moreBox = document.getElementById('listMore');
  if (!moreBox) return;
  const s = searchState;
  const canMore = anyProviderMore();
  if (canMore) {
    moreBox.innerHTML = '<button type="button" id="sMore" class="s-more sm ghost">加载更多 ↓</button>';
    moreBox.classList.remove('hidden');
  } else {
    moreBox.innerHTML = '';
    moreBox.classList.add('hidden');
  }
}

// 合并模式下：对所有“还有更多”的搜索源并行拉下一页并追加（数字与书籍随之变化）。
async function loadMoreProviders() {
  const q = lastSearchQuery;
  const st = searchState;
  const sel = st ? activeProvidersForState(st) : selectedProviderSet();
  const curTab = (st && st.tab) || 1;
  const curSel = st ? [...st.selected.keys()].join(',') : '';
  const pend = [];
  for (const name of PROVIDER_ORDER) {
    if (!sel.has(name)) continue;
    const pg = providerPages[name];
    if (!pg || !pg.hasMore) continue;
    let url;
    if (name === 'fanqie') {
      url = `/api/search?q=${encodeURIComponent(q)}&provider=fanqie&tab=${curTab}&selected_items=${encodeURIComponent(curSel)}&offset=${pg.nextOffset || 0}`;
    } else if (name === 'qimao') {
      pg.page = (pg.page || 1) + 1;
      url = `/api/search?q=${encodeURIComponent(q)}&provider=qimao&tab=${curTab}&selected_items=${encodeURIComponent(curSel)}&page=${pg.page}`;
    } else if (name === 'shuqi') {
      pg.page = (pg.page || 1) + 1;
      url = `/api/search?q=${encodeURIComponent(q)}&provider=shuqi&page=${pg.page}`;
    } else {
      continue;
    }
    pend.push((async () => {
      try {
        const data = await j(url);
        const items = data.items || [];
        for (const it of items) it._provider = name;
        absorbProviderMeta(name, data);
        const cache = searchProviderCache[name] || [];
        const seen = new Set(cache.map(x => String(x.book_id)));
        const fresh = items.filter(x => !seen.has(String(x.book_id)));
        searchProviderCache[name] = cache.concat(fresh);
        if (data.provider_has_more && data.provider_has_more[name] != null) {
          pg.hasMore = !!data.provider_has_more[name];
        } else if (name === 'fanqie') {
          pg.hasMore = !!data.has_more;
          pg.nextOffset = data.next_offset || 0;
        }
        if (fresh.length === 0) pg.hasMore = false;
      } catch { pg.hasMore = false; }
    })());
  }
  await Promise.all(pend);
  renderMergedResults(q);
}

// 结果排序：关键词相关度为主、平台为辅（平台顺序即 PROVIDER_REGISTRY 的注册顺序）。
function providerOrder(b) {
  const s = String((b && (b._provider || b.book_id)) || '');
  const name = (b && b._provider)
    || (PROVIDER_REGISTRY.find(p => p.prefix && s.startsWith(p.prefix)) || {}).name;
  const i = PROVIDER_ORDER.indexOf(name);
  return i < 0 ? 0 : i;
}
function relevanceScore(b, q) {
  const query = (q || '').toString().trim().toLowerCase();
  if (!query) return 0;
  const title = String((b && b.title) || '').toLowerCase();
  const author = String((b && b.author) || '').toLowerCase();
  const cat = String((b && b.category) || '').toLowerCase();
  let score = 0;
  for (const t of query.split(/\s+/).filter(Boolean)) {
    if (title === t) score += 100;
    else if (title.startsWith(t)) score += 60;
    else if (title.includes(t)) score += 40;
    if (author.includes(t)) score += 15;
    if (cat.includes(t)) score += 8;
  }
  return score;
}
function sortResultsByRelevance(items, q) {
  return items.sort((a, b) => {
    const d = relevanceScore(b, q) - relevanceScore(a, q);
    return d !== 0 ? d : providerOrder(a) - providerOrder(b);
  });
}

// 内容品类展示名：后端只给结构判定的 content_kind（novel/audio/video）与可选 kind_label；
// “听书”来自 book_type=1，“短剧/漫剧”来自 video_data 结构，均为上游事实，不靠书名猜。
const CONTENT_KIND_LABELS = { audio: '听书', video: '短剧/漫剧' };

function bookCardFrom(b, q) {
  const bidStr = String(b.book_id ?? '');
  // 标签：接口原字段直显（书旗 cornerTagExt 的原创/独家 + tags，番茄 tags），
  // 位置对齐其他源——放作者下方的 meta 行，标题行只留来源图标。
  const cat = (b.category || '').toString().trim();
  const seen = new Set(cat ? [cat] : []);
  const kind = b.content_kind && b.content_kind !== 'novel'
    ? (b.kind_label || CONTENT_KIND_LABELS[b.content_kind] || '')
    : '';
  const tagChips = [
    ...(kind ? [{ text: kind, cls: 'tag-kind' }] : []),
    ...(b.badges || []),
    ...(b.tags || []),
  ]
    .map(t => (typeof t === 'object' ? { ...t, text: String(t.text ?? '').trim() } : String(t ?? '').trim()))
    .filter(t => {
      const key = typeof t === 'object' ? t.text : t;
      return key && !seen.has(key) && seen.add(key);
    });
  return bookCard({
    query: q, cover: b.cover_url, title: b.title || bidStr, author: b.author,
    desc: b.description, score: b.score, meta: searchCardMeta(b),
    displayTitle: b.display_title, official: !!b.official_highlight,
    tagChips,
    badges: [sourceBadge(bidStr)],
    bookId: bidStr, scoreInFoot: true,
    foot: `<code class="book-card-id" title="Book ID">${esc(bidStr)}</code>`,
  });
}

// 合并多个 provider 的 meta，构建「工具栏模型」：每个分类 tab / 筛选项都带 providers 数组，
// 声明哪些源**真实支持**该条件（能力由各 provider 自身返回的 selector/tabs 决定，不硬编码）。
function buildToolbarModel(providerMetas) {
  const tabMap = new Map();  // tab_type -> {tab_type,title,providers:Set}
  const rowMap = new Map();  // row.key -> {row_name,type,selection_type,itemMap:Map(id->{...,providers:Set})}
  for (const { name, meta } of providerMetas) {
    if (!meta) continue;
    for (const t of (meta.tabs || [])) {
      const key = Number(t.tab_type);
      if (!tabMap.has(key)) tabMap.set(key, { tab_type: t.tab_type, title: t.title, providers: new Set() });
      tabMap.get(key).providers.add(name);
    }
    const rows = (meta.selector && meta.selector.rows) || [];
    for (const row of rows) {
      const rk = row.type || row.row_name || '';
      if (!rowMap.has(rk)) rowMap.set(rk, { row_name: row.row_name, type: row.type, selection_type: row.selection_type, itemMap: new Map() });
      const R = rowMap.get(rk);
      for (const it of (row.items || [])) {
        const ik = it.selector_item_id;
        if (!R.itemMap.has(ik)) R.itemMap.set(ik, { selector_item_id: ik, show_name: it.show_name, value: it.value, providers: new Set() });
        R.itemMap.get(ik).providers.add(name);
      }
    }
  }
  const tabs = [...tabMap.values()].map(t => ({ tab_type: t.tab_type, title: t.title, providers: [...t.providers] }));
  const rows = [...rowMap.values()].map(r => ({
    row_name: r.row_name, type: r.type, selection_type: r.selection_type,
    items: [...r.itemMap.values()].map(it => ({ selector_item_id: it.selector_item_id, show_name: it.show_name, value: it.value, providers: [...it.providers] })),
  }));
  return { tabs, rows };
}

// 当前 tab + 已选筛选项，计算「真正支持全部激活条件」且已被勾选的 provider 集合。
// tab=1（综合）或无已选项时不施加约束→返回全部勾选源。
function activeProvidersForState(s) {
  let active = new Set(selectedProviderSet());
  if (!s) return active;
  if (s.tab && Number(s.tab) !== 1) {
    const t = (s.tabs || []).find(x => Number(x.tab_type) === Number(s.tab));
    const sup = new Set(t && t.providers ? t.providers : []);
    active = new Set([...active].filter(p => sup.has(p)));
  }
  for (const id of s.selected.keys()) {
    let sup = new Set();
    for (const row of (s.rows || [])) for (const it of (row.items || [])) if (it.selector_item_id === id) (it.providers || []).forEach(p => sup.add(p));
    active = new Set([...active].filter(p => sup.has(p)));
  }
  return active;
}

function initSearchToolbar(q, model) {
  const bar = document.getElementById('searchToolbar');
  if (!bar) return;
  const tabs = (model && model.tabs) || [];
  const rows = (model && model.rows) || [];
  searchState = {
    q, tab: 1, selected: new Map(), offset: 0,
    tabs, rows, hasMore: false, nextOffset: 0,
    mode: 'merged', filtersOpen: false,
  };
  if (tabs.length <= 1 && rows.length === 0) { bar.classList.add('hidden'); bar.innerHTML = ''; return; }
  renderToolbar();
  bar.classList.remove('hidden');
}

function renderToolbar() {
  const bar = document.getElementById('searchToolbar');
  if (!bar || !searchState) return;
  const s = searchState;
  let html = '';
  const hasFilters = s.rows.length > 0;
  const showTabs = s.tabs.length > 1;
  const toggleBtn = hasFilters
    ? `<button type="button" id="sFilterToggle" class="stab s-filter-toggle${s.filtersOpen ? ' active' : ''}" title="展开/收起筛选">筛选 <span class="sf-caret">▾</span></button>`
    : '';
  if (showTabs) {
    html += '<div class="stabs">' + s.tabs.map(t => {
      const on = Number(t.tab_type) === s.tab;
      return `<button type="button" class="stab${on ? ' active' : ''}" data-tab="${t.tab_type}">${esc(t.title || t.tab_type)}</button>`;
    }).join('') + toggleBtn + '</div>';
  } else if (hasFilters) {
    // 只有筛选项、无多分类时（如仅七猫），“筛选 ▾”入口必须独立成行：
    // 不能把按钮寄生在分类条里，否则分类条不渲染时面板永远打不开/收不起。
    html += `<div class="stabs">${toggleBtn}</div>`;
  }
  if (hasFilters) {
    const groups = s.rows.map(row => {
      const items = (row.items || []).map(it => {
        const on = s.selected.has(it.selector_item_id);
        return `<button type="button" class="sfilter${on ? ' active' : ''}" data-item="${esc(it.selector_item_id)}" data-type="${esc(row.type || '')}">${esc(it.show_name || it.value)}</button>`;
      }).join('');
      return `<div class="srow"><span class="srow-name">${esc(row.row_name || '')}</span><span class="srow-items">${items}</span></div>`;
    }).join('');
    html += `<div class="sfilter-groups${s.filtersOpen ? '' : ' hidden'}">${groups}</div>`;
  }
  // 保留分类条横向滚动位置：整体重建 innerHTML 会把 .stabs 的 scrollLeft 重置为 0，
  // 导致点击“筛选”等按钮后分类条跳回最前。先记录、重建后恢复。
  const prevStabs = bar.querySelector('.stabs');
  const prevScrollLeft = prevStabs ? prevStabs.scrollLeft : 0;
  bar.innerHTML = html;
  const newStabs = bar.querySelector('.stabs');
  if (newStabs) newStabs.scrollLeft = prevScrollLeft;
  // 加载更多按钮置于结果列表末尾右下角
  refreshMoreButton();
}

function ensureToolbarBound() {
  const moreBox = document.getElementById('listMore');
  if (moreBox && !moreBox.__bound) {
    moreBox.__bound = true;
    moreBox.addEventListener('click', async (e) => {
      if (e.target && e.target.id === 'sMore') {
        await loadMoreProviders();
      }
    });
  }
  const bar = document.getElementById('searchToolbar');
  if (!bar || bar.__bound) return;
  bar.__bound = true;
  bar.addEventListener('click', async (e) => {
    const s = searchState;
    if (!s) return;
    if (e.target.closest('#sFilterToggle')) { s.filtersOpen = !s.filtersOpen; renderToolbar(); return; }
    const stab = e.target.closest('.stab');
    if (stab) {
      const tab = Number(stab.dataset.tab);
      s.tab = tab;
      s.offset = 0;
      if (tab === 1 && s.selected.size === 0) { s.mode = 'merged'; await doSearch(s.q); return; }
      s.mode = 'tab';
      await refreshTab();
      return;
    }
    const sf = e.target.closest('.sfilter');
    if (sf) {
      const id = sf.dataset.item, type = sf.dataset.type;
      if (s.selected.has(id)) s.selected.delete(id);
      else {
        for (const [k, v] of s.selected) if (v === type) s.selected.delete(k); // 同行单选
        s.selected.set(id, type);
      }
      s.offset = 0;
      s.mode = (s.tab === 1 && s.selected.size === 0) ? 'merged' : 'tab';
      if (s.mode === 'merged') { await doSearch(s.q); } else { await refreshTab(); }
      return;
    }
  });
}

async function refreshTab() {
  const s = searchState;
  if (!s) return;
  const prog = document.getElementById('searchProgress');
  const pt = document.getElementById('searchProgressText');
  const sel = [...s.selected.keys()].join(',');
  const active = activeProvidersForState(s);
  // 不支持当前分类/筛选条件的源：清空缓存并跳过（不搜索、不展示、不计入统计）。
  for (const nm of PROVIDER_ORDER) if (!active.has(nm)) searchProviderCache[nm] = [];
  const provSel = active;
  const tasks = [];
  try {
    if (prog) prog.classList.remove('hidden');
    if (pt) pt.textContent = '筛选中...';
    for (const name of PROVIDER_ORDER) {
      if (!provSel.has(name)) continue;
      let url;
      if (name === 'fanqie') {
        url = `/api/search?q=${encodeURIComponent(s.q)}&provider=fanqie&tab=${s.tab || 1}&selected_items=${encodeURIComponent(sel)}&offset=0`;
      } else if (name === 'qimao') {
        url = `/api/search?q=${encodeURIComponent(s.q)}&provider=qimao&tab=${s.tab || 1}&selected_items=${encodeURIComponent(sel)}&page=1`;
      } else {
        url = `/api/search?q=${encodeURIComponent(s.q)}&provider=shuqi&page=1`;
      }
      tasks.push((async () => {
        try {
          const data = await j(url);
          const items = data.items || [];
          for (const it of items) it._provider = name;
          searchProviderCache[name] = items;
          absorbProviderMeta(name, data);
          const phm = data.provider_has_more && data.provider_has_more[name];
          if (name === 'fanqie') {
            providerPages.fanqie = { hasMore: phm != null ? !!phm : !!data.has_more, nextOffset: data.next_offset || 0 };
          } else if (name === 'qimao') {
            providerPages.qimao = { hasMore: phm != null ? !!phm : false, page: 1 };
          } else {
            providerPages.shuqi = { hasMore: phm != null ? !!phm : false, page: 1 };
          }
        } catch {
          searchProviderCache[name] = [];
          providerPages[name] = { hasMore: false, page: 1 };
        }
      })());
    }
    await Promise.all(tasks);
  } finally {
    if (prog) prog.classList.add('hidden');
  }
  // 统一走合并渲染：会依据各源当前结果数重算并刷新右下角三平台统计。
  renderMergedResults(s.q);
  renderToolbar();
}

// ── 前端“只删除显示”隐藏集合（localStorage，不动后端） ──────────
const HIDDEN_JOBS_KEY = 'tnd.hidden_jobs';

function loadIdSet(key) {
  try { const a = JSON.parse(localStorage.getItem(key) || '[]'); return new Set((Array.isArray(a) ? a : []).map(String)); }
  catch { return new Set(); }
}
function saveIdSet(key, set) { try { localStorage.setItem(key, JSON.stringify([...set])); } catch {} }
function addHidden(key, id) { const s = loadIdSet(key); s.add(String(id)); saveIdSet(key, s); }

// 进行中任务卡片：与成品书卡同一套 bookCard 结构（封面/书名/作者/meta/简介/底栏），
// 差别只在底栏左侧用进度条代替“可更新”角标、右侧操作按钮换为任务操作（图标化、右对齐）。
// 数据全部来自任务自身（提交时携带的封面 + 本任务上游 meta），缺失字段一律不渲染。
// updating=true 表示该书磁盘上已有成品（重下/更新场景），打上“更新中”标记。
function jobCard(it, updating) {
  it = it || {};
  const m = it.meta || {};
  const p = it.progress || {};
  const saved = Number(p.saved_chapters) || 0;
  const total = Number(p.chapter_total) || 0;
  const pct = total > 0 ? Math.min(100, Math.round((saved / total) * 100)) : 0;
  let vState = (it.state || '').toLowerCase();
  if (vState === 'done' && total > 0 && saved < total) vState = 'partial';
  const hasCfg = (it.book_name_options || []).length > 0 || (it.format_options || []).length > 0;
  const bidStr = String(it.book_id || '');
  // 无名时回退到真实标识 book_id，不编造书名。
  const title = it.title || bidStr || '';
  const src = sourceBadge(bidStr);

  const badges = [];
  if (src.icon) badges.push({ icon: src.icon, text: src.text });
  // 下载中不重复放文字标签（百分比已在进度条里），只给需要处置/终态的状态。
  if (hasCfg) badges.push({ text: '待配置', cls: 'tag-orange' });
  else if (vState === 'queued') badges.push({ text: '排队中', cls: '' });
  else if (vState === 'failed') badges.push({ text: '失败', cls: 'tag-red' });
  else if (vState === 'partial') badges.push({ text: '部分失败', cls: 'tag-orange' });
  else if (vState === 'canceled') badges.push({ text: '已取消', cls: '' });
  if (updating && !hasCfg) badges.push({ text: '更新中', cls: 'tag-green' });

  const ICON_CANCEL = '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="m15 9-6 6"/><path d="m9 9 6 6"/></svg>';
  const ICON_RETRY = '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 1 1-2.64-6.36"/><path d="M21 3v6h-6"/></svg>';
  const ICON_CFG = '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 21v-6"/><path d="M4 11V3"/><path d="M12 21v-9"/><path d="M12 8V3"/><path d="M20 21v-4"/><path d="M20 13V3"/><path d="M2 15h4"/><path d="M10 6h4"/><path d="M18 17h4"/></svg>';
  const ICON_HIDE = '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></svg>';

  let acts = '';
  const jid = esc(it.id);
  if (hasCfg) {
    const kind = (it.book_name_options || []).length > 0 ? 'book_name' : 'format';
    acts += `<button type="button" data-jobid="${jid}" data-kind="${esc(kind)}" class="configJob icon-btn warning" title="配置后继续">${ICON_CFG}</button>`;
  } else if (vState === 'failed' || vState === 'partial' || vState === 'canceled') {
    acts += `<button type="button" data-jobid="${jid}" data-bookid="${esc(bidStr)}" class="retryJob icon-btn" title="重试">${ICON_RETRY}</button>`;
  } else {
    acts += `<button type="button" data-jobid="${jid}" class="cancelJob icon-btn danger" title="取消任务">${ICON_CANCEL}</button>`;
  }
  acts += `<button type="button" data-jobid="${jid}" class="hideJobBtn icon-btn ghost" title="从视图移除">${ICON_HIDE}</button>`;

  // 底栏左侧：进度条（取代成品卡的“可更新”角标位，保持同一 DOM 层级）。
  // 分工：条长 = 百分比，标题行徽章 = 状态词，文本 = 精短计数；
  // 两列窄屏下 foot 仅 60~90px，写全会被省略号截断，全量信息放 title 悬浮。
  const cnt = total > 0 ? `${saved}/${total}` : '';
  let fill = pct, indeterminate = false;
  if (hasCfg || vState === 'queued') { fill = 0; indeterminate = true; }
  const text = cnt || (vState === 'running' ? '下载中' : '');
  let tip = cnt ? `${cnt} 章 · ${pct}%` : '';
  if (vState === 'queued' && !cnt) tip = '排队等待中';
  if (hasCfg && !cnt) tip = '等待选择书名或输出格式';
  if (vState === 'failed' && it.message) tip = `失败：${it.message}`;
  const bar = `<span class="job-progress${indeterminate ? ' indeterminate' : ''}">` +
    `<span class="job-progress-fill" style="width:${fill}%"></span></span>`;
  const txtHtml = text ? `<span class="job-progress-text"${tip ? ` title="${esc(tip)}"` : ''}>${esc(text)}</span>` : '';
  const footLeft = `<span class="book-card-foot-left job-foot">${bar}${txtHtml}</span>`;
  const foot = footLeft + `<span class="book-card-actions">${acts}</span>`;

  return bookCard({
    cover: m.cover_url, title, author: it.author,
    desc: m.description, score: m.score, meta: searchCardMeta(m),
    tagChips: m.tags, badges, foot, bookId: '',
    cardClass: `job-card state-${esc(vState)}${hasCfg ? ' needscfg' : ''}`,
  });
}

function parseBookId(input) {
  const trimmed = (input ?? '').toString().trim();
  if (!trimmed) return '';
  if (/^[0-9]+$/.test(trimmed)) return trimmed;

  // 书旗（Shuqi）：sq: 前缀或 shuqi.com URL
  if (/^sq:[0-9]+$/i.test(trimmed)) return trimmed;
  try {
    const parsed = new URL(trimmed);
    if (/(^|\.)shuqi\.com$/.test(parsed.hostname.toLowerCase())) {
      const bid = parsed.searchParams.get('bid')
        || (parsed.pathname.match(/\/(?:book|reader)\/(\d+)/i) || [])[1];
      if (bid) return 'sq:' + bid;
    }
  } catch (_) { /* not a URL */ }

  // 七猫（Qimao）：qm: 前缀或 qimao.com / wtzw.com URL
  if (/^qm:[0-9]+$/i.test(trimmed)) return trimmed;
  try {
    const parsed = new URL(trimmed);
    const host = parsed.hostname.toLowerCase();
    if (/(^|\.)qimao\.com$/.test(host) || /(^|\.)wtzw\.com$/.test(host)) {
      const bid = parsed.searchParams.get('id')
        || parsed.searchParams.get('book_id')
        || parsed.searchParams.get('bid')
        || (parsed.pathname.match(/\/(?:book|detail|reader)\/(\d+)/i) || [])[1];
      if (bid) return 'qm:' + bid;
    }
  } catch (_) { /* not a URL */ }

  const urlMatch = trimmed.match(/https?:\/\/\S+/i);
  const target = urlMatch ? urlMatch[0] : trimmed;

  const qs = target.match(/(?:^|[?&#])(?:book_id|bookId)=([0-9]+)/i);
  if (qs && qs[1]) return qs[1];

  const page = target.match(/\/page\/([0-9]+)/i);
  if (page && page[1]) return page[1];

  // Short link (e.g. https://changdunovel.com/t/E_HDbOHpMJA/ or
  // https://zlink.fqnovel.com/dhVGe) – return the URL so the server can
  // follow the redirect and extract the book ID.
  // Restrict to known share-link hosts to prevent forwarding arbitrary URLs.
  const allowedShortLinkHosts = new Set(['changdunovel.com', 'www.changdunovel.com', 'fanqienovel.com', 'www.fanqienovel.com', 'fqnovel.com', 'www.fqnovel.com', 'zlink.fqnovel.com']);
  try {
    const parsed = new URL(target);
    if (
      (parsed.protocol === 'http:' || parsed.protocol === 'https:') &&
      allowedShortLinkHosts.has(parsed.hostname.toLowerCase())
    ) {
      // Standard short link: /t/<token>
      if (/^\/t\/[A-Za-z0-9_-]+\/?$/.test(parsed.pathname)) {
        return target;
      }
      // zlink-style short link: /<token> (only for zlink.fqnovel.com)
      if (parsed.hostname.toLowerCase() === 'zlink.fqnovel.com' &&
          /^\/[A-Za-z0-9_-]+\/?$/.test(parsed.pathname)) {
        return target;
      }
    }
  } catch (_) {
    // Not a valid absolute URL; ignore and fall through.
  }

  return '';
}

function isLikelyHeicUrl(url) {
  const s = (url || '').toString().toLowerCase();
  if (!s) return false;
  return /[\/.](heic|heif)(?:$|[?#])/i.test(s) || s.includes('format=heic') || s.includes('mime=image/heic');
}

function buildCoverCandidates(preview, hintCoverUrl) {
  const list = [];
  const add = (u) => {
    const v = (u || '').toString().trim();
    if (!v) return;
    if (!(v.startsWith('http://') || v.startsWith('https://') || v.startsWith('/'))) return;
    if (!list.includes(v)) list.push(v);
  };

  // 搜索结果中的封面 URL 优先（书旗 catalog API 可能不返回封面）
  add(hintCoverUrl);
  add(preview?.detail_cover_url);
  add(preview?.cover_url);

  const nonHeic = list.filter(u => !isLikelyHeicUrl(u));
  const heic = list.filter(isLikelyHeicUrl);
  return [...nonHeic, ...heic];
}

// ── Status ─────────────────────────────────────────────────────────

// 状态页 UI（版本/保存目录/监听地址/锁定状态）已从 index.html 移除，
// 原 refreshStatus() 一并删除；以下仅保留书名/格式弹窗仍在使用的模块变量。
let pendingBookNameJobId = null;
let pendingBookNameOptions = [];
let pendingFormatJobId = null;
let pendingFormatOptions = [];

// ── Library（下载库 = 已落库成品书 + 进行中任务卡片）──────────────────

let libraryBooksCache = []; // /api/library/books：磁盘成品 + 历史补全

// 任务数据分两层，配合 /api/jobs 的增量协议，保证轮询不重复拉相同数据：
//   jobsMap         动态层（state/progress/待配置项），1.5s 只收变化项
//   jobStaticMap    静态层（book_id/书名/作者/封面/简介），每个任务只拉一次
//   jobStaticDone   已取过静态层的任务 id，不重复请求
let jobsMap = new Map();
let jobStaticMap = new Map();
let jobStaticDone = new Set();
let jobsCursor = 0;     // /api/jobs 的 since 游标
let jobsEpoch = '';     // 后端任务表纪元，变化即说明内存任务表已换一批
let jobsActiveCount = 0; // 上一次轮询的活跃任务数，用于判断“有任务结束”
let prevActiveJobBids = new Set(); // 上一次轮询的活跃任务 book_id，用于识别“刚完成”的书触发单本重算

function resetJobsState() {
  jobsMap.clear();
  jobStaticMap.clear();
  jobStaticDone.clear();
  jobsCursor = 0;
  jobsActiveCount = 0;
}

function normBookId(v) {
  return String(v ?? '').trim().toLowerCase();
}

// 后到数据优先，但空字段不覆盖已有值：避免排队期本地播种的封面/书名被“尚无”抹掉。
function mergeJobStatic(prev, next) {
  if (!prev) return next;
  if (!next) return prev;
  const out = Object.assign({}, prev);
  for (const k of Object.keys(next)) {
    const v = next[k];
    if (v === null || v === undefined || v === '') continue;
    if (k === 'meta') continue;
    out[k] = v;
  }
  const nm = next.meta || {};
  const clean = {};
  for (const k of Object.keys(nm)) {
    if (nm[k] !== null && nm[k] !== undefined && nm[k] !== '' && !(Array.isArray(nm[k]) && !nm[k].length)) clean[k] = nm[k];
  }
  out.meta = Object.assign({}, prev.meta || {}, clean);
  return out;
}

// 合并视图：渲染与状态判定统一用它，字段语义与旧版 /api/jobs 项一致。
function jobView(slim) {
  const s = jobStaticMap.get(slim.id);
  return s ? Object.assign({}, s, slim) : slim;
}

// 当前任务列表（新→旧），已剔除“仅从视图隐藏”的项。
function jobsList() {
  const hidden = loadIdSet(HIDDEN_JOBS_KEY);
  const out = [];
  for (const it of jobsMap.values()) {
    if (hidden.has(String(it.id))) continue;
    out.push(jobView(it));
  }
  out.sort((a, b) => ((b.updated_ms || 0) - (a.updated_ms || 0)) || (b.id - a.id));
  return out;
}

// 任务是否“进行中/需处理”：待配置、排队、下载中、失败、部分失败、已取消；
// done 但章节没存完也归为“部分失败”，与卡片内的派生保持同一口径。
function isJobActive(it) {
  const s = (it && it.state || '').toLowerCase();
  const needCfg = (it && it.book_name_options || []).length > 0
    || (it && it.format_options || []).length > 0;
  if (needCfg || s === 'queued' || s === 'running' || s === 'failed'
    || s === 'partial' || s === 'canceled') return true;
  if (s === 'done') {
    const p = it.progress || {};
    const total = Number(p.chapter_total) || 0;
    return total > 0 && (Number(p.saved_chapters) || 0) < total;
  }
  return false;
}

// 卡片/小列表用的简短状态文案。
function jobStateText(it) {
  const needCfg = (it && it.book_name_options || []).length > 0
    || (it && it.format_options || []).length > 0;
  if (needCfg) return '待配置';
  const s = (it && it.state || '').toLowerCase();
  if (s === 'running') {
    const p = it.progress || {};
    const total = p.chapter_total || 0, saved = p.saved_chapters || 0;
    return total > 0 ? `下载中 ${Math.min(100, Math.round(saved / total * 100))}%` : '下载中';
  }
  if (s === 'queued') return '排队中';
  if (s === 'failed') return '失败';
  if (s === 'partial') return '部分失败';
  if (s === 'canceled') return '已取消';
  return it && it.state || '';
}

// 已落库成品书卡片：封面/书名/作者/简介/评分 + 徽章 + 下载/预览/删除。
function bookLibraryCard(b) {
  b = b || {};
  const ICON_DL = '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12"/><path d="m7 10 5 5 5-5"/><path d="M5 21h14"/></svg>';
  const ICON_DEL = '<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/><path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6"/></svg>';
  const bid = String(b.book_id || '');
  const mainRel = b.main_rel || '';
  const enc = encodePathSegments(mainRel);
  const dlHref = (b.dl_zip ? '/download-zip/' : '/download/') + enc;
  const badges = [];
  const coverFormat = b.format ? String(b.format).toUpperCase() : (b.main_is_dir ? '文件夹' : '');
  if (b.has_audio) badges.push({ text: '有声', cls: 'tag-orange' });
  badges.push(sourceBadge(bid));
  const newCount = Number(b.new_count) || 0;
  const updBadge = newCount > 0
    ? `<span class="update-badge" title="本地 ${Number(b.local_total) || 0}/${Number(b.remote_total) || 0} 章，可更新 +${newCount} 章">可更新 +${newCount}</span>`
    : '';
  const dlBtn = `<button type="button" class="libDl icon-btn" data-href="${esc(dlHref)}" title="下载成品文件">${ICON_DL}</button>`;
  const delBtn = `<button type="button" class="libDelete icon-btn danger" data-paths="${esc(JSON.stringify(b.paths || [mainRel]))}" data-title="${esc(b.title || b.stem || '')}" title="删除文件">${ICON_DEL}</button>`;
  const foot = `<span class="book-card-foot-left"></span><span class="book-card-actions">${dlBtn}${delBtn}</span>`;
  return bookCard({
    cover: b.cover_url, title: b.title || b.stem, author: b.author,
    desc: b.description, score: b.score, meta: searchCardMeta(b),
    badges, foot, bookId: bid, coverFormat, coverBadgeDown: updBadge,
  });
}

// 下载库视图：同一本书只允许一张卡——进行中卡优先，磁盘成品卡让位；
// 让位的那本书记入 downloadedBids，进行中卡因此打上“更新中”。
function libraryView() {
  const active = jobsList().filter(isJobActive);
  const downloadedBids = new Set(libraryBooksCache.map(b => normBookId(b.book_id)).filter(Boolean));
  const activeBids = new Set(active.map(it => normBookId(it.book_id)).filter(Boolean));
  const books = libraryBooksCache.filter(b => !activeBids.has(normBookId(b.book_id)));
  return { active, books, downloadedBids };
}

function jobCardsHtml(active, downloadedBids) {
  return active
    .map(it => jobCard(it, downloadedBids.has(normBookId(it.book_id))))
    .join('');
}

// 渲染下载库网格：进行中任务卡片与已下载书卡同级展示，按书名过滤成品书。
function renderLibraryGrid() {
  const grid = document.getElementById('libraryBooks');
  if (!grid) return;
  const filterEl = document.getElementById('libFilter');
  const kw = (filterEl && filterEl.value || '').toString().trim().toLowerCase();
  const { active, books, downloadedBids } = libraryView();
  const shown = kw
    ? books.filter(b => ((b.title || '') + ' ' + (b.stem || '')).toLowerCase().includes(kw))
    : books;
  const cards = jobCardsHtml(active, downloadedBids) + shown.map(bookLibraryCard).join('');
  if (!cards) {
    grid.innerHTML = '<div class="grid-empty">暂无已下载小说，先去下载一本书吧</div>';
  } else {
    grid.innerHTML = cards;
  }
  const hint = document.getElementById('libHint');
  if (hint) hint.textContent = `已下载 ${libraryBooksCache.length} 本` + (active.length ? ` · 进行中 ${active.length}` : '');
}

// 拉取磁盘成品书并渲染（下载/删除/任务结束后调用）。
async function refreshLibrary() {
  const data = await j('/api/library/books');
  libraryBooksCache = (data && data.books) || [];
  renderLibraryGrid();
  renderRecentList();
}

// ── Search ─────────────────────────────────────────────────────────

// 分列设置按钮：仅在有搜索结果时出现。
function setListHeadVisible(v) { const lh = document.querySelector('.list-head'); if (lh) lh.classList.toggle('hidden', !v); }
// 搜索框清空叉号：有输入时显示。
function updateSearchClear() { const q = document.getElementById('q'); const c = document.getElementById('searchClear'); if (q && c) c.classList.toggle('hidden', !q.value); }

async function doSearch(q) {
  const out = document.getElementById('searchResults');
  out.innerHTML = '';
  out.classList.add('hidden');
  const tb = document.getElementById('searchToolbar');
  if (tb) { tb.classList.add('hidden'); tb.innerHTML = ''; }
  // 搜索时收起首页面板，突出结果
  const panels = document.getElementById('homePanels');
  if (panels) panels.classList.add('hidden');
  setListHeadVisible(false);
  if (!q) return;
  searchProviderCache = {};
  providerPages = {};
  lastSearchQuery = q;

  // 进度显示
  const progressEl = document.getElementById('searchProgress');
  const progressText = document.getElementById('searchProgressText');
  const showProgress = (text) => {
    if (progressEl) progressEl.classList.remove('hidden');
    if (progressText) progressText.textContent = text;
  };
  const hideProgress = () => {
    if (progressEl) progressEl.classList.add('hidden');
  };

  // 搜索源：按页面筛选（至少一个，由 #providerFilter 的 change 处理保证）
  const chosen = PROVIDER_REGISTRY.filter(p => selectedProviderSet().has(p.name));
  if (chosen.length === 0) {
    // 未选择任何搜索源：明确提示用户，而非静默回退到某个源。
    showSearchHint('请至少选择一个搜索源', 0);
    return;
  }
  const providers = chosen;
  const totalProviders = providers.length;
  let completedProviders = 0;
  let allItems = [];
  const counts = {}; // name -> {label, count, failed}
  providers.forEach(p => { counts[p.name] = { label: p.label, count: 0, failed: false }; });

  showProgress(`搜索中... 已完成 0/${totalProviders} 个源`);
  document.getElementById('searchCounts')?.classList.add('hidden');

  // 并发请求各搜索源（各源响应里的 provider_meta 逐源吸收进能力缓存）
  const promises = providers.map(async (p) => {
    try {
      const data = await j(`/api/search?q=${encodeURIComponent(q)}&provider=${p.name}`);
      const items = data.items || [];
      for (const it of items) it._provider = p.name;
      searchProviderCache[p.name] = items;
      absorbProviderMeta(p.name, data);
      const phm = data.provider_has_more && data.provider_has_more[p.name];
      providerPages[p.name] = p.name === 'fanqie'
        ? { hasMore: phm != null ? !!phm : !!data.has_more, nextOffset: data.next_offset || 0 }
        : { hasMore: phm != null ? !!phm : false, page: 1 };
      completedProviders++;
      counts[p.name].count = items.length;
      showProgress(`搜索中... 已完成 ${completedProviders}/${totalProviders} 个源（${p.label} ${items.length} 条）`);
      return items;
    } catch (err) {
      completedProviders++;
      counts[p.name].failed = true;
      showProgress(`搜索中... 已完成 ${completedProviders}/${totalProviders} 个源（${p.label} 失败）`);
      return [];
    }
  });

  const results = await Promise.all(promises);
  for (const items of results) {
    allItems = allItems.concat(items);
  }
  sortResultsByRelevance(allItems, q);

  hideProgress();
  renderSearchCounts(counts);

  if (allItems.length === 0) {
    out.innerHTML = '<div class="grid-empty">无结果</div>';
    out.classList.remove('hidden');
    setListHeadVisible(false);
    return;
  }
  out.innerHTML = allItems.map(b => bookCardFrom(b, q)).join('');
  out.classList.remove('hidden');
  setListHeadVisible(allItems.length > 0);
  ensureToolbarBound();
  // 工具栏由「当前勾选源的能力并集」重建：哪个源自报了分类/筛选就显示哪个的，
  // 都没自报（如仅书旗）则整个工具栏隐藏。
  syncToolbar();
  // 合并模式下把当前结果种入 searchState.items，使“加载更多”能在其后
  // 追加下一页（refreshTab 依赖 s.items 累积），而非整体替换。
  if (searchState) searchState.items = allItems;
  refreshMoreButton();
}

// 搜索源选中集合（来自 #providerFilter 复选框）。
function selectedProviderSet() {
  const set = new Set();
  document.querySelectorAll('#providerFilter input[type=checkbox]').forEach(cb => { if (cb.checked) set.add(cb.value); });
  return set;
}

// 依据当前选中搜索源，从缓存合并渲染结果（增一个源→追加该源；减一个源→结果随之减少）。
function renderMergedResults(q) {
  const out = document.getElementById('searchResults');
  if (!out) return;
  const cbSel = selectedProviderSet();
  const sel = searchState ? activeProvidersForState(searchState) : cbSel;
  const order = PROVIDER_ORDER;
  const labels = PROVIDER_LABELS;
  let items = [];
  const counts = {};
  for (const p of order) {
    if (!sel.has(p)) continue;
    const arr = searchProviderCache[p] || [];
    items = items.concat(arr);
    counts[p] = { label: labels[p], count: arr.length, failed: false };
  }
  sortResultsByRelevance(items, q);
  out.innerHTML = items.length
    ? items.map(b => bookCardFrom(b, q)).join('')
    : '<div class="grid-empty">无结果</div>';
  out.classList.remove('hidden');
  setListHeadVisible(items.length > 0);
  renderSearchCounts(counts);
  if (searchState) searchState.items = items;
  // 番茄未选中时隐藏其分类/筛选工具栏；加载更多按钮按各源是否还有更多独立刷新。
  const tb = document.getElementById('searchToolbar');
  const hasModel = searchState && (((searchState.tabs || []).length > 1) || (searchState.rows || []).length > 0);
  if (tb) tb.classList.toggle('hidden', !((cbSel.has('fanqie') || cbSel.has('qimao')) && hasModel));
  refreshMoreButton();
}

// 各源搜索数量展示。
function renderSearchCounts(counts) {
  const el = document.getElementById('searchCounts');
  if (!el) return;
  const iconOf = { fanqie: '/assets/fqnovel.webp', shuqi: '/assets/sqnovel.png', qimao: '/assets/qmnovel.webp' };
  const labelOf = { fanqie: '番茄', shuqi: '书旗', qimao: '七猫' };
  const parts = Object.entries(counts).map(([k, c]) => {
    const n = c.failed ? '—' : c.count;
    const ico = iconOf[k];
    const lead = ico
      ? `<img class="count-ico" src="${ico}" alt="${esc(labelOf[k] || '')}" title="${esc(labelOf[k] || k)}">`
      : `<span>${esc(c.label || k)}</span>`;
    return `<span class="count-item">${lead}<span>${n}</span></span>`;
  });
  el.innerHTML = parts.join('<span class="count-sep">·</span>');
  el.classList.remove('hidden');
}

// ── Preview ────────────────────────────────────────────────────────

// ── Search Home: history + recent downloads ────────────────────────

let homeSearchActive = false;

// 搜索记录已改为服务端落库（跨设备同步）。本地仅保留一份内存缓存供渲染。
let searchHistoryCache = [];

function renderSearchHistory() {
  const box = document.getElementById('searchHistory');
  const clearBtn = document.getElementById('clearSearchHistory');
  if (!box) return;
  if (clearBtn) clearBtn.classList.toggle('hidden', searchHistoryCache.length === 0);
  if (searchHistoryCache.length === 0) { box.innerHTML = '<span class="k">暂无搜索记录</span>'; return; }
  box.innerHTML = '';
  for (const r of searchHistoryCache) {
    const chip = document.createElement('button');
    chip.type = 'button';
    chip.className = 'chip';
    chip.textContent = r.keyword;
    chip.addEventListener('click', () => {
      const inp = document.getElementById('q');
      if (inp) inp.value = r.keyword;
      updateSearchClear();
      runSearch(r.keyword);
    });
    box.appendChild(chip);
  }
}

async function refreshSearchHistory() {
  try {
    const data = await j('/api/search-history');
    searchHistoryCache = data.items || [];
  } catch {
    searchHistoryCache = [];
  }
  renderSearchHistory();
}

async function recordSearchHistory(keyword) {
  const kw = (keyword || '').toString().trim();
  if (!kw) return;
  try {
    const data = await j('/api/search-history', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ keyword: kw }),
    });
    if (data && data.items) searchHistoryCache = data.items;
  } catch { /* 忽略记录失败，不影响搜索 */ }
  renderSearchHistory();
}

async function clearSearchHistory() {
  try { await j('/api/search-history', { method: 'DELETE' }); } catch {}
  searchHistoryCache = [];
  renderSearchHistory();
}

async function loadRecentDownloads() {
  const box = document.getElementById('recentDownloads');
  if (!box) return;
  if (!libraryBooksCache.length) { try { await refreshLibrary(); } catch { /* ignore */ } }
  else renderRecentList();
}

// 首页“最近下载”小列表：与下载库同一口径（同书不双卡，进行中卡标“更新中”）。
function renderRecentList() {
  const box = document.getElementById('recentDownloads');
  if (!box) return;
  const { active, books, downloadedBids } = libraryView();
  const cards = jobCardsHtml(active, downloadedBids) + books.slice(0, 6).map(bookLibraryCard).join('');
  box.innerHTML = cards || '<span class="k">暂无已下载书籍</span>';
}

// 重置到搜索前首页状态（清空结果/工具栏/统计，恢复首页面板）。
function resetSearchHome() {
  homeSearchActive = false;
  lastSearchQuery = '';
  searchProviderCache = {};
  setListHeadVisible(false);
  updateSearchClear();
  const out = document.getElementById('searchResults');
  if (out) { out.innerHTML = ''; out.classList.add('hidden'); }
  const tb = document.getElementById('searchToolbar');
  if (tb) { tb.classList.add('hidden'); tb.innerHTML = ''; }
  searchState = null;
  const more = document.getElementById('listMore');
  if (more) { more.innerHTML = ''; more.classList.add('hidden'); }
  const cnt = document.getElementById('searchCounts');
  if (cnt) cnt.classList.add('hidden');
  const hint = document.getElementById('searchHint');
  if (hint) hint.textContent = '';
  const panels = document.getElementById('homePanels');
  if (panels) panels.classList.remove('hidden');
  refreshSearchHistory().catch(() => {});
  loadRecentDownloads().catch(() => {});
}

// 统一搜索入口：book_id/链接直接下载（不记历史），名称搜索记入历史。
async function runSearch(rawQ) {
  const q = (rawQ || '').toString().trim();
  const hint = document.getElementById('searchHint');
  if (!q) {
    resetSearchHome();
    return;
  }
  if (hint) hint.textContent = '';
  homeSearchActive = true;

  const bookId = parseBookId(q);
  if (bookId) {
    const panels = document.getElementById('homePanels');
    if (panels) panels.classList.add('hidden');
    try {
      await startDownload(bookId);
      if (hint) hint.textContent = `已打开预览：${bookId}`;
      const out = document.getElementById('searchResults');
      if (out) { out.innerHTML = '<div class="grid-empty">已打开书籍预览，可在弹窗中确认下载</div>'; out.classList.remove('hidden'); }
    } catch (err) {
      if (hint) hint.textContent = '创建任务失败';
      alert(err);
    }
    return;
  }
  recordSearchHistory(q);
  try { await doSearch(q); } catch (err) { alert(err); }
}

let currentPreviewBookId = null;
let currentPreviewData = null;
let currentPreviewCoverUrl = null;
let previewRequestSerial = 0;
let previewAbortController = null;

function showPreviewModal(show) {
  const modal = document.getElementById('previewModal');
  if (!modal) return;
  modal.classList.toggle('hidden', !show);
  document.body.style.overflow = show ? 'hidden' : '';
  if (!show) {
    const bookIdToCleanup = currentPreviewBookId;
    previewRequestSerial += 1;
    if (previewAbortController) {
      previewAbortController.abort();
      previewAbortController = null;
    }
    // 关闭预览时，清理服务端因预览产生的封面缓存文件夹
    if (bookIdToCleanup) {
      fetchWithCreds(`/api/preview/${encodeURIComponent(bookIdToCleanup)}/cleanup`, {
        method: 'POST',
      }).catch(() => {}); // fire-and-forget
    }
    currentPreviewBookId = null;
    currentPreviewData = null;
  }
}

// ── 预览本地缓存（浏览器级） ────────────────────────
const PREVIEW_CACHE_KEY = 'tnd.preview_cache';
const PREVIEW_CACHE_MAX = 40;
function loadPreviewCache() {
  try { const o = JSON.parse(localStorage.getItem(PREVIEW_CACHE_KEY) || '{}'); return (o && typeof o === 'object') ? o : {}; }
  catch { return {}; }
}
function getPreviewCache(bookId) { return loadPreviewCache()[String(bookId)] || null; }
function setPreviewCache(bookId, data) {
  try {
    const store = loadPreviewCache();
    store[String(bookId)] = { data, ts: Date.now() };
    // 简单淘汰：超过上限时删除最旧的。
    const keys = Object.keys(store);
    if (keys.length > PREVIEW_CACHE_MAX) {
      keys.sort((a, b) => (store[a].ts || 0) - (store[b].ts || 0));
      for (const k of keys.slice(0, keys.length - PREVIEW_CACHE_MAX)) delete store[k];
    }
    localStorage.setItem(PREVIEW_CACHE_KEY, JSON.stringify(store));
  } catch {}
}

// 当前预览的来源侧书名（搜索结果/下载库卡片已知值），作为 hint 传给 /api/preview，
// 用于番茄网页详情页 404 时保留“用户看到的名字”（响应会标 book_name_from_hint）。
let currentPreviewHintTitle = null;

async function openPreview(bookId, hintCoverUrl, force, hintTitle) {
  if (previewAbortController) previewAbortController.abort();
  const requestSerial = ++previewRequestSerial;
  const abortController = new AbortController();
  previewAbortController = abortController;
  currentPreviewBookId = bookId;
  currentPreviewData = null;
  currentPreviewCoverUrl = (hintCoverUrl || '').toString() || null;
  currentPreviewHintTitle = (hintTitle || '').toString() || null;
  showPreviewModal(true);

  const loading = document.getElementById('previewLoading');
  const rangeInput = document.getElementById('previewRangeInput');
  const rangeHint = document.getElementById('previewRangeHint');
  if (rangeInput) rangeInput.value = '';
  if (rangeHint) { rangeHint.textContent = ''; rangeHint.classList.remove('error'); }

  // 1) 缓存优先：命中则立即渲染（秒开），后台再拉新。
  const cached = force ? null : getPreviewCache(bookId);
  if (cached && cached.data) {
    currentPreviewData = cached.data;
    applyPreview(cached.data, hintCoverUrl, requestSerial, bookId, true);
    if (loading) loading.classList.add('hidden');
  } else {
    if (loading) { loading.textContent = '加载中...'; loading.classList.remove('hidden'); }
    const dataEl = document.getElementById('previewData');
    if (dataEl) dataEl.classList.add('hidden');
  }

  // 2) 后台拉取最新（stale-while-revalidate），失败时保留缓存。
  try {
    const qs = currentPreviewHintTitle
      ? `?hint_title=${encodeURIComponent(currentPreviewHintTitle)}`
      : '';
    const preview = await j(`/api/preview/${encodeURIComponent(bookId)}${qs}`, {
      signal: abortController.signal,
    });
    if (requestSerial !== previewRequestSerial || currentPreviewBookId !== bookId) return;
    currentPreviewData = preview;
    if (preview.book_id) currentPreviewBookId = preview.book_id;
    applyPreview(preview, hintCoverUrl, requestSerial, bookId, false);
    // 预览过一次→写入缓存；同时自动补档（若之前无有效存档）。
    setPreviewCache(preview.book_id || bookId, preview);
    fetchWithCreds(`/api/preview/${encodeURIComponent(preview.book_id || bookId)}/archive`, { method: 'POST' })
      .then(() => { loadRecentDownloads().catch(() => {}); })
      .catch(() => {});
  } catch (err) {
    if (abortController.signal.aborted || requestSerial !== previewRequestSerial || currentPreviewBookId !== bookId) return;
    if (!cached) { if (loading) loading.textContent = `加载失败: ${err}`; console.error('Preview load error:', err); }
  } finally {
    if (previewAbortController === abortController) previewAbortController = null;
  }
}

// 将 preview 数据填充到弹窗 DOM。
function applyPreview(preview, hintCoverUrl, requestSerial, bookId, fromCache) {
  const loading = document.getElementById('previewLoading');
  const data = document.getElementById('previewData');
  const rangeHint = document.getElementById('previewRangeHint');
  if (loading) loading.classList.add('hidden');
  if (data) data.classList.remove('hidden');

  // 缓存标识提示
  const cacheHintEl = document.getElementById('previewCacheHint');
  if (cacheHintEl) {
    cacheHintEl.textContent = fromCache ? '（缓存，正在更新…）' : '';
    cacheHintEl.classList.toggle('hidden', !fromCache);
  }

  const title = document.getElementById('previewTitle');
  const origTitle = document.getElementById('previewOrigTitle');
  const author = document.getElementById('previewAuthor');
  const stats = document.getElementById('previewStats');
  const desc = document.getElementById('previewDesc');
  const tags = document.getElementById('previewTags');
  const chapters = document.getElementById('previewChapters');
  const cover = document.getElementById('previewCover');

  if (title) {
    // 不再用“未知书名”这种看起来像数据的占位假值：没名字就回退到真实标识 book_id，
    // 并由顶部告警条说明为何缺失。
    title.textContent = preview.book_name || preview.book_id || '';
  }

  // 网页元数据缺失（番茄详情页 404 / 抓取失败）→ 明确告知，不静默展示空壳。
  // 文案不写死“哪些不可用”，而是按本次响应里字段的真实有无生成，
  // 避免声称“封面不可用”却把封面正常显示出来这种归属错误。
  const warn = document.getElementById('previewMetaWarn');
  if (warn) {
    if (preview.meta_missing) {
      const miss = [];
      if (!preview.author) miss.push('作者');
      if (!preview.description) miss.push('简介');
      if (!(Array.isArray(preview.tags) && preview.tags.length)) miss.push('标签');
      if (preview.category == null) miss.push('分类');
      if (preview.score == null) miss.push('评分');
      if (preview.word_count == null) miss.push('字数');
      if (preview.finished == null) miss.push('连载状态');
      const src = preview.book_name_from_hint ? '（书名来自搜索结果，非详情页）' : '';
      warn.textContent = '⚠ 番茄网页详情页不存在（404）或信息抓取失败'
        + src + '：' + (miss.length ? miss.join(' / ') : '部分字段')
        + '本次未能从网页获取；目录与下载不受影响。';
      warn.classList.remove('hidden');
    } else {
      warn.classList.add('hidden');
      warn.textContent = '';
    }
  }

  if (origTitle) {
    if (preview.original_book_name && preview.original_book_name !== preview.book_name) {
      origTitle.textContent = `原名: ${preview.original_book_name}`;
      origTitle.classList.remove('hidden');
    } else {
      origTitle.classList.add('hidden');
    }
  }

  if (author) {
    // 作者为空则整行不渲染（零补全：不写“作者: 未知”）。
    if (preview.author) {
      author.textContent = `作者: ${preview.author}`;
      author.classList.remove('hidden');
    } else {
      author.textContent = '';
      author.classList.add('hidden');
    }
  }

  if (stats) {
    const parts = [];
    if (preview.category) parts.push(`分类: ${preview.category}`);
    if (preview.chapter_count) parts.push(`章节: ${preview.chapter_count}`);
    if (preview.finished !== null && preview.finished !== undefined) {
      parts.push(`状态: ${preview.finished ? '完结' : '连载'}`);
    }
    if (preview.word_count) {
      const words = Number(preview.word_count);
      parts.push(`字数: ${words >= 10000 ? (words / 10000).toFixed(1) + '万' : words}字`);
    }
    if (preview.score != null) parts.push(`评分: ${Number(preview.score).toFixed(1)}`);
    if (preview.read_count_text || preview.read_count) {
      parts.push(`阅读: ${preview.read_count_text || preview.read_count}`);
    }
    stats.innerHTML = '';
    parts.forEach(p => {
      const span = document.createElement('span');
      span.textContent = p;
      stats.appendChild(span);
    });
  }

  if (desc) desc.textContent = preview.description || '暂无简介';

  if (tags) {
    if (preview.tags && preview.tags.length > 0) {
      tags.innerHTML = '';
      preview.tags.forEach(t => {
        const badge = document.createElement('span');
        badge.className = 'badge';
        badge.textContent = t;
        tags.appendChild(badge);
      });
      tags.classList.remove('hidden');
    } else {
      tags.classList.add('hidden');
    }
  }

  if (chapters) {
    const chapterInfo = [];
    if (preview.chapter_count) chapterInfo.push(`总章节数: ${preview.chapter_count}`);
    if (preview.first_chapter_title) chapterInfo.push(`首章: ${preview.first_chapter_title}`);
    if (preview.last_chapter_title) chapterInfo.push(`末章: ${preview.last_chapter_title}`);
    if (preview.category) chapterInfo.push(`分类: ${preview.category}`);
    chapters.innerHTML = '';
    chapterInfo.forEach(info => {
      const div = document.createElement('div');
      div.textContent = info;
      chapters.appendChild(div);
    });
  }

  if (cover) {
    const candidates = buildCoverCandidates(preview, hintCoverUrl);
    let coverIdx = 0;
    const loadCandidate = () => {
      if (coverIdx >= candidates.length) {
        cover.removeAttribute('src');
        cover.classList.add('hidden');
        cover.onerror = null;
        return;
      }
      cover.src = candidates[coverIdx++];
      cover.classList.remove('hidden');
    };
    cover.onerror = () => loadCandidate();
    if (candidates.length > 0) { loadCandidate(); }
    else { cover.removeAttribute('src'); cover.classList.add('hidden'); cover.onerror = null; }
  }

  if (rangeHint && preview.chapter_count) {
    rangeHint.textContent = `例如: 1-10 下载第1到第10章，1-${preview.chapter_count} 下载全部`;
  }

  // 若该书有可更新内容，确认按钮文案改为“确认更新”。
  const confirmBtn = document.getElementById('previewConfirm');
  if (confirmBtn) {
    const pbid = String((preview && preview.book_id) || bookId || '');
    const hit = (libraryBooksCache || []).find(x => String(x.book_id) === pbid);
    confirmBtn.textContent = (hit && Number(hit.new_count) > 0) ? '确认更新' : '确认下载';
  }
}

async function confirmPreview() {
  if (!currentPreviewBookId || !currentPreviewData) { showPreviewModal(false); return; }

  const bookId = currentPreviewBookId;
  const coverUrl = currentPreviewCoverUrl || '';
  const rangeInput = document.getElementById('previewRangeInput');
  const rangeHint = document.getElementById('previewRangeHint');
  const rangeText = rangeInput ? rangeInput.value.trim() : '';

  let rangeStart = null;
  let rangeEnd = null;

  if (rangeText) {
    const total = currentPreviewData.chapter_count || 0;
    if (total === 0) {
      if (rangeHint) { rangeHint.textContent = '章节数未知，无法使用范围下载'; rangeHint.classList.add('error'); }
      return;
    }
    const parts = rangeText.split('-').map(p => p.trim());
    if (parts.length === 2) {
      const start = parts[0] === '' ? 1 : parseInt(parts[0], 10);
      const end = parts[1] === '' ? total : parseInt(parts[1], 10);
      if (isNaN(start) || isNaN(end) || start < 1 || end < 1 || start > end || end > total) {
        if (rangeHint) { rangeHint.textContent = `范围无效 (1-${total})`; rangeHint.classList.add('error'); }
        return;
      }
      rangeStart = start;
      rangeEnd = end;
    } else {
      if (rangeHint) { rangeHint.textContent = '格式应为 start-end，例如 1-10'; rangeHint.classList.add('error'); }
      return;
    }
  }

  if (rangeHint) rangeHint.classList.remove('error');
  showPreviewModal(false);

  try {
    const payload = { book_id: bookId };
    if (coverUrl) payload.cover_url = coverUrl;
    if (rangeStart !== null && rangeEnd !== null) {
      payload.range_start = rangeStart;
      payload.range_end = rangeEnd;
    }
    const resp = await j('/api/jobs', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(payload)
    });
    if (resp && resp.id) {
      const pv = currentPreviewData || {};
      seedJobStatic(resp.id, {
        book_id: resp.book_id || bookId,
        title: pv.book_name || '',
        author: pv.author || '',
        meta: { cover_url: resp.cover_url || '' },
      });
    }
    await refreshJobs();
    window.location.hash = '#library';
    refreshLibrary().catch(() => {});
    const hint = document.getElementById('searchHint');
    if (hint) {
      hint.textContent = rangeStart && rangeEnd
        ? `已创建下载任务：${bookId} (章节 ${rangeStart}-${rangeEnd})`
        : `已创建下载任务：${bookId}`;
    }
  } catch (err) {
    alert(`创建任务失败: ${err}`);
  }
}

async function startDownload(bookId, coverUrl) {
  await openPreview(bookId, coverUrl);
  return null;
}

async function startDownloadDirect(bookId) {
  const job = await j('/api/jobs', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ book_id: bookId })
  });
  if (job && job.id) {
    seedJobStatic(job.id, { book_id: job.book_id || bookId, meta: { cover_url: job.cover_url || '' } });
  }
  await refreshJobs();
  return job;
}

// ── Jobs ───────────────────────────────────────────────────────────

// 取任务静态层（书名/作者/封面/简介）：每个任务只拉一次，不随轮询重复传输。
async function fetchJobStatic(ids) {
  if (!ids.length) return;
  let data;
  try { data = await j('/api/jobs/meta?ids=' + encodeURIComponent(ids.join(','))); } catch { return; }
  const map = (data && data.items) || {};
  for (const k of Object.keys(map)) {
    const id = Number(k);
    if (!jobsMap.has(id)) continue; // 本轮已被移除：不写入陈旧元数据
    jobStaticMap.set(id, mergeJobStatic(jobStaticMap.get(id), map[k]));
    jobStaticDone.add(k);
  }
}

// 任务静态数据本地播种：book_id/封面/书名/作者都是提交前预览卡上已展示过的该书真实数据，
// 先给卡片用，避免“排队中却无书名”的空白期（随后被任务自身元数据覆盖为权威值）。
function seedJobStatic(id, obj) {
  const key = Number(id);
  if (!key) return;
  jobStaticMap.set(key, mergeJobStatic(jobStaticMap.get(key), obj));
}

// 增量同步：只拉 updated_ms 大于游标的变化项 + 该时刻后被移除的 id（墓碑）。
// 无变化时 items 为空，1.5s 轮询 payload 接近零。full=true 则回到 since=0 完整重建。
async function syncJobs(full) {
  let data;
  try { data = await j(`/api/jobs?since=${full ? 0 : jobsCursor}`); } catch { return; }

  // 后端进程重启 → 内存任务表已清空，本地缓存必须作废重建，否则会残留幽灵卡片。
  const epoch = data.epoch_ms ? String(data.epoch_ms) : '';
  if (jobsEpoch && epoch && jobsEpoch !== epoch) {
    jobsEpoch = epoch;
    resetJobsState();
    return syncJobs(true);
  }
  if (!jobsEpoch && epoch) jobsEpoch = epoch;

  for (const id of (data.removed || [])) {
    jobsMap.delete(id);
    jobStaticMap.delete(id);
    jobStaticDone.delete(String(id));
  }
  for (const it of (data.items || [])) jobsMap.set(it.id, it);
  if (typeof data.cursor === 'number') jobsCursor = Math.max(jobsCursor, data.cursor);

  // 仅对“元数据已就绪且尚未拉过”的任务发一次批量拉取（has_meta=false 时不请求，不会空转）。
  const need = [];
  for (const it of jobsMap.values()) {
    if (it.has_meta && !jobStaticDone.has(String(it.id))) need.push(it.id);
  }
  if (need.length) await fetchJobStatic(need);

  const list = jobsList();
  const activeNow = list.filter(isJobActive);
  const nowActive = activeNow.length;
  const curBids = new Set(activeNow.map(it => String(it.book_id || '').trim()).filter(Boolean));
  const finishedBids = [...prevActiveJobBids].filter(b => !curBids.has(b));
  prevActiveJobBids = curBids;
  // 有任务结束 → 重扫磁盘，新落库成品接管展示（同书不双卡）。
  if (nowActive < jobsActiveCount) {
    refreshLibrary().catch(() => {});
  }
  // #6：对刚下载完的书单本重算远端/本地章节数并回写后端快照，避免“可更新”徽标滞留在下载前的旧值。
  if (finishedBids.length) {
    Promise.all(finishedBids.map(bid =>
      j('/api/updates/refresh-one?book_id=' + encodeURIComponent(bid), { method: 'POST' }).catch(() => {})
    )).then(() => refreshLibrary().catch(() => {})).catch(() => {});
  }
  jobsActiveCount = nowActive;
  renderLibraryGrid();
  renderRecentList();
  for (const it of list) {
    if ((it.state || '').toLowerCase() === 'failed' && it.message) maybeShowIidWarningFromError(it.message);
  }
}

// 显式刷新（创建/取消/重试/配置提交后）：做一次完整同步，不依赖游标追上刚发生的状态变更。
function refreshJobs() { return syncJobs(true); }
// 定时轮询：只收变化项。
function pollJobs() { return syncJobs(false); }

// ── Updates ────────────────────────────────────────────────────────

let updatesPollTimer = null;

function scheduleUpdatesPoll() {
  if (updatesPollTimer) clearTimeout(updatesPollTimer);
  updatesPollTimer = setTimeout(() => {
    updatesPollTimer = null;
    refreshUpdates(false).catch(() => {});
  }, 2000);
}

// 触发/轮询可更新扫描。徽标数据已并入 /api/library/books，本函数只负责“跑扫描 + 完成后重取库数据”。
// 冷启动扫描已由服务启动时 boot_scan 负责，页面加载不再调用本函数。
async function refreshUpdates(start = true) {
  let data;
  try {
    data = await j(start ? '/api/updates' : '/api/updates?start=false');
  } catch { return; }
  if (data.running) {
    scheduleUpdatesPoll();
  } else {
    if (updatesPollTimer) { clearTimeout(updatesPollTimer); updatesPollTimer = null; }
    refreshLibrary().catch(() => {});   // 扫描结束：拉取带可更新状态的库数据刷新徽标
  }
}

async function cancelJob(id) {
  await j(`/api/jobs/${encodeURIComponent(id)}/cancel`, { method: 'POST' });
  await refreshJobs();
}

async function clearJob(id) {
  await j(`/api/jobs/${encodeURIComponent(id)}`, { method: 'DELETE' });
}

// ── Book Name Modal ────────────────────────────────────────────────

function hideBookNameModal() {
  pendingBookNameJobId = null;
  pendingBookNameOptions = [];
  const modal = document.getElementById('bookNameModal');
  if (modal) modal.classList.add('hidden');
  document.body.style.overflow = '';
}

function showBookNameModal(job) {
  pendingBookNameJobId = job.id;
  pendingBookNameOptions = job.book_name_options || [];
  const modal = document.getElementById('bookNameModal');
  const hint = document.getElementById('bookNameJobHint');
  const options = document.getElementById('bookNameOptions');
  if (!modal || !options) return;

  if (hint) {
    const title = job.title || job.book_id || '';
    hint.textContent = title ? `《${title}》` : '';
  }

  options.innerHTML = '';
  pendingBookNameOptions.forEach((opt, idx) => {
    const id = `bookNameOpt_${idx}`;
    const row = document.createElement('label');
    row.className = 'row';
    row.innerHTML = `
      <input type="radio" name="bookNameOpt" id="${id}" value="${esc(opt.value)}" ${idx === 0 ? 'checked' : ''} />
      <span>${esc(opt.label)}: ${esc(opt.value)}</span>
    `;
    options.appendChild(row);
  });
  document.body.style.overflow = 'hidden';
  modal.classList.remove('hidden');
}

async function submitBookNameChoice(value) {
  if (!pendingBookNameJobId) return;
  await j(`/api/jobs/${encodeURIComponent(pendingBookNameJobId)}/book_name`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ value })
  });
  hideBookNameModal();
  await refreshJobs();
}

function hideFormatModal() {
  pendingFormatJobId = null;
  pendingFormatOptions = [];
  const modal = document.getElementById('formatModal');
  if (modal) modal.classList.add('hidden');
  document.body.style.overflow = '';
}

function showFormatModal(job) {
  pendingFormatJobId = job.id;
  pendingFormatOptions = job.format_options || [];
  const modal = document.getElementById('formatModal');
  const hint = document.getElementById('formatJobHint');
  const options = document.getElementById('formatOptions');
  if (!modal || !options) return;

  if (hint) {
    const title = job.title || job.book_id || '';
    hint.textContent = title ? `《${title}》` : '';
  }

  options.innerHTML = '';
  pendingFormatOptions.forEach((opt, idx) => {
    const id = `formatOpt_${idx}`;
    const row = document.createElement('label');
    row.className = 'row';
    row.innerHTML = `
      <input type="radio" name="formatOpt" id="${id}" value="${esc(opt.value)}" ${idx === 0 ? 'checked' : ''} />
      <span>${esc(opt.label)}: ${esc(opt.value)}</span>
    `;
    options.appendChild(row);
  });
  document.body.style.overflow = 'hidden';
  modal.classList.remove('hidden');
}

async function submitFormatChoice(value) {
  if (!pendingFormatJobId) return;
  await j(`/api/jobs/${encodeURIComponent(pendingFormatJobId)}/format`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ value })
  });
  hideFormatModal();
  await refreshJobs();
}

async function openJobConfiguration(jobId, kindHint) {
  const data = await j(`/api/jobs?id=${encodeURIComponent(jobId)}`);
  const job = (data.items || [])[0];
  if (!job) {
    throw new Error('任务不存在或已被清理');
  }

  const hasBookNameOptions = (job.book_name_options || []).length > 0;
  const hasFormatOptions = (job.format_options || []).length > 0;

  if (hasBookNameOptions && (kindHint === 'book_name' || !hasFormatOptions)) {
    hideFormatModal();
    showBookNameModal(job);
    return;
  }

  if (hasFormatOptions) {
    hideBookNameModal();
    showFormatModal(job);
    return;
  }

  throw new Error('当前任务没有待配置项');
}

// ── Wire ───────────────────────────────────────────────────────────

function wire() {
  // -- Navigation --
  const navLinks = document.querySelectorAll('.nav a');
  const sections = document.querySelectorAll('.section');

  function switchSection(hash) {
    if (!hash) hash = '#search';
    navLinks.forEach(link => {
      link.classList.toggle('active', link.getAttribute('href') === hash);
    });
    sections.forEach(sec => {
      sec.classList.toggle('active', '#' + sec.id === hash);
    });
    // 首页=搜索页：无搜索时展示“搜索记录 + 最近下载”面板
    if (hash === '#search' && !homeSearchActive) {
      const panels = document.getElementById('homePanels');
      if (panels) panels.classList.remove('hidden');
      refreshSearchHistory().catch(() => {});
      loadRecentDownloads().catch(() => {});
    }
    if (hash === '#library') {
      refreshLibrary().catch(() => {});
    }
  }

  window.addEventListener('hashchange', () => switchSection(window.location.hash));
  switchSection(window.location.hash);

  // -- Theme Toggle --
  const themeBtn = document.getElementById('themeToggle');
  if (themeBtn) themeBtn.addEventListener('click', toggleTheme);
  updateThemeButton(getStoredTheme());

  // -- 下载库工具栏：刷新 / 书名过滤 --
  const libRefreshBtn = document.getElementById('libRefresh');
  if (libRefreshBtn) libRefreshBtn.addEventListener('click', () => { refreshLibrary().catch(() => {}); refreshUpdates(true).catch(() => {}); });
  const libFilterEl = document.getElementById('libFilter');
  if (libFilterEl) libFilterEl.addEventListener('input', () => { renderLibraryGrid(); });

  // 列数切换（全局，作用于所有 .book-grid）
  document.querySelectorAll('.colToggle').forEach(btn => btn.addEventListener('click', () => {
    const cur = localStorage.getItem(GRID_COLS_KEY) === '1' ? 1 : 2;
    try { localStorage.setItem(GRID_COLS_KEY, cur === 2 ? '1' : '2'); } catch {}
    applyGridCols();
  }));

  // -- Search --
  const searchForm = document.getElementById('searchForm');
  const qInput = document.getElementById('q');
  const searchClearBtn = document.getElementById('searchClear');
  if (qInput) qInput.addEventListener('input', updateSearchClear);
  if (searchClearBtn) searchClearBtn.addEventListener('click', () => { if (qInput) { qInput.value = ''; qInput.focus(); } updateSearchClear(); });
  updateSearchClear();
  if (searchForm) {
    searchForm.addEventListener('submit', async (e) => {
      e.preventDefault();
      await runSearch(qInput ? qInput.value : '');
    });
  }

  const clearHistBtn = document.getElementById('clearSearchHistory');
  if (clearHistBtn) clearHistBtn.addEventListener('click', clearSearchHistory);

  // -- 搜索源筛选（默认全选，持久化） --
  const PROVIDERS_KEY = 'tnd.providers';
  const providerBox = document.getElementById('providerFilter');
  if (providerBox) {
    let saved;
    try { const a = JSON.parse(localStorage.getItem(PROVIDERS_KEY)); saved = Array.isArray(a) ? new Set(a) : null; }
    catch { saved = null; }
    if (!saved) saved = new Set(['fanqie', 'shuqi', 'qimao']);
    providerBox.querySelectorAll('input[type=checkbox]').forEach(cb => {
      cb.checked = saved.has(cb.value);
      cb.closest('label')?.classList.toggle('checked', cb.checked);
    });
    providerBox.addEventListener('change', async (e) => {
      const cb = e.target;
      if (!cb || cb.tagName !== 'INPUT') return;
      const checked = [...providerBox.querySelectorAll('input:checked')].map(x => x.value);
      // 至少保留一个搜索源：取消最后一个时回弹勾选并明确提示（不再静默吞掉操作）。
      if (checked.length === 0) {
        cb.checked = true;
        cb.closest('label')?.classList.toggle('checked', true);
        showSearchHint(`至少需要保留一个搜索源，无法取消「${PROVIDER_LABELS[cb.value] || cb.value}」`, 2800);
        return;
      }
      cb.closest('label')?.classList.toggle('checked', cb.checked);
      try { localStorage.setItem(PROVIDERS_KEY, JSON.stringify(checked)); } catch {}
      if (!lastSearchQuery) return;

      // 新勾选且未缓存的源：只拉这一个源（增量），并带上当前激活的分类/筛选条件。
      // 该源是否真支持这些条件，由它自己响应里的 provider_meta 决定（不支持的源后端自会忽略），
      // 前端不再硬编码“只有番茄/七猫支持分类筛选”。
      if (cb.checked && !searchProviderCache[cb.value]) {
        let url = `/api/search?q=${encodeURIComponent(lastSearchQuery)}&provider=${cb.value}`;
        if (searchState) {
          const fTab = searchState.tab || 1;
          const fSel = [...searchState.selected.keys()].join(',');
          url += `&tab=${fTab}&selected_items=${encodeURIComponent(fSel)}`;
        }
        try {
          const data = await j(url);
          const items = data.items || [];
          for (const it of items) it._provider = cb.value;
          searchProviderCache[cb.value] = items;
          absorbProviderMeta(cb.value, data);
          const phm = data.provider_has_more && data.provider_has_more[cb.value];
          providerPages[cb.value] = cb.value === 'fanqie'
            ? { hasMore: phm != null ? !!phm : !!data.has_more, nextOffset: data.next_offset || 0 }
            : { hasMore: phm != null ? !!phm : false, page: 1 };
        } catch {
          searchProviderCache[cb.value] = [];
          providerPages[cb.value] = { hasMore: false, page: 1 };
        }
      }
      // 勾选变化会改变能力并集：重建工具栏——追加源后补上它的分类/筛选，
      // 取消支持分类/筛选的源后该能力从并集消失，全部消失则整个工具栏隐藏。
      const st = syncToolbar();
      if (st.pruned && searchState) {
        // 有分类/筛选项因源被取消而失效 → 按新条件重取，避免继续展示旧条件筛选出的结果。
        await refreshTab();
      } else {
        renderMergedResults(lastSearchQuery);
      }
    });
  }

  // -- Delegated Click Handlers --
  document.addEventListener('click', async (e) => {
    const t = e.target;
    if (!t || !t.classList) return;

    // 下载库：图标下载按钮（非超链接）
    const dlBtn = t.closest ? t.closest('.libDl') : null;
    if (dlBtn) {
      const href = dlBtn.getAttribute('data-href') || '';
      if (href) { const a = document.createElement('a'); a.href = href; a.download = ''; document.body.appendChild(a); a.click(); a.remove(); }
      return;
    }
    // 点击卡片任意处 → 打开预览（排除卡片内的按钮/链接，如下载、删除）
    const card = t.closest ? t.closest('.book-card[data-bookid]') : null;
    if (card && !t.closest('button, a')) {
      try {
        await openPreview(
          card.getAttribute('data-bookid'),
          card.getAttribute('data-cover') || '',
          false,
          card.getAttribute('data-title') || ''
        );
      } catch (err) { alert(err); }
      return;
    }

    if (t.classList.contains('startDownload')) {
      const bookId = t.getAttribute('data-bookid');
      const coverUrl = t.getAttribute('data-cover') || '';
      try { await startDownload(bookId, coverUrl); } catch (err) { alert(err); }
    }
    if (t.classList.contains('cancelJob')) {
      const id = t.getAttribute('data-jobid');
      if (!confirm('确认取消该任务并从列表中清理吗？')) return;
      try { await cancelJob(id); } catch (err) { alert(err); }
    }
    if (t.classList.contains('retryJob')) {
      const bookId = t.getAttribute('data-bookid');
      const jobId = t.getAttribute('data-jobid');
      try {
        await startDownloadDirect(bookId);
        if (jobId) {
          await clearJob(jobId).catch(() => {});
        }
        await refreshJobs();
      } catch (err) { alert(err); }
    }
    if (t.classList.contains('configJob')) {
      const jobId = t.getAttribute('data-jobid');
      const kind = t.getAttribute('data-kind') || '';
      try { await openJobConfiguration(jobId, kind); } catch (err) { alert(err); }
    }
    if (t.classList.contains('libDelete')) {
      let paths = [];
      try { paths = JSON.parse(t.getAttribute('data-paths') || '[]'); } catch { paths = []; }
      const title = t.getAttribute('data-title') || '该书';
      if (!Array.isArray(paths) || paths.length === 0) { alert('没有可删除的文件路径'); return; }
      if (!confirm(`确认删除《${title}》的下载文件？此操作不可恢复。`)) return;
      try {
        await j('/api/library/delete', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ paths }) });
        await refreshLibrary();
      } catch (err) { alert(err); }
    }
    if (t.classList.contains('hideJobBtn')) {
      const jobId = t.getAttribute('data-jobid');
      if (jobId) { addHidden(HIDDEN_JOBS_KEY, jobId); refreshJobs().catch(() => {}); }
    }
  });

  // -- Escape Key for Modals --
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') {
      const previewModal = document.getElementById('previewModal');
      if (previewModal && !previewModal.classList.contains('hidden')) {
        showPreviewModal(false);
        return;
      }
      const bookNameModal = document.getElementById('bookNameModal');
      if (bookNameModal && !bookNameModal.classList.contains('hidden')) {
        hideBookNameModal();
        return;
      }
      const formatModal = document.getElementById('formatModal');
      if (formatModal && !formatModal.classList.contains('hidden')) {
        hideFormatModal();
        return;
      }
      const iidWarningModal = document.getElementById('iidWarningModal');
      if (iidWarningModal && !iidWarningModal.classList.contains('hidden')) {
        showIidWarningModal(false);
        return;
      }
      const loginModal = document.getElementById('loginModal');
      if (loginModal && !loginModal.classList.contains('hidden')) {
        showLogin(false);
      }
    }
  });

  // -- Preview Modal Buttons --
  const previewConfirm = document.getElementById('previewConfirm');
  if (previewConfirm) previewConfirm.addEventListener('click', async () => {
    try { await confirmPreview(); } catch (err) { alert(err); }
  });

  const previewCancel = document.getElementById('previewCancel');
  if (previewCancel) previewCancel.addEventListener('click', () => showPreviewModal(false));

  const previewClose = document.getElementById('previewClose');
  if (previewClose) previewClose.addEventListener('click', () => showPreviewModal(false));

  // 点击预览遮罩空白处（非卡片内部）→ 退出
  const previewModalEl = document.getElementById('previewModal');
  if (previewModalEl) previewModalEl.addEventListener('click', (e) => { if (e.target === previewModalEl) showPreviewModal(false); });

  const previewRefresh = document.getElementById('previewRefresh');
  if (previewRefresh) previewRefresh.addEventListener('click', async () => {
    if (!currentPreviewBookId) return;
    try { await openPreview(currentPreviewBookId, null, true, currentPreviewHintTitle); } catch (err) { alert(err); }
    // #5：单本刷新后把结果写回库缓存对应书条目，徽标数值随之动态变化（无需全量重扫）。
    try {
      const r = await j('/api/updates/refresh-one?book_id=' + encodeURIComponent(currentPreviewBookId), { method: 'POST' });
      const bid = String(currentPreviewBookId);
      const hit = (libraryBooksCache || []).find(x => String(x.book_id) === bid);
      if (hit) {
        if (r && r.ok && r.row) {
          hit.new_count = Number(r.row.new_count) || 0;
          hit.local_total = Number(r.row.local_total) || 0;
          hit.remote_total = Number(r.row.remote_total) || 0;
        } else {
          hit.new_count = 0;
        }
      }
      renderLibraryGrid();
      renderRecentList();
    } catch (_) { /* 单本刷新失败不影响预览本身 */ }
  });

  // -- Book Name Modal --
  const bookNameConfirm = document.getElementById('bookNameConfirm');
  if (bookNameConfirm) bookNameConfirm.addEventListener('click', async () => {
    const selected = document.querySelector('input[name="bookNameOpt"]:checked');
    if (!selected) { alert('请选择一个书名'); return; }
    await submitBookNameChoice(selected.value);
  });

  const bookNameClose = document.getElementById('bookNameClose');
  if (bookNameClose) bookNameClose.addEventListener('click', () => hideBookNameModal());

  const formatConfirm = document.getElementById('formatConfirm');
  if (formatConfirm) formatConfirm.addEventListener('click', async () => {
    const selected = document.querySelector('input[name="formatOpt"]:checked');
    if (!selected) { alert('请选择一个输出格式'); return; }
    await submitFormatChoice(selected.value);
  });

  const formatClose = document.getElementById('formatClose');
  if (formatClose) formatClose.addEventListener('click', () => hideFormatModal());

  const iidWarningClose = document.getElementById('iidWarningClose');
  if (iidWarningClose) iidWarningClose.addEventListener('click', () => showIidWarningModal(false));
  const iidWarningOk = document.getElementById('iidWarningOk');
  if (iidWarningOk) iidWarningOk.addEventListener('click', () => showIidWarningModal(false));
}

// ── Boot ───────────────────────────────────────────────────────────

const GRID_COLS_KEY = 'tnd.grid_cols';
function applyGridCols() {
  const cols = localStorage.getItem(GRID_COLS_KEY) === '1' ? 1 : 2;
  document.body.classList.toggle('grid-cols-1', cols === 1);
  document.body.classList.toggle('grid-cols-2', cols === 2);
    document.querySelectorAll('.colToggle').forEach(btn => { btn.title = `切换列数（当前 ${cols} 列）`; });
}

async function boot() {
  wire();
  applyGridCols();
  await Promise.allSettled([
    refreshJobs(),
    refreshLibrary(),
    refreshSearchHistory(),
  ]);
  // #5：“可更新”状态已随 refreshLibrary 一次取到（后端启动时 boot_scan 已载入快照+串行扫）；页面加载不再拉 /api/updates。
  // 定时轮询只拉变化项（since=游标），不重复传输相同数据。
  setInterval(() => pollJobs().catch(() => {}), 1500);
}

boot().catch(err => console.error(err));
