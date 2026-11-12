import { initPlatter, PH } from './platter.js';
import { initAnatomy } from './anatomy.js';
import { initThreads } from './threads.js';
import { initValues } from './values.js';

const { gsap, ScrollTrigger, Lenis } = window;
gsap.registerPlugin(ScrollTrigger);
const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];
const clamp01 = x => Math.min(1, Math.max(0, x));
const seg = (p, a, b) => clamp01((p - a) / (b - a));
const ease = t => t < .5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2;
const NS = 'http://www.w3.org/2000/svg';
const mk = (tag, attrs = {}, parent, text) => { const e = document.createElementNS(NS, tag); for (const k in attrs) e.setAttribute(k, attrs[k]); if (text != null) e.textContent = text; parent && parent.appendChild(e); return e; };
const gradDefs = (svg, id, stops, attrs = {}) => { let d = svg.querySelector('defs') || mk('defs', {}, svg); const g = mk('linearGradient', { id, ...attrs }, d); stops.forEach(([o, c, a = 1]) => mk('stop', { offset: o, 'stop-color': c, 'stop-opacity': a }, g)); return g; };
const GRAD = [['0', '#FFFFFF'], ['.4', '#E6DEFF'], ['.72', '#B7C2FF'], ['1', '#A4E8E1']];
const esc = s => s.replace(/&/g, '&amp;').replace(/</g, '&lt;');

/* ---------------- smooth scroll ---------------- */
let lenis = null;
if (!reduced && Lenis) {
  lenis = new Lenis({ duration: 1.2, smoothWheel: true });
  lenis.on('scroll', ScrollTrigger.update);
  gsap.ticker.add(t => lenis.raf(t * 1000));
  gsap.ticker.lagSmoothing(0);
}
const scrollToY = y => lenis ? lenis.scrollTo(y, { duration: 1.4 }) : scrollTo({ top: y, behavior: 'smooth' });
$$('a[href^="#"]').forEach(a => a.addEventListener('click', e => {
  const t = $(a.getAttribute('href')); if (!t) return;
  e.preventDefault(); if (!introDone) endIntro(true);
  lenis ? lenis.scrollTo(t, { duration: 1.6 }) : t.scrollIntoView({ behavior: 'smooth' });
}));
ScrollTrigger.create({ start: () => innerHeight * 9.1, end: 'max', onToggle: s => $('.top').classList.toggle('solid', s.isActive) });
$('#cargo').addEventListener('click', () => { navigator.clipboard?.writeText('cargo add waraq'); const c = $('#cargo .cp'); c.textContent = '✓'; setTimeout(() => c.textContent = '⧉', 1200); });

/* =========================================================
   00 · THE DISK: a track becomes a lane, the lane becomes flash, the camera rises to the SSD
   ========================================================= */
const canvas = $('#gl');
const cap = $('#formCap'), hero = $('#hero'), hudL = $('#hudL'), hudR = $('#hudR'), scrimT = $('#scrimT'), scrimB = $('#scrimB');
const dive = $('#dive'), meter = $('#meter'), ladderBox = $('#ladderBox'), ladder = $$('#ladder li');
let stageP = 0, introDone = false;
const heroIn = { v: 0 };
ScrollTrigger.create({ trigger: '#scrolly', start: 'top top', end: 'bottom bottom', onUpdate: s => stageP = s.progress });

// the story, keyed to scroll; the drive's own phases are in platter.js
const STORY = [
  [.09, .15, '1970. The B-tree is designed for <b>spinning disks</b>.'],
  [.15, .21, 'One node fills one disk block, so <b>one seek reads one node</b>.'],
  [.21, .27, 'Wide nodes keep the tree shallow: <b>three or four seeks</b> find any key.'],
  [.27, .32, 'Each seek waits for the arm to move and the disk to <b>spin round</b>.'],
  [.40, .47, 'Then storage moved to <b>flash</b>.'],
  [.555, .67, 'With many reads in flight, the same drive answered <b>about 240,000 a second</b>.'],
  [.72, .83, 'Pages already in RAM skip the drive: a lookup takes <b>under a microsecond</b>.'],
];
const HEADS = [
  [.32, .40, '01 — The track', 'Every read waits<br>for the disk to turn.'],
  [.47, .545, '02 — NVMe', 'Flash reads a random page<br>in about 25 microseconds.'],
  [.675, .835, '03 — Waraq', 'Built to keep that drive busy,<br>from every core at once.'],
];
let capI = -1, headI = -1;
const fmtLat = ns => ns >= 1e6 ? (ns / 1e6).toFixed(1) + ' ms' : ns >= 1e3 ? Math.round(ns / 1e3) + ' µs' : Math.round(ns) + ' ns';

