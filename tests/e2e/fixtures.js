// Shared fixtures: every test fails on console errors, page errors or CSP violations.
const base = require('@playwright/test');

// The browser logs expected API failures (401 before sign-in, 4xx from validation tests) as
// "Failed to load resource". Those are not app errors; everything else is.
const EXPECTED = /Failed to load resource: the server responded with a status of (401|404|409|422|429)/;

const test = base.test.extend({
  page: async ({ page }, use) => {
    const problems = [];
    page.on('console', (msg) => {
      if (msg.type() !== 'error') return;
      const text = msg.text();
      if (/Content.Security.Policy/i.test(text) || !EXPECTED.test(text)) problems.push(text);
    });
    page.on('pageerror', (err) => problems.push(`page error: ${err.message}`));
    await use(page);
    base.expect(problems, 'console errors or CSP violations').toEqual([]);
  },
});

let n = 0;
const email = (tag = 'e2e') => `${tag}-${Date.now().toString(36)}-${process.pid}-${++n}@example.com`;

// The dev mock answers {"mock": true}; the real server serves the app (HTML) for unknown paths,
// so only a JSON reply that says so counts.
async function isMock(request) {
  try {
    const r = await request.get('/__mock/ping');
    if (!r.ok() || !(r.headers()['content-type'] || '').includes('json')) return false;
    return (await r.json()).mock === true;
  } catch { return false; }
}

// Creates a business account directly in a given state (mock only) and signs the page in.
async function seed(page, state, extra = {}) {
  const r = await page.request.post('/__mock/seed', { data: { state, email: email('seed'), ...extra } });
  base.expect(r.ok()).toBeTruthy();
  return r.json();
}

async function signUp(page, { name = 'Northwind Studio', site = 'northwind.example', mail = email(), password = 'correct horse battery' } = {}) {
  await page.goto('/signup');
  await page.getByLabel('Business name').fill(name);
  await page.getByLabel('Website').fill(site);
  await page.getByLabel('Work email').fill(mail);
  await page.getByLabel('Password', { exact: true }).fill(password);
  await page.getByRole('button', { name: 'Create account' }).click();
  return { name, site, mail, password };
}

module.exports = { test, expect: base.expect, email, isMock, seed, signUp };
