// The engine, as one figure with six layers (after the layered anatomy in Hugging Face's MoE handbook).
// "Engine" plays a request through every layer; each numbered tab zooms the camera into one layer
// and steps through how it works, with a caption per step. Play, step, or scrub across all of it.

const NS = 'http://www.w3.org/2000/svg';
const mk = (tag, attrs = {}, parent, text) => { const e = document.createElementNS(NS, tag); for (const k in attrs) e.setAttribute(k, attrs[k]); if (text != null) e.textContent = text; parent && parent.appendChild(e); return e; };
const $ = (s, r = document) => r.querySelector(s);

const VB = [0, 0, 1000, 560];
const BLOCKS = {
  cores:  { n: 1, title: 'Cores',     sub: 'thread per core',        at: [20, 40, 300, 230] },
  tree:   { n: 2, title: 'Tree',      sub: 'optimistic lock coupling', at: [350, 40, 300, 230] },
  pages:  { n: 3, title: 'Pages',     sub: 'translation · cooling',  at: [680, 40, 300, 230] },
  snap:   { n: 4, title: 'Snapshots', sub: 'mvcc · first writer wins', at: [20, 300, 300, 230] },
  commit: { n: 5, title: 'Commit',    sub: 'a log per core',         at: [350, 300, 300, 230] },
  device: { n: 6, title: 'Device',    sub: 'io_uring · four files',  at: [680, 300, 300, 230] },
};
const ORDER = ['cores', 'tree', 'pages', 'snap', 'commit', 'device'];