const platter = initPlatter(canvas, { onFrame: st => choreograph(stageP, st) });
if (!platter) canvas.style.display = 'none';
if (platter) platter.state.spin = 0;

const rot = $('#rot');
const ROLES = ['NVMe', 'every core', 'memory speed', 'the browser'];
let roleIdx = 0;
function nextRole() {
  if (stageP > .03) return;
  roleIdx = (roleIdx + 1) % ROLES.length;
  gsap.to(rot, { opacity: 0, y: -12, filter: 'blur(6px)', duration: .22, ease: 'power2.in', onComplete: () => { rot.textContent = ROLES[roleIdx]; gsap.fromTo(rot, { opacity: 0, y: 12, filter: 'blur(6px)' }, { opacity: 1, y: 0, filter: 'blur(0px)', duration: .42, ease: 'power3.out' }); } });
}

// on load the words come in while the disk spins up; scrolling is never locked
gsap.set(['.pill', '.h1 .ln', '.hero-sub', '.cta-row'], { opacity: 0, y: 30 });
if (platter) gsap.to(platter.state, { spin: 1, duration: 2.6, ease: 'power2.inOut' });
function endIntro(fast) {
  if (introDone) return; introDone = true;
  document.body.classList.remove('intro');
  hero.classList.add('live');
  const d0 = fast ? .2 : .3;
  gsap.to(heroIn, { v: 1, duration: 1.2, delay: d0, ease: 'power2.out' });
  gsap.fromTo(['.pill', '.h1 .ln', '.hero-sub', '.cta-row'], { opacity: 0, y: 30 }, { opacity: 1, y: 0, duration: 1.5, stagger: .15, delay: d0, ease: 'power3.out' });
  setTimeout(() => setInterval(nextRole, 2200), (d0 + 1.6) * 1000);
}
$('#skip').addEventListener('click', () => endIntro(true));
addEventListener('wheel', () => { if (!introDone) endIntro(true); }, { passive: true });
addEventListener('touchmove', () => { if (!introDone) endIntro(true); }, { passive: true });
addEventListener('keydown', e => { if (!introDone && [' ', 'ArrowDown', 'PageDown', 'Enter'].includes(e.key)) endIntro(true); });
endIntro(reduced);

