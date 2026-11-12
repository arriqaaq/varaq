// Values of any size, as a pinned figure in three steps: in the leaf, in a value page, as one blob run.

const NS = 'http://www.w3.org/2000/svg';
const mk = (tag, attrs = {}, parent, text) => { const e = document.createElementNS(NS, tag); for (const k in attrs) e.setAttribute(k, attrs[k]); if (text != null) e.textContent = text; parent && parent.appendChild(e); return e; };
const $ = (s, r = document) => r.querySelector(s);

const STEPS = [
  { title: 'A small value lives <b>in its leaf</b>.',
    facts: [['Up to 506 bytes', 'kept in the leaf slot'], ['One page read', 'brings the key and the value'], ['A scan', 'reads values with no extra hop']] },
  { title: 'Past 506 bytes, it moves to a <b>value page</b>.',
    facts: [['The leaf keeps', 'an 8-byte reference and the first bytes'], ['A value page', 'packs several values'], ['A key-only scan', 'never touches it']] },
  { title: 'Bigger than a page: <b>one contiguous run</b>.',
    facts: [['No page-size ceiling', 'one run of pages, side by side'], ['A cold read', 'up to 256 pages, 1 MiB, per device read'], ['The bytes', 'never enter the log']] },
];

export function initValues({ gsap, ScrollTrigger, scrollToY }) {
  const sec = $('#values'); if (!sec) return;
  const svg = $('#vaSvg');
  const defs = mk('defs', {}, svg);
  const lg = mk('linearGradient', { id: 'vaGrad', x1: 0, y1: 0, x2: 1, y2: 0 }, defs);
  [['0', '#FFFFFF'], ['.4', '#E6DEFF'], ['.72', '#B7C2FF'], ['1', '#A4E8E1']].forEach(([o, c]) => mk('stop', { offset: o, 'stop-color': c }, lg));
  const T = (x, y, s, size = 11, cls = 'mono', anchor = 'start', p = svg) => mk('text', { x, y, class: cls, 'font-size': size, 'text-anchor': anchor }, p, s);

  // the leaf: a page of slots
  mk('rect', { x: 30, y: 40, width: 230, height: 270, rx: 8, class: 'box' }, svg);
  T(44, 64, 'leaf page · 4 KiB', 11);
  const keys = ['user:17', 'user:23', 'user:42', 'user:58', 'user:61', 'user:77'];
  const slots = keys.map((k, i) => {
    const y = 82 + i * 36;
    const r = mk('rect', { x: 44, y, width: 202, height: 26, rx: 4, class: 'box' }, svg);
    T(54, y + 17, k, 11);
    const bar = mk('rect', { x: 128, y: y + 9, width: 40 + (i * 23) % 60, height: 8, rx: 2, class: 'soft' }, svg);
    return { r, bar, y };
  });
  const hot = slots[2];
  const hotLab = T(246 - 8, hot.y + 17, '', 10, 'mono', 'end');

  // the value page
  const vp = mk('g', { opacity: 0 }, svg);
  mk('rect', { x: 330, y: 40, width: 240, height: 128, rx: 8, class: 'box' }, vp);
  T(344, 64, 'value page · several values', 11, 'mono', 'start', vp);
  [0, 1, 2].forEach(k => mk('rect', { x: 344, y: 80 + k * 26, width: 212, height: 18, rx: 3, class: k === 1 ? 'box hot' : 'box' }, vp));
  const arrow1 = mk('path', { d: `M246,${hot.y + 13} C290,${hot.y + 13} 300,107 344,107`, class: 'ln hot', opacity: 0 }, svg);

  // the blob run
  const run = mk('g', { opacity: 0 }, svg);
  T(344, 214, 'one contiguous run of pages', 11, 'mono', 'start', run);
  const runPages = [0, 1, 2, 3, 4, 5].map(k => mk('rect', { x: 330 + k * 40, y: 226, width: 36, height: 52, rx: 3, class: 'box' }, run));
  const sweep = mk('rect', { x: 330, y: 288, width: 0, height: 3, rx: 1.5, fill: 'url(#vaGrad)' }, run);
  T(570, 304, 'one read per 1 MiB', 11, 'mono', 'end', run);
  const arrow2 = mk('path', { d: `M246,${hot.y + 13} C300,${hot.y + 13} 300,252 330,252`, class: 'ln hot', opacity: 0 }, svg);
  const log = mk('g', { opacity: 0 }, svg);
  mk('rect', { x: 330, y: 316, width: 240, height: 18, rx: 3, class: 'box dim' }, log);
  T(340, 329, 'log · run bytes not written', 10, 'mono', 'start', log);

  let tl = null, cur = -1;
  function show(i) {
    if (i === cur) return; cur = i;
    if (tl) tl.kill();
    sec.querySelectorAll('#vaIdx li').forEach((li, k) => li.classList.toggle('on', k === i));
    const st = STEPS[i];
    $('#vaTitle').innerHTML = st.title;
    $('#vaFacts').innerHTML = st.facts.map(([a, b]) => `<li class="in"><span>${a}</span><span>${b}</span></li>`).join('');
    gsap.fromTo(['#vaTitle', '#vaFacts li'], { opacity: 0, y: 6 }, { opacity: 1, y: 0, duration: .4, stagger: .06 });
    slots.forEach(s => s.r.setAttribute('class', 'box'));
    hot.r.setAttribute('class', 'box hot');
    tl = gsap.timeline();
    tl.set([vp, run, log, arrow1, arrow2], { opacity: 0 });
    if (i === 0) { hotLab.textContent = '24 B · inline'; tl.fromTo(hot.bar, { attr: { width: 0 } }, { attr: { width: 70 }, duration: .6 }); }
    if (i === 1) { hotLab.textContent = 'ref · first bytes'; tl.set(hot.bar, { attr: { width: 0 } }).to(arrow1, { opacity: 1, duration: .4 }, .1).to(vp, { opacity: 1, duration: .5 }, .2); }
    if (i === 2) {
      hotLab.textContent = 'ref · first bytes'; tl.set(hot.bar, { attr: { width: 0 } }).to(arrow2, { opacity: 1, duration: .4 }, .1).to(run, { opacity: 1, duration: .5 }, .2);
      runPages.forEach((p, k) => tl.call(() => p.setAttribute('class', 'box hot'), null, .5 + k * .08));
      tl.fromTo(sweep, { attr: { width: 0 } }, { attr: { width: 236 }, duration: .7, ease: 'power2.inOut' }, .5).to(log, { opacity: 1, duration: .4 }, 1.1);
    }
  }
  const yOf = i => sec.getBoundingClientRect().top + scrollY + (sec.offsetHeight - innerHeight) * ((i + .5) / STEPS.length);
  sec.querySelectorAll('#vaIdx li').forEach((li, i) => li.addEventListener('click', () => scrollToY(yOf(i))));
  show(0);
  ScrollTrigger.create({ trigger: sec, start: 'top top', end: 'bottom bottom', onUpdate: s => show(Math.min(STEPS.length - 1, Math.floor(s.progress * STEPS.length * .9999))) });
}