export function initAnatomy({ gsap, ScrollTrigger, scrollToY }) {
  const svg = $('#anSvg'); if (!svg) return;
  const defs = mk('defs', {}, svg);
  const lg = mk('linearGradient', { id: 'anGrad', x1: 0, y1: 0, x2: 1, y2: 0 }, defs);
  [['0', '#FFFFFF'], ['.4', '#E6DEFF'], ['.72', '#B7C2FF'], ['1', '#A4E8E1']].forEach(([o, c]) => mk('stop', { offset: o, 'stop-color': c }, lg));
  const fl = mk('filter', { id: 'anGlow', x: '-200%', y: '-200%', width: '500%', height: '500%' }, defs);
  mk('feGaussianBlur', { stdDeviation: 2.2, result: 'b' }, fl);
  const fm = mk('feMerge', {}, fl); mk('feMergeNode', { in: 'b' }, fm); mk('feMergeNode', { in: 'SourceGraphic' }, fm);

  /* ---------- the overview: links between layers ---------- */
  const links = mk('g', {}, svg);
  const L = (d, id) => mk('path', { d, class: 'ln d', id }, links);
  const lk = {
    ct: L('M320,155 L350,155', 'lk-ct'), tp: L('M650,155 L680,155', 'lk-tp'), pd: L('M830,270 L830,300', 'lk-pd'),
    cs: L('M170,270 L170,300', 'lk-cs'), sc: L('M320,415 L350,415', 'lk-sc'), cd: L('M650,415 L680,415', 'lk-cd'),
  };
  const ENGINE_PATH = 'M-10,155 L60,155 L320,155 L350,155 L500,155 L680,155 L830,155 L830,415 L650,415 L500,415 L350,415 L170,415 L170,300';

  /* ---------- the blocks ---------- */
  const G = {};
  ORDER.forEach(key => {
    const b = BLOCKS[key], [x, y, w, h] = b.at;
    const g = mk('g', { transform: `translate(${x},${y})`, class: 'block' }, svg);
    const r = mk('rect', { x: 0, y: 0, width: w, height: h, rx: 10, class: 'blk' }, g);
    mk('text', { x: 14, y: 22, class: 'bn' }, g, String(b.n).padStart(2, '0'));
    mk('text', { x: 36, y: 23, class: 'bt' }, g, b.title);
    mk('text', { x: w - 14, y: 22, class: 'mono', 'font-size': 7.5, 'text-anchor': 'end' }, g, b.sub);
    G[key] = { g, r, w, h };
  });
  const tok = mk('circle', { r: 4, class: 'tok', opacity: 0 }, svg);

  // small builders, in a block's local coordinates
  const R = (p, x, y, w, h, cls = 'box', rx = 3) => mk('rect', { x, y, width: w, height: h, rx, class: cls }, p);
  const T = (p, x, y, s, size = 8, cls = 'mono', anchor = 'start') => mk('text', { x, y, class: cls, 'font-size': size, 'text-anchor': anchor }, p, s);
  const P = (p, d, cls = 'ln') => mk('path', { d, class: cls }, p);
  const along = (path, el, tl, dur, at, ease = 'power1.inOut') => {
    const len = path.getTotalLength(), o = { k: 0 };
    tl.fromTo(o, { k: 0 }, { k: 1, duration: dur, ease, onUpdate: () => { const pt = path.getPointAtLength(o.k * len); el.setAttribute('cx', pt.x); el.setAttribute('cy', pt.y); } }, at);
  };

  /* ===== 1 · CORES ===== */
  const cores = (() => {
    const { g } = G.cores;
    const lanesG = mk('g', {}, g), taskG = mk('g', {}, g);
    const latch = R(g, 76, 46, 3, 168, 'fillw', 1.5); latch.setAttribute('opacity', 0);
    const latchT = T(g, 77, 224, 'one lock', 6.5, 'mono', 'middle'); latchT.setAttribute('opacity', 0);
    const rate = T(g, 286, 222, '', 7, 'mono', 'end');
    const st = { n: 4, shared: false, lanes: [], done: 0, hist: [], lockLane: 0, forceMiss: -1, running: true };
    function layout() {
      lanesG.replaceChildren(); taskG.replaceChildren();
      st.lanes = [];
      const top = 50, span = 162, lh = span / st.n;
      for (let i = 0; i < st.n; i++) {
        const y = top + lh * i + lh / 2;
        T(lanesG, 14, y + 2.5, `core ${i}`, 7);
        P(lanesG, `M84,${y} L232,${y}`, 'ln');
        const ring = mk('circle', { cx: 262, cy: y, r: Math.min(8, lh * .32), class: 'box' }, lanesG);
        const ringDot = mk('circle', { cx: 262, cy: y, r: 2, class: 'fillw', opacity: 0 }, lanesG);
        const lane = { y, ring, ringDot, run: null, queue: [], parked: [], sz: Math.min(9, lh * .42) };
        for (let q = 0; q < 3; q++) lane.queue.push(newTask(lane));
        st.lanes.push(lane);
      }
      st.lockLane = 0;
    }
    function newTask(lane) { return { x: 84, el: R(taskG, 0, 0, lane.sz, lane.sz, 'box', 1.5), miss: Math.random() < .28, missed: false, wait: 0 }; }
    function tick(dt) {
      if (!st.running) return;
      const speed = 120; // units per second
      st.lanes.forEach((ln, i) => {
        // parked tasks wait on the ring, then rejoin the front of the queue
        ln.parked.forEach(t => { t.wait -= dt; });
        const back = ln.parked.filter(t => t.wait <= 0); ln.parked = ln.parked.filter(t => t.wait > 0);
        back.forEach(t => { ln.queue.unshift(t); });
        ln.ringDot.setAttribute('opacity', ln.parked.length ? .9 : 0);
        ln.ring.classList.toggle('hot', ln.parked.length > 0);
        if (!ln.run && ln.queue.length) ln.run = ln.queue.shift();
        if (ln.queue.length < 3) ln.queue.push(newTask(ln));
        const may = !st.shared || st.lockLane === i;
        const t = ln.run;
        if (t && may) {
          t.x += speed * dt * (st.shared ? 1 : 1);
          if (((t.miss && !t.missed) || st.forceMiss === i) && t.x >= 158 && !t.missed) {
            t.missed = true; t.wait = .9; ln.parked.push(t); ln.run = null; if (st.forceMiss === i) st.forceMiss = -1;
            if (st.shared) st.lockLane = (st.lockLane + 1) % st.n;
          } else if (t.x >= 232) {
            t.el.remove(); ln.run = null; st.done++; st.hist.push(performance.now());
            if (st.shared) st.lockLane = (st.lockLane + 1) % st.n;
          }
        }
        // draw: queue stacked at the left, running on the lane, parked at the ring
        ln.queue.forEach((q, k) => { q.el.setAttribute('x', 74 - (k + 1) * (ln.sz + 2)); q.el.setAttribute('y', ln.y - ln.sz / 2); q.el.setAttribute('class', 'box dim'); });
        if (ln.run) { ln.run.el.setAttribute('x', ln.run.x - ln.sz / 2); ln.run.el.setAttribute('y', ln.y - ln.sz / 2); ln.run.el.setAttribute('class', may ? 'box hot' : 'box dim'); }
        ln.parked.forEach((q, k) => { q.el.setAttribute('x', 262 - ln.sz / 2 + k * 3); q.el.setAttribute('y', ln.y - ln.sz / 2); q.el.setAttribute('class', 'box warm'); });
      });
      const now = performance.now(); st.hist = st.hist.filter(t => now - t < 2000);
      rate.textContent = `${(st.hist.length / 2).toFixed(1)} tasks/s · illustrative`;
    }
    layout();
    const setShared = v => { st.shared = v; gsap.to([latch, latchT], { opacity: v ? 1 : 0, duration: .3 }); };
    const steps = [
      { cap: 'One worker per <b>core</b>, many tasks per worker. Nothing is shared between them.', run: tl => { tl.call(() => setShared(false)); } },
      { cap: 'A task that misses the pool submits one read and <b>steps aside</b>.', run: tl => { tl.call(() => { st.forceMiss = Math.min(1, st.n - 1); }); } },
      { cap: 'The core keeps running <b>other tasks</b> while that read is in flight.', run: tl => { tl.call(() => { st.forceMiss = 0; }); } },
      { cap: 'The page lands and the task <b>resumes</b>. Now try one shared lock, or more cores, above.', run: tl => { tl.call(() => {}); } },
    ];
    const controls = el => {
      el.innerHTML = `<label>cores <input type="range" min="1" max="8" step="1" value="${st.n}" id="anCores"><output id="anCoresO">${st.n}</output></label><div class="seg" id="anLock"><button class="${st.shared ? '' : 'on'}" data-v="0">Optimistic</button><button class="${st.shared ? 'on' : ''}" data-v="1">One shared lock</button></div>`;
      $('#anCores').addEventListener('input', e => { st.n = +e.target.value; $('#anCoresO').textContent = st.n; layout(); });
      el.querySelectorAll('#anLock button').forEach(b => b.addEventListener('click', () => { setShared(b.dataset.v === '1'); el.querySelectorAll('#anLock button').forEach(x => x.classList.toggle('on', x === b)); }));
    };
    return { steps, tick, reset: () => {}, controls, focus: () => {} };
  })();

  /* ===== 2 · TREE ===== */
  const tree = (() => {
    const { g } = G.tree;
    const nodes = {}, ver = {};
    const node = (id, x, y, w, keys) => {
      const ng = mk('g', {}, g);
      const r = R(ng, x - w / 2, y - 8, w, 16, 'box');
      for (let k = 1; k < keys; k++) P(ng, `M${x - w / 2 + w / keys * k},${y - 8} L${x - w / 2 + w / keys * k},${y + 8}`, 'ln');
      const v = T(ng, x, y - 12, '', 6.5, 'mono', 'middle');
      nodes[id] = { r, x, y, w, v, n: 7 + (id.length * 3) % 5 };
      return nodes[id];
    };
    const root = node('r', 150, 66, 60, 3);
    const inn = [node('a', 70, 116, 50, 3), node('b', 150, 116, 50, 3), node('c', 230, 116, 50, 3)];
    const lx = [32, 72, 112, 150, 188, 228, 268];
    const leaves = lx.map((x, i) => node('l' + i, x, 172, 30, 2));
    inn.forEach(n => P(g, `M${n.x},${n.y - 8} L${root.x + (n.x - root.x) * .35},${root.y + 8}`, 'ln'));
    const parentOf = i => inn[i < 2 ? 0 : i < 5 ? 1 : 2];
    leaves.forEach((l, i) => P(g, `M${l.x},${l.y - 8} L${parentOf(i).x + (l.x - parentOf(i).x) * .3},${parentOf(i).y + 8}`, 'ln'));
    const rl = leaves.slice(0, -1).map((l, i) => P(g, `M${l.x + 15},${l.y} L${leaves[i + 1].x - 15},${leaves[i + 1].y}`, 'ln d'));
    T(g, 150, 216, 'leaves · each links to its right neighbour', 6.5, 'mono', 'middle');
    const rd = mk('circle', { r: 3.5, class: 'tok', opacity: 0 }, g);
    const wr = R(g, 0, 0, 8, 8, 'fillw', 1.5); wr.setAttribute('opacity', 0);
    const mark = T(g, 0, 0, '', 7.5, 'mono', 'middle');
    const descend = mk('path', { d: `M${root.x},${root.y} L${inn[1].x},${inn[1].y} L${leaves[4].x},${leaves[4].y}`, fill: 'none' }, g);
    const sideways = mk('path', { d: `M${leaves[4].x},${leaves[4].y} L${leaves[6].x},${leaves[6].y}`, fill: 'none' }, g);
    const path = [root, inn[1], leaves[4]];
    const reset = () => {
      Object.values(nodes).forEach(n => { n.r.setAttribute('class', 'box'); n.v.textContent = ''; n.n = 7 + (Math.random() * 3 | 0); });
      rl.forEach(p => p.setAttribute('class', 'ln d'));
      gsap.set([rd, wr, mark], { opacity: 0 });
    };
    const steps = [
      { cap: 'Readers descend <b>without taking locks</b>. They note each page’s version on the way.', run: tl => {
        tl.set(rd, { opacity: 1 });
        along(descend, rd, tl, 1.8, 0);
        path.forEach((n, k) => tl.call(() => { n.r.setAttribute('class', 'box hot'); n.v.textContent = 'v' + n.n; }, null, k * .85));
      } },
      { cap: 'They copy what they need, then <b>check the version again</b>. Same number: the copy is good.', run: tl => {
        tl.set(rd, { opacity: 1, attr: { cx: leaves[4].x, cy: leaves[4].y } });
        tl.set(mark, { attr: { x: leaves[4].x, y: leaves[4].y + 22 }, textContent: '✓ same version', opacity: 0 });
        tl.to(mark, { opacity: 1, duration: .4 }, .5);
      } },
      { cap: 'If a writer changed the page meanwhile, the version moved, and the reader simply <b>tries again</b>.', run: tl => {
        const l = leaves[4];
        tl.set(rd, { opacity: 1, attr: { cx: l.x, cy: l.y } });
        tl.set(wr, { attr: { x: l.x - 4, y: l.y - 30 }, opacity: 0 });
        tl.to(wr, { opacity: 1, attr: { y: l.y - 4 }, duration: .5 }, .2);
        tl.call(() => { l.r.setAttribute('class', 'box warm'); l.v.textContent = 'v' + (l.n + 1); }, null, .7);
        tl.set(mark, { attr: { x: l.x, y: l.y + 22 }, textContent: '↺ moved · retry', opacity: 0 }, .9);
        tl.to(mark, { opacity: 1, duration: .3 }, .9);
        tl.to(wr, { opacity: 0, duration: .3 }, 1.2);
        tl.call(() => path.forEach(n => n.r.setAttribute('class', 'box')), null, 1.5);
        along(descend, rd, tl, 1.4, 1.6);
        tl.call(() => path.forEach(n => n.r.setAttribute('class', 'box hot')), null, 2.9);
        tl.set(mark, { textContent: '✓ same version' }, 3.1);
      } },
      { cap: 'Leaves link to their right neighbour, so a scan <b>walks sideways</b> and never climbs back up.', run: tl => {
        tl.set(rd, { opacity: 1, attr: { cx: leaves[4].x, cy: leaves[4].y } });
        tl.call(() => { rl[4].setAttribute('class', 'ln hot'); rl[5].setAttribute('class', 'ln hot'); }, null, .2);
        along(sideways, rd, tl, 1.6, .3);
        tl.call(() => leaves[5].r.setAttribute('class', 'box hot'), null, .9);
        tl.call(() => leaves[6].r.setAttribute('class', 'box hot'), null, 1.8);
      } },
    ];
    return { steps, reset };
  })();

  /* ===== 3 · PAGES ===== */
  const pages = (() => {
    const { g } = G.pages;
    T(g, 14, 52, 'page id → frame', 7);
    const rows = [[4107, 12], [221, 3], [9032, 17], [77, 8], [5120, 21], [3311, 5]];
    const rowEls = rows.map(([id, f], i) => {
      const y = 62 + i * 22;
      const r = R(g, 14, y, 104, 16, 'box');
      T(g, 22, y + 11, String(id), 7.5);
      const ft = T(g, 110, y + 11, 'f ' + f, 7.5, 'mono', 'end');
      return { r, ft, f };
    });
    const frames = [];
    for (let k = 0; k < 24; k++) {
      const c = k % 6, rr = (k / 6) | 0, x = 136 + c * 25, y = 62 + rr * 34;
      const r = R(g, x, y, 20, 28, 'box');
      const fill = R(g, x + 3, y + 3, 14, 22, 'soft', 1.5);
      frames.push({ r, fill, x, y, st: k % 5 === 4 ? 'free' : 'hot' });
    }
    T(g, 211, 210, 'frames in memory', 7, 'mono', 'middle');
    const arrow = P(g, 'M118,70 C126,70 128,70 136,76', 'ln hot'); arrow.setAttribute('opacity', 0);
    const mark = T(g, 0, 0, '✓', 10, 'bt', 'middle'); mark.setAttribute('opacity', 0);
    const paint = () => frames.forEach(f => { f.r.setAttribute('class', f.st === 'hot' ? 'box' : f.st === 'cool' ? 'box cool' : f.st === 'sel' ? 'box hot' : 'box dim'); f.fill.setAttribute('opacity', f.st === 'free' ? 0 : f.st === 'cool' ? .4 : 1); });
    const reset = () => { frames.forEach((f, k) => f.st = k % 5 === 4 ? 'free' : 'hot'); paint(); rowEls.forEach(r => r.r.setAttribute('class', 'box')); gsap.set([arrow, mark], { opacity: 0 }); };
    reset();
    const target = frames[12];
    const steps = [
      { cap: 'A child is named by <b>page id</b>, not by address. A table translates the id to a frame.', run: tl => {
        tl.call(() => rowEls[0].r.setAttribute('class', 'box hot'), null, .2);
      } },
      { cap: 'The frame is <b>predicted from the id</b> and confirmed after. One look, usually no chain to follow.', run: tl => {
        tl.call(() => rowEls[0].r.setAttribute('class', 'box hot'));
        tl.set(arrow, { attr: { d: `M118,70 C ${target.x - 20},70 ${target.x - 10},${target.y + 14} ${target.x},${target.y + 14}` } });
        tl.to(arrow, { opacity: 1, duration: .4 }, .2);
        tl.call(() => { target.st = 'sel'; paint(); }, null, .6);
        tl.set(mark, { attr: { x: target.x + 10, y: target.y - 4 } }, .6).to(mark, { opacity: 1, duration: .3 }, .9);
      } },
      { cap: 'Pages nobody touches <b>cool</b>. Touch a cooling page and it warms again, at no cost.', run: tl => {
        const cool = [2, 7, 15, 20, 22];
        tl.call(() => { cool.forEach(k => frames[k].st = 'cool'); paint(); }, null, .3);
        tl.call(() => { frames[7].st = 'sel'; paint(); }, null, 1.6);
        tl.call(() => { frames[7].st = 'hot'; paint(); }, null, 2.3);
      } },
      { cap: 'Only a page that stays cold is <b>evicted</b>. Its frame takes the next page that is read in.', run: tl => {
        tl.call(() => { [2, 15, 20, 22].forEach(k => frames[k].st = 'cool'); paint(); });
        tl.call(() => { frames[15].st = 'free'; paint(); }, null, .7);
        tl.call(() => { frames[15].st = 'sel'; paint(); }, null, 1.7);
      } },
    ];
    return { steps, reset };
  })();

  /* ===== 4 · SNAPSHOTS ===== */
  const snap = (() => {
    const { g } = G.snap;
    T(g, 14, 54, 'key  acct:alice   · newest → oldest', 7);
    const vx = [30, 102, 174, 246];
    const vals = [['', ''], ['70', 't9'], ['Δ 100', 't5'], ['Δ 120', 't2']];
    const vs = vals.map(([a, b], i) => {
      const vg = mk('g', {}, g);
      const r = R(vg, vx[i] - 30, 66, 60, 26, 'box');
      const t1 = T(vg, vx[i], 81, a, 8.5, 'bt', 'middle'); t1.setAttribute('font-size', 9);
      const t2 = T(vg, vx[i], 100, b ? 'commit ' + b : '', 6.5, 'mono', 'middle');
      if (i < 3) P(vg, `M${vx[i] + 30},79 L${vx[i + 1] - 30},79`, 'ln');
      return { vg, r, t1, t2 };
    });
    // transactions as horizontal lines with a start mark
    const txs = [['T1 · began at t7', 128], ['T2 · began at t11', 156], ['T3 · began at t11', 184]].map(([s, y]) => {
      const tg = mk('g', {}, g);
      T(tg, 14, y + 3, s, 7);
      const ln = P(tg, `M120,${y} L286,${y}`, 'ln d');
      const res = T(tg, 286, y + 3, '', 7, 'mono', 'end');
      return { tg, ln, res, y };
    });
    const wm = P(g, 'M0,60 L0,206', 'ln hot'); wm.setAttribute('opacity', 0);
    const wmT = T(g, 0, 214, 'watermark', 6.5, 'mono', 'middle'); wmT.setAttribute('opacity', 0);
    const look = P(g, '', 'ln hot'); look.setAttribute('opacity', 0);
    const reset = () => {
      gsap.set(vs[0].vg, { opacity: 0 }); vs.forEach(v => { v.r.setAttribute('class', 'box'); gsap.set(v.vg, { opacity: v === vs[0] ? 0 : 1 }); });
      txs.forEach(t => { t.ln.setAttribute('class', 'ln d'); t.res.textContent = ''; gsap.set(t.tg, { opacity: .5 }); });
      gsap.set([wm, wmT, look], { opacity: 0 });
    };
    reset();
    const steps = [
      { cap: 'Each transaction reads one <b>snapshot</b>: the newest version committed before it began.', run: tl => {
        tl.set(txs[0].tg, { opacity: 1 }).call(() => txs[0].ln.setAttribute('class', 'ln hot'));
        tl.set(look, { attr: { d: `M150,${txs[0].y} C150,118 ${vx[2]},118 ${vx[2]},93` } }).to(look, { opacity: 1, duration: .5 }, .3);
        tl.call(() => vs[2].r.setAttribute('class', 'box hot'), null, .8);
        tl.call(() => { txs[0].res.textContent = 'sees 100'; }, null, 1);
      } },
      { cap: 'A write puts the new version in front and keeps the old one as a <b>delta</b>. Readers never block it.', run: tl => {
        tl.set(txs[1].tg, { opacity: 1 }).call(() => txs[1].ln.setAttribute('class', 'ln hot'));
        tl.call(() => { vs[0].t1.textContent = '130'; vs[0].t2.textContent = 'by T2'; });
        tl.fromTo(vs[0].vg, { opacity: 0, x: -20 }, { opacity: 1, x: 0, duration: .6 }, .3);
        tl.call(() => vs[0].r.setAttribute('class', 'box warm'), null, .6);
        tl.call(() => { txs[1].res.textContent = 'writes 130'; }, null, .8);
      } },
      { cap: 'Two writers on one key: <b>the first one wins</b>. The second aborts, with no validation pass.', run: tl => {
        tl.set(txs[2].tg, { opacity: 1 }).call(() => txs[2].ln.setAttribute('class', 'ln'));
        tl.call(() => { txs[2].res.textContent = 'abort · conflict'; }, null, .6);
        tl.call(() => { txs[1].res.textContent = 'commits'; }, null, 1.1);
      } },
      { cap: 'Once no snapshot can see an old version, it is <b>collected</b> behind the watermark.', run: tl => {
        tl.set([wm, wmT], { opacity: 1, x: 286 });
        tl.fromTo([wm, wmT], { x: 300 }, { x: 214, duration: 1.2, ease: 'power2.inOut' }, .2);
        tl.to(vs[3].vg, { opacity: .12, duration: .5 }, 1.3);
      } },
    ];
    return { steps, reset };
  })();

  /* ===== 5 · COMMIT ===== */
  const commit = (() => {
    const { g } = G.commit;
    const lanes = [0, 1, 2, 3].map(i => {
      const y = 60 + i * 30;
      T(g, 14, y + 4, `log ${i}`, 7);
      R(g, 54, y - 7, 172, 14, 'box', 2);
      const writ = R(g, 55, y - 6, 0, 12, 'soft', 1.5);
      const dur = R(g, 55, y - 6, 0, 12, 'fillw', 1.5);
      const ok = T(g, 288, y + 3, '', 7, 'mono', 'end');
      return { y, writ, dur, ok, w: 30 + i * 18, d: 10 + i * 8 };
    });
    T(g, 140, 46, 'written ▢  durable ▮', 6.5, 'mono', 'middle');
    const chip = mk('g', { opacity: 0 }, g); R(chip, 54, 182, 140, 20, 'box hot', 10); const chipT = T(chip, 124, 195, 'T on core 2 · ready', 7, 'mono', 'middle');
    const free = T(g, 204, 195, '', 7, 'mono');
    const dep = P(g, 'M0,0', 'ln hot'); dep.setAttribute('opacity', 0);
    const paint = () => lanes.forEach(l => { l.writ.setAttribute('width', l.w); l.dur.setAttribute('width', l.d); });
    const reset = () => { lanes.forEach((l, i) => { l.w = 30 + i * 18; l.d = 10 + i * 8; l.ok.textContent = ''; gsap.set([l.writ, l.dur], { opacity: 1 }); }); paint(); gsap.set([chip, dep], { opacity: 0 }); free.textContent = ''; chipT.textContent = 'T on core 2 · ready'; };
    reset();
    const grow = (tl, i, dw, at, dd = 0) => { const l = lanes[i], o = { w: 0, d: 0 }; tl.to(o, { w: dw, d: dd, duration: .9, ease: 'power2.out', onStart: () => { o.w0 = l.w; o.d0 = l.d; }, onUpdate: () => { if (o.w0 === undefined) { o.w0 = l.w; o.d0 = l.d; } l.writ.setAttribute('width', Math.min(170, o.w0 + o.w)); l.dur.setAttribute('width', Math.min(170, o.d0 + o.d)); }, onComplete: () => { if (o.w0 === undefined) { o.w0 = l.w; o.d0 = l.d; } l.w = Math.min(170, o.w0 + dw); l.d = Math.min(170, o.d0 + dd); } }, at); };
    const steps = [
      { cap: 'Every core writes <b>its own log</b>. There is no global sequence number to fight over.', run: tl => {
        [0, 1, 2, 3].forEach(i => grow(tl, i, 22 + i * 7, .1 + i * .25));
      } },
      { cap: 'A transaction <b>pre-commits</b>: its record is appended and its core is free at once.', run: tl => {
        grow(tl, 2, 16, .1);
        tl.to(chip, { opacity: 1, duration: .4 }, .4);
        tl.call(() => { free.textContent = 'core 2 free'; }, null, .9);
      } },
      { cap: 'It is acknowledged once its own log is durable, and <b>only the logs it read from</b>. Logs 1 and 3 never wait.', run: tl => {
        tl.set(chip, { opacity: 1 });
        tl.set(dep, { attr: { d: `M60,${lanes[2].y} C40,${lanes[2].y} 40,${lanes[0].y} 60,${lanes[0].y}` } }).to(dep, { opacity: 1, duration: .4 }, .1);
        grow(tl, 0, 0, .5, lanes[0].w - lanes[0].d);
        grow(tl, 2, 0, .7, lanes[2].w - lanes[2].d + 16);
        tl.to([lanes[1].writ, lanes[1].dur, lanes[3].writ, lanes[3].dur], { opacity: .35, duration: .4 }, .5);
        tl.call(() => { lanes[0].ok.textContent = 'durable'; lanes[2].ok.textContent = 'durable'; chipT.textContent = 'T acknowledged ✓'; }, null, 1.7);
      } },
      { cap: 'Want the answer sooner? An <b>eventual</b> commit answers once its log write is issued, and hardens later.', run: tl => {
        tl.set(chip, { opacity: 1 }).call(() => { chipT.textContent = 'T answered · eventual'; free.textContent = ''; });
        grow(tl, 1, 26, .2); grow(tl, 1, 0, 1.2, 40);
        tl.call(() => { lanes[1].ok.textContent = 'hardened'; }, null, 2.1);
      } },
    ];
    return { steps, reset };
  })();

  /* ===== 6 · DEVICE ===== */
  const device = (() => {
    const { g } = G.device;
    const files = ['catalog', 'log', 'data', 'versions'].map((n, i) => {
      const y = 50 + i * 38;
      const r = R(g, 14, y, 92, 28, 'box', 4);
      T(g, 24, y + 17, n, 8);
      return { r, y };
    });
    const rings = [0, 1, 2, 3].map(i => {
      const cx = 176 + (i % 2) * 64, cy = 72 + ((i / 2) | 0) * 50;
      const c = mk('circle', { cx, cy, r: 18, class: 'box' }, g);
      for (let k = 0; k < 8; k++) { const a = k / 8 * Math.PI * 2; mk('circle', { cx: cx + Math.cos(a) * 13, cy: cy + Math.sin(a) * 13, r: 1.2, class: 'soft' }, g); }
      T(g, cx, cy + 3, `core ${i}`, 6.5, 'mono', 'middle');
      return { c, cx, cy };
    });
    T(g, 208, 112 + 50, 'one io_uring per core · O_DIRECT', 6.5, 'mono', 'middle');
    const pg = mk('g', { opacity: 0 }, g); R(pg, 0, 0, 26, 30, 'box hot', 2); T(pg, 13, 19, '4K', 7, 'mono', 'middle');
    const crc = T(g, 208, 220, '', 7, 'mono', 'middle');
    const reads = [0, 1, 2, 3].map(() => mk('circle', { r: 2.6, class: 'tok', opacity: 0 }, g));
    const flash = R(g, 0, 0, 300, 230, 'fillw', 10); flash.setAttribute('opacity', 0);
    const reset = () => { files.forEach(f => f.r.setAttribute('class', 'box')); rings.forEach(r => r.c.setAttribute('class', 'box')); gsap.set([pg, flash, ...reads], { opacity: 0 }); crc.textContent = ''; };
    reset();
    const steps = [
      { cap: 'A store is <b>four files</b>: the catalog, the log, the data and the versions.', run: tl => {
        files.forEach((f, i) => tl.call(() => f.r.setAttribute('class', 'box hot'), null, .2 + i * .3));
      } },
      { cap: 'On Linux each core owns an <b>io_uring</b> ring over O_DIRECT, and reaps only its own completions.', run: tl => {
        tl.call(() => files[2].r.setAttribute('class', 'box hot'));
        rings.forEach((r, i) => {
          tl.call(() => r.c.setAttribute('class', 'box hot'), null, .2 + i * .2);
          const p = mk('path', { d: `M106,${files[2].y + 14} C140,${files[2].y + 14} ${r.cx - 30},${r.cy} ${r.cx - 20},${r.cy}`, fill: 'none' }, g);
          tl.set(reads[i], { opacity: 1 }, .3 + i * .2); along(p, reads[i], tl, .8, .3 + i * .2); tl.set(reads[i], { opacity: 0 }, 1.1 + i * .2);
        });
      } },
      { cap: 'Every page carries a checksum over its bytes <b>and its id</b>, so a torn write is caught when it is read.', run: tl => {
        tl.set(pg, { opacity: 1, x: 130, y: 172 });
        tl.call(() => { crc.textContent = 'crc32c(id 4107 ‖ bytes) ✓'; }, null, .3);
        tl.to(pg, { x: 250, duration: .8 }, .9);
        tl.call(() => { crc.textContent = 'torn write · caught on read'; }, null, 1.8);
      } },
      { cap: 'After a crash the log is read back: committed work is <b>redone</b>, unfinished work <b>undone</b>.', run: tl => {
        tl.fromTo(flash, { opacity: .5 }, { opacity: 0, duration: .5 });
        tl.call(() => files[1].r.setAttribute('class', 'box hot'), null, .5);
        tl.call(() => files[2].r.setAttribute('class', 'box warm'), null, 1.2);
        tl.call(() => { crc.textContent = 'redo 1,204 records · undo 3 transactions'; }, null, 1.4);
      } },
    ];
    return { steps, reset };
  })();

  /* ===== ENGINE: one request through all six ===== */
  const enginePath = mk('path', { d: ENGINE_PATH, fill: 'none' }, svg);
  const lit = keys => ORDER.forEach(k => { G[k].r.classList.toggle('on', keys.includes(k)); if (curTab === 0) gsap.to(G[k].g, { opacity: keys.length && !keys.includes(k) ? .45 : 1, duration: .4, overwrite: 'auto' }); });
  const litLinks = ids => Object.entries(lk).forEach(([k, p]) => p.setAttribute('class', ids.includes(k) ? 'ln hot' : 'ln d'));
  const engineSeg = (tl, a, b, dur) => { const len = enginePath.getTotalLength(), o = { k: a }; tl.set(tok, { opacity: 1 }); tl.to(o, { k: b, duration: dur, ease: 'power1.inOut', onUpdate: () => { const pt = enginePath.getPointAtLength(o.k * len); tok.setAttribute('cx', pt.x); tok.setAttribute('cy', pt.y); } }); };
  const engine = {
    reset: () => { lit([]); litLinks([]); gsap.set(tok, { opacity: 0 }); },
    steps: [
      { cap: 'A request lands on a <b>core</b>. Every core runs its own worker, with its own queue.', run: tl => { tl.call(() => { lit(['cores']); litLinks([]); }); engineSeg(tl, 0, .08, 1); } },
      { cap: 'The worker walks the <b>B+tree</b>, from the root to the leaf that holds the key.', run: tl => { tl.call(() => { lit(['tree']); litLinks(['ct']); }); engineSeg(tl, .08, .3, 1.2); } },
      { cap: 'Each step turns a page id into a <b>frame in memory</b>, almost always without a miss.', run: tl => { tl.call(() => { lit(['pages']); litLinks(['tp']); }); engineSeg(tl, .3, .45, 1); } },
      { cap: 'Only a page that is not in memory reaches the <b>device</b>: one read, and the core works on.', run: tl => { tl.call(() => { lit(['device']); litLinks(['pd']); }); engineSeg(tl, .45, .62, 1); } },
      { cap: 'A write appends to <b>this core’s own log</b>. No other core stands in the way.', run: tl => { tl.call(() => { lit(['commit']); litLinks(['cd']); }); engineSeg(tl, .62, .78, 1); } },
      { cap: 'Readers see a <b>snapshot</b>; the old version stays as a delta until no one can see it.', run: tl => { tl.call(() => { lit(['snap']); litLinks(['sc', 'cs']); }); engineSeg(tl, .78, 1, 1.2); } },
    ],
  };

  /* ---------- the player ---------- */
  const TABS = [{ key: 'engine', label: 'Engine', n: '', m: engine }, ...ORDER.map(k => ({ key: k, label: BLOCKS[k].title, n: String(BLOCKS[k].n).padStart(2, '0'), m: { cores, tree, pages, snap, commit, device }[k] }))];
  const SEQ = []; TABS.forEach((t, ti) => t.m.steps.forEach((s, si) => SEQ.push({ ti, si })));
  const tabsEl = $('#anTabs'), ctlEl = $('#anCtl'), capEl = $('#anCap');
  tabsEl.innerHTML = TABS.map((t, i) => `<button role="tab" data-i="${i}">${t.n ? `<span class="label">${t.n}</span>` : ''}${t.label}</button>`).join('');
  const marks = $('#anMarks'); let acc = 0;
  TABS.forEach(t => { if (acc) { const m = document.createElement('i'); m.style.left = (acc / SEQ.length * 100) + '%'; marks.appendChild(m); } acc += t.m.steps.length; });

  let cur = -1, curTab = -1, tl = null, playing = false, inView = false, timer = null;
  const vb = { x: 0, y: 0, w: 1000, h: 560 };
  const setVB = () => svg.setAttribute('viewBox', `${vb.x} ${vb.y} ${vb.w} ${vb.h}`);
  const sec = document.getElementById('anatomy');
  const pinned = () => sec.offsetHeight > innerHeight * 1.5;
  // the scroll position of step i inside the pinned section
  const yOf = i => { const { ti, si } = SEQ[i], n = TABS[ti].m.steps.length; return sec.getBoundingClientRect().top + scrollY + (sec.offsetHeight - innerHeight) * ((ti + (si + .5) / n) / TABS.length); };
  const request = i => { i = (i + SEQ.length) % SEQ.length; if (pinned()) scrollToY(yOf(i)); else goTo(i); };
  function camTo(ti) {
    let target = { x: 0, y: 0, w: 1000, h: 560 };
    if (ti > 0) {
      const [x, y, w, h] = BLOCKS[TABS[ti].key].at, pad = 12, hh = h + pad * 2, ww = Math.max(w + pad * 2, hh * 1000 / 560), h2 = ww * 560 / 1000;
      target = { x: x + w / 2 - ww / 2, y: y + h / 2 - h2 / 2, w: ww, h: h2 };
    }
    gsap.to(vb, { ...target, duration: .75, ease: 'power3.inOut', onUpdate: setVB, overwrite: 'auto' });
    ORDER.forEach((k, i) => { G[k].r.classList.remove('on'); gsap.to(G[k].g, { opacity: ti === 0 || ti === i + 1 ? 1 : 0, duration: .5, overwrite: 'auto' }); });
    gsap.to([links, tok], { opacity: ti === 0 ? 1 : 0, duration: .4, overwrite: 'auto' });
  }
  function goTo(i, autoplay = playing) {
    i = (i + SEQ.length) % SEQ.length;
    const { ti, si } = SEQ[i];
    if (tl) tl.kill();
    clearTimeout(timer);
    const moved = ti !== curTab;
    if (moved) {
      TABS[curTab]?.m.reset();
      curTab = ti;
      if (ti !== 0) engine.reset();
      camTo(ti);
      [...tabsEl.children].forEach((b, k) => b.classList.toggle('on', k === ti));
      ctlEl.innerHTML = ''; TABS[ti].m.controls?.(ctlEl);
    }
    const m = TABS[ti].m;
    m.reset();
    for (let j = 0; j < si; j++) { const t0 = gsap.timeline({ paused: true }); m.steps[j].run(t0); t0.progress(1); t0.kill(); }
    if (ti === 0) { /* the engine's token must not be left where a skipped step put it */ }
    tl = gsap.timeline({ delay: moved ? .7 : .05 });
    m.steps[si].run(tl);
    cur = i;
    capEl.innerHTML = m.steps[si].cap;
    gsap.fromTo(capEl, { opacity: 0, y: 6 }, { opacity: 1, y: 0, duration: .4, delay: moved ? .5 : 0, overwrite: 'auto' });
    const k = (i + .5) / SEQ.length * 100;
    $('#anFill').style.width = k + '%'; $('#anKnob').style.left = k + '%';
    $('#anCount').textContent = `${String(i + 1).padStart(2, '0')} / ${SEQ.length}`;
    if (autoplay) timer = setTimeout(() => { if (playing && inView) request(cur + 1); }, Math.max(3600, (tl.duration() + 1.6) * 1000));
  }
  const setPlaying = v => { playing = v; $('#anPlayIcon').setAttribute('d', v ? 'M5 3h2v10H5zM9 3h2v10H9z' : 'M5 3l8 5-8 5z'); $('#anPlay').setAttribute('aria-label', v ? 'Pause' : 'Play'); if (v) request(cur + 1); else clearTimeout(timer); };
  $('#anPlay').addEventListener('click', () => setPlaying(!playing));
  $('#anNext').addEventListener('click', () => request(cur + 1));
  tabsEl.addEventListener('click', e => { const b = e.target.closest('button'); if (!b) return; const ti = +b.dataset.i; request(SEQ.findIndex(s => s.ti === ti)); });
  const scrub = $('#anScrub'); let drag = false;
  const fromX = e => { const r = scrub.getBoundingClientRect(); return Math.min(SEQ.length - 1, Math.max(0, Math.floor((e.clientX - r.left) / r.width * SEQ.length))); };
  scrub.addEventListener('pointerdown', e => { drag = true; scrub.setPointerCapture(e.pointerId); const i = fromX(e); if (i !== cur) request(i); });
  scrub.addEventListener('pointermove', e => { if (!drag) return; const i = fromX(e); if (i !== cur) request(i); });
  scrub.addEventListener('pointerup', () => { drag = false; });

  // the cores layer runs a live simulation
  let last = performance.now();
  gsap.ticker.add(() => { const now = performance.now(), dt = Math.min(.05, (now - last) / 1000); last = now; if (inView) cores.tick(dt); });

  // pinned: the section's scroll progress picks the step
  ScrollTrigger.create({ trigger: sec, start: 'top bottom', end: 'bottom top', onToggle: st => { inView = st.isActive; if (inView && cur < 0) goTo(0, false); } });
  ScrollTrigger.create({ trigger: sec, start: 'top top', end: 'bottom bottom', onUpdate: st => {
    if (!pinned()) return;
    const p = Math.min(.9999, st.progress) * TABS.length, ti = Math.floor(p);
    const n = TABS[ti].m.steps.length, si = Math.min(n - 1, Math.floor((p - ti) * n));
    const i = SEQ.findIndex(q => q.ti === ti && q.si === si);
    if (i !== cur) goTo(i, playing);
  } });
}