function choreograph(p, st) {
  // hero and its HUD
  const out = seg(p, .012, .05);
  hero.style.opacity = heroIn.v * (1 - out); hero.style.transform = `translateY(${-out * 40}px)`;
  if (st && st.cur) {
    const m = platter.D0 / st.cur.dist;
    $('#hMag').textContent = '×' + (m < 10 ? m.toFixed(1) : Math.round(m).toLocaleString('en-US'));
  }
  // captions
  let ci = -1; STORY.forEach(([a, b], i) => { if (p >= a && p < b) ci = i; });
  if (introDone && ci !== capI) {
    capI = ci;
    if (ci < 0) gsap.to(cap, { opacity: 0, duration: .25 });
    else { cap.innerHTML = STORY[ci][2]; gsap.fromTo(cap, { opacity: 0, y: 8 }, { opacity: 1, y: 0, duration: .4 }); }
  }
  // the chapter headlines
  let hi = -1; HEADS.forEach(([a, b], i) => { if (p >= a && p < b) hi = i; });
  if (hi !== headI) { headI = hi; if (hi >= 0) { $('#dive .label').textContent = HEADS[hi][2]; $('#dive .h2').innerHTML = HEADS[hi][3]; } }
  const hv2 = hi >= 0 ? seg(p, HEADS[hi][0], HEADS[hi][0] + .015) * (1 - seg(p, HEADS[hi][1] - .015, HEADS[hi][1])) : 0;
  dive.style.opacity = hv2; dive.style.transform = `translateY(${(1 - hv2) * 20}px)`;
  // the meter: a disk seek, then an NVMe read, then a read from memory
  const mv = seg(p, PH.dive1, PH.dive1 + .02) * (1 - seg(p, .82, .84));
  meter.style.opacity = mv;
  const k1 = ease(seg(p, PH.warp0 + .01, .53)), k2 = ease(seg(p, .73, .81));
  const lnS = Math.log(4.2e6), lnN = Math.log(2.5e4), lnM = Math.log(500);
  const ns = Math.exp(lnS + (lnN - lnS) * k1 + (lnM - lnN) * k2);
  $('#mV').textContent = k2 > .97 ? '< 1 µs' : fmtLat(ns);
  $('#meter .m-read .label').textContent = k2 > .5 ? 'for one lookup in RAM' : 'to read one 4 KiB page';
  const pos = 3 + k1 * 44 + k2 * 48;
  $('#mFill').style.width = pos + '%'; $('#mNeedle').style.left = pos + '%';
  $('#ssdName').style.opacity = st ? seg(p, .6, .63) * (1 - seg(p, .835, .85)) : 0;
  const passed = pos >= 46.5;
  $('#mSsd').classList.toggle('on', passed);
  $('#mChip').style.opacity = passed && p < .6 ? 1 : 0;
  const med = k2 > .5 ? 'RAM' : k1 > .5 ? 'NVMe flash' : 'Magnetic disk';
  $('#hMed').textContent = med;
  $('#hLat').textContent = k2 > .97 ? '< 1 µs' : fmtLat(ns);
  $('#hRpm').textContent = k1 > .3 ? '—' : '7,200';
  const src = mv > .5; $('#hHint').style.display = src ? 'none' : ''; $('#hSrc').style.display = src ? 'block' : 'none';
  $('#hTrk').textContent = st && st.scene > .5 ? 'flash · 4 KiB pages' : '172 · 4 KiB sectors';
  hudL.style.opacity = hudR.style.opacity = heroIn.v * (1 - seg(p, .02, .05)) + seg(p, PH.dive1, PH.dive1 + .02) * (1 - seg(p, .82, .84));
  // the etched side and the ladder
  const lp = seg(p, .855, .985);
  const lv = seg(p, .84, .86) * (1 - seg(p, .975, .995));
  ladderBox.style.opacity = lv;
  canvas.style.opacity = 1;
  scrimT.style.opacity = hv2;
  scrimB.style.opacity = seg(p, .08, .095) * (1 - lv);
  if (p < .06) scrimT.style.opacity = 0;
  let curI = 0;
  ladder.forEach((li, i) => {
    li.style.opacity = seg(lp, i * .2, i * .2 + .05);
    const cur = lp >= i * .2 && (i === 4 || lp < (i + 1) * .2); if (cur) curI = i;
    li.classList.toggle('cur', cur); li.classList.toggle('done', !cur && lp >= (i + 1) * .2);
  });
  $('#ladStep').textContent = String(curI + 1).padStart(2, '0');
  if (platter) platter.state.s = p;
  const vis = p < .995;
  canvas.style.visibility = vis ? 'visible' : 'hidden';
  if (platter) platter.state.visible = vis;
}
if (!platter) gsap.ticker.add(() => choreograph(stageP, null));

/* LINEAGE: the eras light in turn */
(() => {
  const words = $$('.tv'), cols = $$('.vt');
  let hover = -1, auto = 0;
  const show = i => { words.forEach((w, k) => w.classList.toggle('on', k === i)); cols.forEach((c, k) => c.classList.toggle('on', k === i)); };
  ScrollTrigger.create({ trigger: '.trust-h', start: 'top 75%', once: true, onEnter: () => {
    words.forEach((w, k) => setTimeout(() => w.classList.add('on'), 250 + k * 380));
    setTimeout(() => { show(0); setInterval(() => { if (hover < 0) show(auto = (auto + 1) % 3); }, 2600); }, 250 + 3 * 380 + 900);
  } });
  cols.forEach((c, k) => { c.addEventListener('mouseenter', () => { hover = k; show(k); }); c.addEventListener('mouseleave', () => { hover = -1; }); });
})();

/* WHAT WARAQ IS · statement + numbers */
(() => {
  const st = $('#statement');
  const walk = node => [...node.childNodes].forEach(n => {
    if (n.nodeType === 3) { const fr = document.createDocumentFragment(); n.textContent.split(/(\s+)/).forEach(w => { if (!w) return; fr.appendChild(/^\s+$/.test(w) ? document.createTextNode(w) : Object.assign(document.createElement('span'), { className: 'w', textContent: w })); }); n.replaceWith(fr); }
    else walk(n);
  });
  walk(st);
  const ws = $$('.w', st), mets = $$('#what .metric');
  gsap.set(mets, { opacity: .12 });
  ScrollTrigger.create({ trigger: '#what', start: 'top top', end: 'bottom bottom', onUpdate: s2 => {
    const pr = s2.progress, k = Math.round(seg(pr, .04, .58) * ws.length);
    ws.forEach((w, i) => w.classList.toggle('in', i < k));
    mets.forEach((m, i) => {
      const v = ease(seg(pr, .6 + i * .07, .74 + i * .07)); m.style.opacity = .12 + .88 * v;
      const el = m.querySelector('.stat'); el.textContent = (+el.dataset.to * v).toFixed(+(el.dataset.dec || 0)) + el.dataset.suf;
    });
  } });
})();

