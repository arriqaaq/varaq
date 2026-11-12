// Renders each <section> of figures.html to ../images/<id>.png at 2x.
// Run from the repository root: node docs/figures/render.js (needs Playwright with Chromium).
const path = require('path');
const { chromium } = require('playwright');
(async () => {
  const b = await chromium.launch();
  const p = await b.newPage({ viewport: { width: 1260, height: 900 }, deviceScaleFactor: 2 });
  await p.goto('file://' + path.join(__dirname, 'figures.html'), { waitUntil: 'networkidle' });
  await p.evaluate(() => document.fonts.ready);
  for (const id of await p.$$eval('section', s => s.map(x => x.id))) {
    await (await p.$('#' + id)).screenshot({ path: path.join(__dirname, '..', 'images', id + '.png'), omitBackground: true });
    console.log('docs/images/' + id + '.png');
  }
  await b.close();
})();
