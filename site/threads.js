// Thread per core, as a pinned figure: a live simulation on the left, what it adds up to on the right.
// Scroll walks five steps; the last hands the controls over. The rates are a simulation, not a benchmark.

const NS = 'http://www.w3.org/2000/svg';
const mk = (tag, attrs = {}, parent, text) => { const e = document.createElementNS(NS, tag); for (const k in attrs) e.setAttribute(k, attrs[k]); if (text != null) e.textContent = text; parent && parent.appendChild(e); return e; };
const $ = (s, r = document) => r.querySelector(s);

export function initThreads({ gsap, ScrollTrigger, scrollToY }) {
  const sec = $('#threads'); if (!sec) return;
  const svg = $('#thSvg'), chart = $('#thChart'), capEl = $('#thCap');
  const defs = mk('defs', {}, svg);
  const lg = mk('linearGradient', { id: 'thGrad', x1: 0, y1: 0, x2: 1, y2: 0 }, defs);
  [['0', '#FFFFFF'], ['.4', '#E6DEFF'], ['.72', '#B7C2FF'], ['1', '#A4E8E1']].forEach(([o, c]) => mk('stop', { offset: o, 'stop-color': c }, lg));
  const fl = mk('filter', { id: 'thGlow', x: '-200%', y: '-200%', width: '500%', height: '500%' }, defs);
  mk('feGaussianBlur', { stdDeviation: 2.4, result: 'b' }, fl);
  const fm = mk('feMerge', {}, fl); mk('feMergeNode', { in: 'b' }, fm); mk('feMergeNode', { in: 'SourceGraphic' }, fm);

  const W = 640, H = 420;
  const lanesG = mk('g', {}, svg), flyG = mk('g', {}, svg), taskG = mk('g', {}, svg);
  // the drive on the right, the lock on the left
  const nv = mk('g', {}, svg);
  mk('rect', { x: 566, y: 40, width: 56, height: 340, rx: 8, class: 'box' }, nv);
  for (let k = 0; k < 10; k++) mk('rect', { x: 576, y: 54 + k * 32, width: 36, height: 22, rx: 3, class: 'soft' }, nv);
  mk('text', { x: 594, y: 398, class: 'mono', 'font-size': 10, 'text-anchor': 'middle' }, nv, 'nvme');
  const lock = mk('g', { opacity: 0 }, svg);
  mk('rect', { x: 150, y: 30, width: 4, height: 360, rx: 2, class: 'fillw' }, lock);
  mk('text', { x: 152, y: 20, class: 'mono', 'font-size': 10, 'text-anchor': 'middle' }, lock, 'one shared lock');
  const st = { n: 4, shared: false, lanes: [], hist: [], lockLane: 0, forceMiss: -1 };

  function layout() {
    lanesG.replaceChildren(); taskG.replaceChildren(); flyG.replaceChildren();
    st.lanes = [];
    const top = 36, span = 352, lh = span / st.n;
    for (let i = 0; i < st.n; i++) {
      const y = top + lh * i + lh / 2;
      mk('text', { x: 18, y: y + 4, class: 'mono', 'font-size': Math.min(11, lh * .3) }, lanesG, `core ${i}`);
      mk('path', { d: `M164,${y} L470,${y}`, class: 'ln' }, lanesG);
      const rr = Math.min(14, lh * .3);
      const ring = mk('circle', { cx: 510, cy: y, r: rr, class: 'box' }, lanesG);
      for (let k = 0; k < 8; k++) { const a = k / 8 * Math.PI * 2; mk('circle', { cx: 510 + Math.cos(a) * rr * .65, cy: y + Math.sin(a) * rr * .65, r: 1.4, class: 'soft' }, lanesG); }
      const sz = Math.min(16, lh * .42);
      const lane = { y, ring, run: null, queue: [], parked: [], sz, fly: [] };
      for (let q = 0; q < 3; q++) lane.queue.push(newTask(lane));
      st.lanes.push(lane);
    }
    st.lockLane = 0;
    drawChart();
  }
  function newTask(lane) { return { x: 164, el: mk('rect', { width: lane.sz, height: lane.sz, rx: 3, class: 'box' }, taskG), miss: Math.random() < .25, missed: false, wait: 0 }; }
  function fly(lane) {
    const dot = mk('circle', { r: 3, class: 'tok' }, flyG);
    const o = { k: 0 };
    gsap.to(o, { k: 1, duration: .9, ease: 'power1.inOut', onUpdate: () => {
      const out = o.k < .5, k = out ? o.k * 2 : (1 - o.k) * 2;
      dot.setAttribute('cx', 510 + (566 - 510) * k); dot.setAttribute('cy', lane.y + (out ? -4 : 4));
    }, onComplete: () => dot.remove() });
  }
  function tick(dt) {
    const speed = 210;
    st.lanes.forEach((ln, i) => {
      ln.parked.forEach(t => { t.wait -= dt; });
      const back = ln.parked.filter(t => t.wait <= 0); ln.parked = ln.parked.filter(t => t.wait > 0);
      back.forEach(t => ln.queue.unshift(t));
      ln.ring.setAttribute('class', ln.parked.length ? 'box warm' : 'box');
      if (!ln.run && ln.queue.length) ln.run = ln.queue.shift();
      if (ln.queue.length < 3) ln.queue.push(newTask(ln));
      const may = !st.shared || st.lockLane === i;
      const t = ln.run;
      if (t && may) {
        t.x += speed * dt;
        if (((t.miss && !t.missed) || st.forceMiss === i) && t.x >= 290 && !t.missed) {
          t.missed = true; t.wait = .9; ln.parked.push(t); ln.run = null; fly(ln); if (st.forceMiss === i) st.forceMiss = -1;
          if (st.shared) st.lockLane = (st.lockLane + 1) % st.n;
        } else if (t.x >= 470) {
          t.el.remove(); ln.run = null; st.hist.push(performance.now());
          if (st.shared) st.lockLane = (st.lockLane + 1) % st.n;
        }
      }
      ln.queue.forEach((q, k) => { q.el.setAttribute('x', 146 - (k + 1) * (ln.sz + 3)); q.el.setAttribute('y', ln.y - ln.sz / 2); q.el.setAttribute('class', 'box dim'); });
      if (ln.run) { ln.run.el.setAttribute('x', ln.run.x - ln.sz / 2); ln.run.el.setAttribute('y', ln.y - ln.sz / 2); ln.run.el.setAttribute('class', may ? 'box hot' : 'box dim'); }
      ln.parked.forEach((q, k) => { q.el.setAttribute('x', 510 - ln.sz / 2 + k * 4); q.el.setAttribute('y', ln.y - ln.sz / 2); q.el.setAttribute('class', 'box warm'); });
    });
    const now = performance.now(); st.hist = st.hist.filter(t => now - t < 2500);
    $('#thRate').textContent = (st.hist.length / 2.5).toFixed(1);
  }

  /* the chart: what each mode adds up to as cores are added (the same simulation, run to steady state) */
  const CW = 420, CH = 300, pad = { l: 40, r: 16, t: 20, b: 34 };
  const cx = n => pad.l + (n - 1) / 15 * (CW - pad.l - pad.r), cy = v => CH - pad.b - v / 16 * (CH - pad.t - pad.b);
  const rateOf = (n, shared) => shared ? Math.min(1.15, .85 + n * .03) : n * .93;
  for (let n = 1; n <= 16; n += 1) if (n === 1 || n % 4 === 0) { mk('line', { x1: cx(n), x2: cx(n), y1: pad.t, y2: CH - pad.b, class: 'ln d' }, chart); mk('text', { x: cx(n), y: CH - pad.b + 18, class: 'mono', 'font-size': 10, 'text-anchor': 'middle' }, chart, n); }
  mk('text', { x: (CW + pad.l) / 2, y: CH - 2, class: 'mono', 'font-size': 10, 'text-anchor': 'middle' }, chart, 'cores');
  mk('text', { x: pad.l - 8, y: pad.t + 4, class: 'mono', 'font-size': 10, 'text-anchor': 'end' }, chart, '×');
  const path = sh => { let d = ''; for (let n = 1; n <= 16; n += .25) d += (d ? ' L' : 'M') + cx(n) + ',' + cy(rateOf(n, sh)); return d; };
  const lineO = mk('path', { d: path(false), class: 'ln hot' }, chart);
  const lineS = mk('path', { d: path(true), class: 'ln', 'stroke-dasharray': '4 4' }, chart);
  const labO = mk('text', { x: cx(16) - 4, y: cy(rateOf(16, false)) - 10, class: 'mono', 'font-size': 10, 'text-anchor': 'end' }, chart, 'optimistic · reads add up');
  const labS = mk('text', { x: cx(16) - 4, y: cy(rateOf(16, true)) - 10, class: 'mono', 'font-size': 10, 'text-anchor': 'end' }, chart, 'one shared lock · one at a time');
  const dot = mk('circle', { r: 6, class: 'tok' }, chart);
  const LO = lineO.getTotalLength(), LS = lineS.getTotalLength();
  function drawChart(k = 1) {
    lineO.style.strokeDasharray = `${LO * k} ${LO}`;
    gsap.to(dot, { attr: { cx: cx(st.n), cy: cy(rateOf(st.n, st.shared)) }, duration: .5, ease: 'power3.out', overwrite: 'auto' });
    $('#thN').textContent = st.n; $('#thMode').textContent = st.shared ? 'one shared lock' : 'optimistic';
  }

  /* controls */
  const range = $('#thCores'), out = $('#thCoresO');
  range.addEventListener('input', () => { st.n = +range.value; out.textContent = st.n; layout(); });
  const setShared = v => {
    st.shared = v; gsap.to(lock, { opacity: v ? 1 : 0, duration: .3, overwrite: 'auto' });
    sec.querySelectorAll('#thLock button').forEach(b => b.classList.toggle('on', (b.dataset.v === '1') === v));
    drawChart();
  };
  sec.querySelectorAll('#thLock button').forEach(b => b.addEventListener('click', () => setShared(b.dataset.v === '1')));
  const setN = n => { st.n = n; range.value = n; out.textContent = n; layout(); };

  const STEPS = [
    { cap: 'One thread per <b>core</b>. Each runs its own worker, with its own queue, log and I/O ring.', on: () => { setShared(false); setN(4); } },
    { cap: 'A task that needs a page not in memory submits <b>one read</b>, and yields. The core runs the next task.', on: () => { setShared(false); st.forceMiss = 1; } },
    { cap: 'Readers take <b>no locks</b>: they check a version. Add cores, and the reads add up.', on: () => { setShared(false); setN(8); } },
    { cap: 'Put <b>one shared lock</b> in the way and every core waits its turn. That is the design Waraq avoids.', on: () => { setShared(true); } },
    { cap: '<b>Your turn.</b> Drag the cores, switch the lock, and watch what it adds up to.', on: () => { setShared(false); } },
  ];
  $('#thIdx').innerHTML = STEPS.map((s, i) => `<li data-i="${i}"><span class="label">${String(i + 1).padStart(2, '0')}</span></li>`).join('');
  let cur = -1;
  function show(i) {
    if (i === cur) return; cur = i;
    capEl.innerHTML = STEPS[i].cap; gsap.fromTo(capEl, { opacity: 0, y: 6 }, { opacity: 1, y: 0, duration: .4 });
    STEPS[i].on();
    sec.querySelectorAll('#thIdx li').forEach((li, k) => li.classList.toggle('on', k === i));
  }
  const yOf = i => sec.getBoundingClientRect().top + scrollY + (sec.offsetHeight - innerHeight) * ((i + .5) / STEPS.length);
  sec.querySelectorAll('#thIdx li').forEach((li, i) => li.addEventListener('click', () => scrollToY(yOf(i))));
  layout(); show(0);
  let inView = false, last = performance.now();
  ScrollTrigger.create({ trigger: sec, start: 'top bottom', end: 'bottom top', onToggle: s => { inView = s.isActive; } });
  ScrollTrigger.create({ trigger: sec, start: 'top top', end: 'bottom bottom', onUpdate: s => show(Math.min(STEPS.length - 1, Math.floor(s.progress * STEPS.length * .9999))) });
  gsap.ticker.add(() => { const now = performance.now(), dt = Math.min(.05, (now - last) / 1000); last = now; if (inView) tick(dt); });
}