/* pinned scrollytelling helper */
function pinSteps(sel, steps, onStep) {
  const sec = $(sel); let cur = -1;
  const st = ScrollTrigger.create({ trigger: sec, start: 'top top', end: 'bottom bottom', onUpdate: s => { const i = Math.min(steps - 1, Math.floor(s.progress * steps * .999)); if (i !== cur) { cur = i; onStep(i); } } });
  const go = i => { const top = sec.getBoundingClientRect().top + scrollY; scrollToY(top + (sec.offsetHeight - innerHeight) * ((i + .5) / steps)); };
  return { go, st };
}

/* 04 · FIG drawings (Linear-style wireframe isometrics) */
const ISO = (x, y, z, s = 1, ox = 160, oy = 150) => [ox + (x - y) * .866 * s, oy + (x + y) * .5 * s - z * s];
function box(g, x, y, z, w, d, h, cls = '') {
  const p = (a, b, c) => ISO(a, b, c).join(',');
  const grp = mk('g', { class: cls }, g);
  mk('polygon', { class: 'face', points: [p(x, y + d, z), p(x + w, y + d, z), p(x + w, y + d, z + h), p(x, y + d, z + h)].join(' ') }, grp);
  mk('polygon', { class: 'face', points: [p(x + w, y, z), p(x + w, y + d, z), p(x + w, y + d, z + h), p(x + w, y, z + h)].join(' ') }, grp);
  mk('polygon', { class: 'face', points: [p(x, y, z + h), p(x + w, y, z + h), p(x + w, y + d, z + h), p(x, y + d, z + h)].join(' ') }, grp);
  const E2 = [[x, y + d, z, x + w, y + d, z], [x + w, y, z, x + w, y + d, z], [x, y + d, z, x, y + d, z + h], [x + w, y + d, z, x + w, y + d, z + h], [x + w, y, z, x + w, y, z + h],
    [x, y, z + h, x + w, y, z + h], [x + w, y, z + h, x + w, y + d, z + h], [x + w, y + d, z + h, x, y + d, z + h], [x, y + d, z + h, x, y, z + h]];
  E2.forEach(([a, b, c, d2, e, f]) => mk('line', { class: 'e', x1: ISO(a, b, c)[0], y1: ISO(a, b, c)[1], x2: ISO(d2, e, f)[0], y2: ISO(d2, e, f)[1] }, grp));
  return grp;
}
(() => {
  // Fig 0.1 · two decks: memory above, NVMe below; pages rise when touched and sink when cold
  const f = $('#figMem');
  box(f, -80, -60, -18, 160, 120, 5, 'ghost');
  mk('text', { x: ISO(80, -60, -16)[0] + 8, y: ISO(80, -60, -16)[1] + 2 }, f, 'nvme');
  box(f, -80, -60, 34, 160, 120, 5);
  mk('text', { x: ISO(80, -60, 36)[0] + 8, y: ISO(80, -60, 36)[1] + 2, class: 'w' }, f, 'memory');
  const cells = [];
  for (let i = 0; i < 4; i++) for (let j = 0; j < 3; j++) cells.push({ x: -70 + i * 38, y: -50 + j * 36, up: (i + j) % 3 !== 0 });
  const pages = cells.map(c => { const g = mk('g', {}, f); box(g, c.x, c.y, 0, 24, 22, 6, c.up ? 'hotl' : ''); return { g, c }; });
  const place = () => pages.forEach(({ g, c }) => gsap.to(g, { y: c.up ? -39 : 23, duration: 1.1, ease: 'power3.inOut' }));
  pages.forEach(({ g, c }) => gsap.set(g, { y: c.up ? -39 : 23 }));
  setInterval(() => { const a = pages[(Math.random() * pages.length) | 0]; a.c.up = !a.c.up; a.g.querySelector('g')?.classList.toggle('hotl', a.c.up); place(); }, 1400);

  // Fig 0.2 · cores in a row; each runs its own stream of tasks, none waits on another
  const c2 = $('#figCores');
  const lanes = [];
  for (let k = 0; k < 4; k++) {
    const y = -66 + k * 36;
    box(c2, -84, y, 0, 168, 22, 3, 'ghost');
    mk('text', { x: ISO(-84, y + 22, 0)[0] - 30, y: ISO(-84, y + 22, 0)[1] + 3 }, c2, `core ${k}`);
    const g = mk('g', {}, c2); box(g, -84, y + 4, 3, 18, 14, 8, 'hot');
    lanes.push(g);
  }
  lanes.forEach((g, k) => gsap.fromTo(g, { x: 0, y: 0 }, { x: 146 * .866, y: 146 * .5, duration: 1.6 + k * .23, ease: 'none', repeat: -1, delay: k * .2 }));

  // Fig 0.3 · the log is made durable on commit; a crash, and the store reopens intact
  const r = $('#figSum');
  box(r, -60, -45, 0, 120, 90, 6, 'hotl');
  mk('text', { x: ISO(60, -45, 6)[0] + 8, y: ISO(60, -45, 6)[1] + 2, class: 'w' }, r, 'data');
  box(r, -60, -45, 44, 120, 26, 5, '');
  mk('text', { x: ISO(60, -45, 49)[0] + 8, y: ISO(60, -45, 49)[1] + 2 }, r, 'log');
  const recs = [];
  for (let k = 0; k < 6; k++) { const g = mk('g', { opacity: 0 }, r); box(g, -54 + k * 19, -40, 49, 14, 16, 4, 'hot'); recs.push(g); }
  const st3 = mk('text', { x: 160, y: 228, 'text-anchor': 'middle', class: 'w' }, r, '');
  const tl = gsap.timeline({ repeat: -1, repeatDelay: .8 });
  recs.forEach((g, k) => tl.to(g, { opacity: 1, duration: .25 }, k * .3));
  tl.call(() => { st3.textContent = 'commit · durable'; }, null, 1.9)
    .call(() => { st3.textContent = 'power cut'; r.style.opacity = .35; }, null, 3)
    .call(() => { st3.textContent = 'reopened · last commit intact'; r.style.opacity = 1; }, null, 3.7)
    .to(recs, { opacity: 0, duration: .3 }, 5.4).call(() => { st3.textContent = ''; }, null, 5.6);
  $$('.fig').forEach(fig => ScrollTrigger.create({ trigger: fig, start: 'top 70%', once: true, onEnter: () => fig.classList.add('on') }));
})();

/* 05 · BUILD (pinned) */
function hlLine(line) {
  const out = []; let i = 0;
  const KW = /^(use|let|mut|fn|if|return|for|in|move|true|false|Ok|Some|None|match|as|pub|struct|impl)\b/;
  while (i < line.length) {
    const r = line.slice(i); let m;
    if ((m = r.match(/^\/\/.*/))) { out.push(`<span class="c">${esc(m[0])}</span>`); break; }
    if ((m = r.match(/^(b?"(?:[^"\\]|\\.)*")/))) { out.push(`<span class="s">${esc(m[0])}</span>`); i += m[0].length; continue; }
    if ((m = r.match(/^-?\d[\d_]*(\.\d+)?\b/)) && !/[\w.]/.test(line[i - 1] || '')) { out.push(`<span class="n">${m[0]}</span>`); i += m[0].length; continue; }
    if ((m = r.match(KW)) && !/[\w.]/.test(line[i - 1] || '')) { out.push(`<span class="kw">${m[0]}</span>`); i += m[0].length; continue; }
    if ((m = r.match(/^[A-Z][A-Za-z0-9_]*/))) { out.push(`<span class="k">${m[0]}</span>`); i += m[0].length; continue; }
    if ((m = r.match(/^[{}\[\](),;?]/))) { out.push(`<span class="p">${m[0]}</span>`); i++; continue; }
    out.push(esc(line[i])); i++;
  }
  return `<span class="l">${out.join('') || ' '}</span>`;
}
const hlCode = code => code.split('\n').map(hlLine).join('');
const ED = [
  { file: 'src/main.rs · open', say: 'A store is <u>four files</u>. Create it once; <u>open</u> runs recovery after any crash.',
    ret: [['files', 'catalog · log · data · versions'], ['workers', 'one per core'], ['recovery', 'on open']],
    code: `use std::fs::OpenOptions;\nuse waraq::WorkerId;\nuse waraq_io::devices::Devices;\n\nlet file = |name: &str| OpenOptions::new()\n    .read(true).write(true).create(true).open(dir.join(name));\nlet devices = Devices::new(\n    file("catalog")?, file("log")?, file("data")?, file("versions")?,\n);\n\n// a worker per core, a 1 GiB page pool, a 64 MiB log ring each\nlet db = devices.create(cores, 262_144, 64 << 20)?;\n\n// after a restart: the log is read back, committed work is redone\nlet db = devices.open(262_144)?;`,
    resS: 'example', res: `created  4 files in ./data\nworkers  8 · one log ring and one I/O ring each\npool     262,144 frames · 1 GiB\n\nreopened after kill -9\nrecovery redo 1,204 records · undo 3 transactions\nready` },
  { file: 'src/main.rs · transact', say: 'A <u>transaction</u> sees one snapshot. If two write the same key, <u>the first one wins</u>.',
    ret: [['isolation', 'snapshot'], ['conflict', 'first writer wins'], ['commit', 'durable on return']],
    code: `let me = WorkerId(core);\nlet _bound = waraq::wal::worker::bind(core);\n\nlet mut tx = db.begin(me)?;\nlet alice = tx.get(b"acct:alice")?;\ntx.insert(b"acct:alice", b"70")?;\ntx.insert(b"acct:bob", b"130")?;\ntx.commit()?;            // durable when it returns`,
    resS: 'ok', res: `begin    worker 3 · snapshot taken\nget      acct:alice = 100\ninsert   acct:alice = 70   (old version kept as a delta)\ninsert   acct:bob   = 130\ncommit   log of worker 3 flushed · acknowledged` },
  { file: 'src/main.rs · scan', say: 'Scans walk the <u>leaves sideways</u>, along the links between them, inside your snapshot.',
    ret: [['order', 'byte order'], ['view', 'one snapshot'], ['stop', 'any time']],
    code: `use waraq::tree::scan::Step;\n\nlet tx = db.begin(me)?;\ntx.scan(b"acct:", |key, value| {\n    if !key.starts_with(b"acct:") { return Step::Stop; }\n    println!("{} = {}", key.escape_ascii(), value.escape_ascii());\n    Step::Continue\n})?;\ntx.commit()?;            // a read-only commit releases the snapshot`,
    resS: 'ok', res: `acct:alice = 70\nacct:bob = 130\nacct:carol = 55\nacct:dave = 410\n\n4 rows · 1 leaf · 0 locks taken` },
  { file: 'src/main.rs · commit', say: 'Free the core at once with a <u>pre-commit</u>, and wait for durability only <u>where you need it</u>.',
    ret: [['pre-commit', 'core freed'], ['wait', 'only your own log'], ['eventual', 'opt-in']],
    code: `use waraq::txn::db::Durability;\n\n// pre-commit: the core goes back to work immediately\nlet ack = tx.precommit()?;\nreply_later(ack);\n\n// ...and somewhere else\nack.wait()?;             // returns once the commit is durable\n\n// or answer once the log write is issued, per store or per transaction\nlet db = db.with_durability(Durability::Eventual);`,
    resS: 'ok', res: `precommit  worker 3 free after 1 record\nwait       log 3 durable · depends on log 1 (read from it)\n           log 1 durable · acknowledged\n\nlogs 0, 2, 4–7 were never waited on` },
];
let edI = 0, runTimer;
function renderEd(i) {
  edI = i; const e = ED[i];
  $$('#eIdx li').forEach((li, k) => li.classList.toggle('on', k === i));
  $('#eSay').innerHTML = e.say;
  $('#eReturns').innerHTML = e.ret.map(([a, b]) => `<li>${a} · <b>${b}</b></li>`).join('');
  $('#eNote').textContent = 'Names follow the current source and may change before 1.0';
  $('#eFile').textContent = e.file;
  $('#eReq').innerHTML = hlCode(e.code);
  $('#eResS').textContent = 'press Run ▸ · example output';
  $('#eRes').innerHTML = '<span class="l"><span class="c">// cargo run</span></span>';
  gsap.fromTo(['#eSay', '#eReq'], { opacity: 0, y: 6 }, { opacity: 1, y: 0, duration: .45, stagger: .05 });
  clearTimeout(runTimer); runTimer = setTimeout(run, 2600);
}
function run() {
  clearTimeout(runTimer);
  const e = ED[edI], outEl = $('#eRes');
  $('#eResS').textContent = 'running…'; outEl.innerHTML = '';
  setTimeout(() => {
    $('#eResS').textContent = e.resS;
    e.res.split('\n').forEach((l, k) => setTimeout(() => { outEl.insertAdjacentHTML('beforeend', `<span class="l">${esc(l) || ' '}</span>`); outEl.scrollTop = outEl.scrollHeight; }, k * 60));
  }, 420);
}
$('#eRun').addEventListener('click', run);
$('#eCopy').addEventListener('click', () => { navigator.clipboard?.writeText(ED[edI].code); $('#eCopy').textContent = 'Copied'; setTimeout(() => $('#eCopy').textContent = 'Copy', 1200); });
renderEd(0); clearTimeout(runTimer);
const ePin = pinSteps('#build', 4, i => renderEd(i));
$$('#eIdx li').forEach((li, i) => li.addEventListener('click', () => ePin.go(i)));

/* 06 · SCALES WITH CORES: the curve, drawn by scroll */
(() => {
  const svg = $('#curve'), W = 1200, H = 560, tip = $('#tip'), wrap = $('.curve-wrap');
  gradDefs(svg, 'gradCurve', GRAD);
  gradDefs(svg, 'gradArea', [['0', '#ffffff', .1], ['1', '#ffffff', 0]], { x1: 0, y1: 0, x2: 0, y2: 1 });
  // lookups relative to one core, measured at 1, 4, 8 and 14 cores
  const PTS = [[1, 1], [4, 3.70], [8, 6.78], [14, 8.92]];
  const val = c => { for (let i = 1; i < PTS.length; i++) if (c <= PTS[i][0]) { const [a, va] = PTS[i - 1], [b, vb] = PTS[i]; const t = (c - a) / (b - a); return va + (vb - va) * (t * t * (3 - 2 * t) * .3 + t * .7); } return PTS[PTS.length - 1][1]; };
  const xOf = c => (c - 1) / 13 * W, yOf = v => H - 30 - (v - 1) / 8.5 * (H - 210);
  const pts = []; for (let i = 0; i <= 130; i++) { const c = 1 + i / 10; pts.push([xOf(c), yOf(val(c))]); }
  const dPath = 'M' + pts.map(p => p.join(',')).join(' L');
  const vls = []; for (let x = 0; x <= W; x += 10) { const c = 1 + x / W * 13; vls.push(mk('line', { class: 'vl', x1: x, x2: x, y1: yOf(val(c)), y2: H, opacity: 0 }, svg)); }
  mk('path', { class: 'area', d: dPath + ` L${W},${H} L0,${H} Z`, 'clip-path': 'url(#clipC)' }, svg);
  const clip = mk('clipPath', { id: 'clipC' }, svg.querySelector('defs')); const clipR = mk('rect', { x: 0, y: 0, width: 0, height: H }, clip);
  const line = mk('path', { class: 'cl', d: dPath }, svg);
  const L2 = line.getTotalLength(); line.style.strokeDasharray = L2; line.style.strokeDashoffset = L2;
  const set = k => {
    line.style.strokeDashoffset = L2 * (1 - k); clipR.setAttribute('width', W * k);
    vls.forEach((v, i) => v.setAttribute('opacity', i / vls.length < k ? 1 : 0));
    const c = 1 + k * 13, v = val(c);
    $('#tipV').textContent = '×' + v.toFixed(1); $('#tipM').textContent = `${Math.round(c)} thread${Math.round(c) > 1 ? 's' : ''}`;
    const r = wrap.getBoundingClientRect(), px = xOf(c) / W * r.width, py = yOf(v) / H * r.height;
    $('#tipDot').style.transform = `translate(${px}px, ${py}px)`;
    const right = px > r.width * .55; tip.classList.toggle('right', right);
    const tw = tip.offsetWidth;
    tip.style.transform = `translate(${right ? px - tw - 22 : px + 22}px, ${Math.max(0, py - tip.offsetHeight - 10)}px)`;
  };
  set(.02);
  ScrollTrigger.create({ trigger: '#proof', start: 'top top', end: 'bottom bottom', scrub: .5, onUpdate: s => set(Math.max(.02, seg(s.progress, .08, .86))) });
})();

/* 07 · CLOSE: the disk returns as a dotted horizon of tracks */
(() => {
  const cv = $('#horizon'), ctx = cv.getContext('2d'); let w = 0, h = 0, vis = false;
  const DPR = Math.min(2, devicePixelRatio || 1);
  const size = () => { const r = cv.getBoundingClientRect(); w = r.width; h = r.height; cv.width = w * DPR; cv.height = h * DPR; ctx.setTransform(DPR, 0, 0, DPR, 0, 0); };
  size(); addEventListener('resize', size);
  ScrollTrigger.create({ trigger: '#close', start: 'top bottom', end: 'bottom top', onToggle: s => vis = s.isActive });
  const draw = now => {
    requestAnimationFrame(draw); if (!vis) return;
    const t = now / 1000, R = Math.max(w * .7, 680), cx = w / 2, cy = h * .66 + R * .32;
    ctx.clearRect(0, 0, w, h);
    const g = ctx.createRadialGradient(cx, cy - R * .3, R * .2, cx, cy - R * .3, R * 1.1);
    g.addColorStop(0, 'rgba(210,200,255,.07)'); g.addColorStop(1, 'rgba(255,255,255,0)');
    ctx.fillStyle = g; ctx.fillRect(0, 0, w, h);
    for (let k = 0; k < 26; k++) {
      const rr = R * (.35 + k * .025), n = 90 + k * 8, ry = rr * .32;
      for (let i = 0; i < n; i++) {
        const a = i / n * Math.PI * 2 + t * .05 * (1 + k * .01);
        const sx = cx + Math.cos(a) * rr, sy = cy + Math.sin(a) * ry;
        if (Math.sin(a) > .15 || sy < 0 || sy > h) continue;
        const z = -Math.sin(a);
        ctx.fillStyle = k === 12 ? `rgba(183,194,255,${.3 + z * .6})` : `rgba(255,255,255,${.06 + z * .32})`;
        ctx.fillRect(sx, sy, 1.4, 1.4);
      }
    }
    ctx.beginPath(); ctx.ellipse(cx, cy, R * .99, R * .99 * .32, 0, Math.PI, 2 * Math.PI); ctx.strokeStyle = 'rgba(255,255,255,.3)'; ctx.lineWidth = 1; ctx.stroke();
  };
  requestAnimationFrame(draw);
})();

/* 02 · ANATOMY */
initAnatomy({ gsap, ScrollTrigger, scrollToY });
initThreads({ gsap, ScrollTrigger, scrollToY });
initValues({ gsap, ScrollTrigger, scrollToY });

/* reveals */
$$('.cell .sec-head, .spot, .vt, .fig').forEach(el => gsap.from(el, { opacity: 0, y: 34, duration: 1.1, ease: 'power3.out', scrollTrigger: { trigger: el, start: 'top 88%', once: true } }));

/* HUMAN / AGENT */
const MD = `GET / · text/markdown · 1.4k tokens
# Waraq (ورق, "leaf")
> The B-tree, rebuilt for NVMe. An ACID key-value store in Rust.

Memory speed while your data fits, NVMe speed when it doesn't.

## What it is
- a B+tree of 4 KiB slotted pages, byte-ordered keys, opaque values
- pages in use stay in RAM; cold pages cool, then leave (one 4 KiB read brings one back)
- snapshot isolation, first writer wins, versions kept as deltas per worker
- one worker per core: its own log, its own I/O ring, many tasks
- crash recovery: redo committed work, undo the rest
- every page checksummed over its bytes and its id

## Layers
| layer     | idea                                                   |
|-----------|--------------------------------------------------------|
| cores     | thread per core, a miss yields instead of blocking     |
| tree      | optimistic lock coupling, no tree-wide lock            |
| pages     | predictive translation, cooling stage eviction         |
| snapshots | MVCC, per-worker version trees, watermark collection   |
| commit    | per-worker logs, remote flush avoidance, pre-commit    |
| device    | io_uring + O_DIRECT, four files, checksums, recovery   |

## Start
- cargo add waraq
- Rust, Linux (io_uring), macOS, browser (OPFS)`;
const av = $('#agentView');
av.innerHTML = MD.split('\n').map(l => { let h = esc(l); if (/^#/.test(l)) h = `<span class="h">${h}</span>`; return `<span class="ln">${h || ' '}</span>`; }).join('');
$$('.mode button').forEach(b => b.addEventListener('click', () => {
  const agentMode = b.dataset.mode === 'agent';
  if (agentMode && !introDone) endIntro(true);
  $$('.mode button').forEach(x => { x.classList.toggle('on', x === b); x.setAttribute('aria-pressed', x === b); });
  av.hidden = !agentMode; if (lenis) agentMode ? lenis.stop() : lenis.start();
}));
