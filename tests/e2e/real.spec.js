// End-to-end against the REAL server (scripts/e2e.sh: release binary + mock upstreams). Every
// test drives the actual product: real accounts, real research and discovery runs (DeepSeek,
// image sources, Bedrock and the connectors are mocked upstream), real settings and connectors.
// Skipped against the dev mock API, which has its own suite.
const { test, expect, email, isMock } = require('./fixtures');

const PASSWORD = 'correct horse battery';

test.beforeEach(async ({ request }) => {
  test.skip(await isMock(request), 'real-server suite');
});

async function until(page, fn, ms = 90_000) {
  const start = Date.now();
  while (Date.now() - start < ms) {
    const v = await fn();
    if (v) return v;
    await page.waitForTimeout(300);
  }
  throw new Error('timed out waiting');
}

const me = async (page) => (await page.request.get('/api/me')).json();
const idle = (page) => until(page, async () => {
  const c = await (await page.request.get('/api/runs/current')).json();
  return c.run ? null : c;
});

// A business account taken through research and confirmation with the real API; the page shares
// the session cookie.
async function onboard(page, name = 'Northwind Cleaning') {
  const mail = email('real');
  const r = await page.request.post('/api/auth/signup', {
    data: { business_name: name, website: 'https://northwind-cleaning.example', email: mail, password: PASSWORD, hints: 'Commercial office cleaning in Cape Town' },
  });
  expect(r.status(), await r.text()).toBe(201);
  const m = await until(page, async () => { const x = await me(page); return x.user.onboarding === 'review' ? x : null; });
  const p = await page.request.put('/api/profile', { data: { profile: m.profile } });
  expect(p.ok(), await p.text()).toBeTruthy();
  await idle(page);
  return { mail, name };
}

test('sign up → live research → review with sources → opportunities and every intelligence tab', async ({ page }) => {
  await page.goto('/signup');
  await page.getByLabel('Business name').fill('Harbour Logistics');
  await page.getByLabel('Website').fill('harbour-logistics.example');
  await page.getByLabel('Work email').fill(email('ui'));
  await page.getByLabel('Password', { exact: true }).fill(PASSWORD);
  await page.getByRole('button', { name: 'Create account' }).click();
  await expect(page.getByRole('heading', { name: /Does this look right|Tell us about your business/ })).toBeVisible({ timeout: 90_000 });
  await expect(page.locator('.research-note')).toBeVisible();
  await expect(page.locator('.research-note .sources a').first()).toHaveAttribute('href', /^https?:\/\//);
  await expect(page.getByLabel('Business name')).toHaveValue('Harbour Logistics');
  await page.getByRole('button', { name: 'Looks right, start prospecting' }).click();
  await expect(page.locator('.card').first()).toBeVisible({ timeout: 90_000 });
  await expect(page.getByRole('heading', { level: 1, name: 'Harbour Logistics' })).toBeVisible();
  // Every card links out to a real source.
  const links = page.locator('.card .card-title a');
  const n = await links.count();
  expect(n).toBeGreaterThan(0);
  for (let i = 0; i < n; i++) await expect(links.nth(i)).toHaveAttribute('href', /^https?:\/\//);
  // Each intelligence tab that's shown has items, each with a source link.
  const tabs = (await page.getByRole('tab').allInnerTexts()).map((t) => t.replace(/\d+/g, '').trim());
  expect(tabs[0]).toBe('Opportunities');
  expect(tabs).toContain('Overview');
  for (const tab of tabs.slice(2)) {
    await page.getByRole('tab', { name: new RegExp(tab.replace('&', '\\&')) }).click();
    if (tab === 'Competitors') await expect(page.locator('.bullseye')).toBeVisible();
    const cards = page.locator('.icard');
    await expect(cards.first()).toBeVisible();
    await expect(page.locator('.icard .src a')).toHaveCount(await cards.count());
  }
  await page.getByRole('tab', { name: 'Overview' }).click();
  await expect(page.locator('.intel')).toBeVisible();
});

test('sign out, wrong password, sign back in', async ({ page }) => {
  const { mail } = await onboard(page, 'Sign In Test Co');
  await page.goto('/');
  await expect(page.locator('.card').first()).toBeVisible();
  await page.getByRole('button', { name: 'Sign out' }).last().click();
  await expect(page.getByRole('heading', { name: 'Welcome back' })).toBeVisible();
  await page.getByLabel('Email').fill(mail);
  await page.getByLabel('Password', { exact: true }).fill('wrong password');
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('alert')).toContainText('Email or password is incorrect');
  await page.getByLabel('Password', { exact: true }).fill(PASSWORD);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.locator('.card').first()).toBeVisible();
});

test('save, the Saved view, dismiss with undo, and votes', async ({ page }) => {
  await onboard(page, 'Actions Test Co');
  await page.goto('/');
  const first = page.locator('.card').first();
  const title = await first.locator('.card-title').innerText();
  const total = await page.locator('.card').count();
  await first.getByRole('button', { name: 'Save' }).click();
  await expect(page.locator('.views button').nth(1)).toContainText('1');
  await page.getByRole('button', { name: /^Saved/ }).first().click();
  await expect(page.locator('.card-title').filter({ hasText: title })).toBeVisible();
  await page.getByRole('button', { name: /^For you/ }).click();
  await expect(page.locator('.card')).toHaveCount(total);
  await page.waitForTimeout(400);
  const second = page.locator('.card').nth(1);
  const gone = await second.locator('.card-title').innerText();
  await second.getByRole('button', { name: 'Not for me' }).click();
  await expect(page.locator('.card-title').filter({ hasText: gone })).toHaveCount(0);
  await page.getByRole('button', { name: 'Undo' }).click();
  await page.waitForTimeout(400);
  await expect(page.locator('.card-title').filter({ hasText: gone })).toHaveCount(1);
  const up = page.locator('.card').first().getByRole('button', { name: 'More like this' });
  await up.click();
  await expect(up).toHaveAttribute('aria-pressed', 'true');
  // It all survives a reload (it was saved on the server).
  await page.reload();
  await expect(page.locator('.views button').nth(1)).toContainText('1');
  await expect(page.locator('.card').first().getByRole('button', { name: 'More like this' })).toHaveAttribute('aria-pressed', 'true');
});

test('steering notes, settings and the business profile persist', async ({ page }) => {
  await onboard(page, 'Settings Test Co');
  await page.goto('/');
  await page.getByRole('button', { name: 'Steer' }).click();
  await page.getByLabel('What should we look for, or skip?').fill('More government tenders');
  await page.getByRole('button', { name: 'Add note' }).click();
  await expect(page.locator('.notes .tag').filter({ hasText: 'More government tenders' })).toBeVisible();
  await page.getByRole('button', { name: 'Remove note: More government tenders' }).click();
  await expect(page.locator('.notes .tag').filter({ hasText: 'More government tenders' })).toHaveCount(0);

  await page.goto('/settings');
  const drawer = page.locator('dialog.drawer');
  await expect(drawer).toBeVisible();
  await drawer.getByLabel('Every day').check();
  await expect(drawer.locator('.saved-flag.is-on').first()).toBeVisible();
  await drawer.getByRole('switch').uncheck();
  await page.waitForTimeout(500);
  await page.reload();
  await page.getByRole('button', { name: 'Settings and account' }).click();
  await expect(page.locator('dialog.drawer').getByLabel('Every day')).toBeChecked();
  await expect(page.locator('dialog.drawer').getByRole('switch')).not.toBeChecked();

  await page.getByRole('button', { name: 'Edit business profile' }).click();
  const modal = page.locator('dialog.modal');
  await expect(modal.getByLabel('Business name')).toHaveValue('Settings Test Co');
  await modal.getByLabel('Goals for the next year').fill('Win three corporate contracts this quarter.');
  await modal.getByRole('button', { name: 'Save changes' }).click();
  await expect(page.getByText(/Saved\./)).toBeVisible();
  const m = await me(page);
  expect(m.profile.goals).toBe('Win three corporate contracts this quarter.');
  await idle(page);
});

test('model selector: DeepSeek V4 Pro, then Claude on Bedrock with validation, a real test call and a masked key', async ({ page }) => {
  await onboard(page, 'Model Test Co');
  await page.goto('/');
  const chip = page.locator('.model-chip');
  await expect(chip).toContainText('DeepSeek Flash');
  await chip.click();
  const dlg = page.locator('dialog.modal');
  await dlg.getByText('DeepSeek V4 Pro').click();
  await dlg.getByRole('button', { name: 'Save' }).click();
  await expect(chip).toContainText('DeepSeek V4 Pro');
  expect((await (await page.request.get('/api/settings/ai')).json()).model).toBe('deepseek-v4-pro');
  await chip.click();
  await dlg.getByText('Claude on Amazon Bedrock').click();
  await dlg.getByLabel('Region').fill('not a region');
  await dlg.getByRole('textbox', { name: 'Model ID or inference profile ARN' }).fill('us.anthropic.claude-sonnet-4-5-20250929-v1:0');
  await dlg.getByLabel('Bedrock API key').fill('ABSKbW9jay1iZWRyb2NrLWFwaS1rZXk=');
  await dlg.getByRole('button', { name: 'Save' }).click();
  await expect(dlg.getByLabel('Region')).toHaveAttribute('aria-invalid', 'true');
  await dlg.getByLabel('Region').fill('us-east-1');
  await dlg.getByRole('button', { name: 'Save' }).click();
  await expect(chip).toContainText(/Bedrock/);
  await chip.click();
  await dlg.getByRole('button', { name: 'Test connection' }).click();
  await expect(dlg.locator('.test-ok')).toBeVisible({ timeout: 30_000 });
  await expect(dlg.getByText(/Saved: ••••/)).toBeVisible();
  const ai = await (await page.request.get('/api/settings/ai')).json();
  expect(ai.provider).toBe('bedrock');
  expect(JSON.stringify(ai)).not.toContain('ABSKbW9jay1iZWRyb2NrLWFwaS1rZXk=');
});

test('connectors: Slack token (write tokens refused), Microsoft 365 OAuth, access log, internal opportunities, disconnect', async ({ page, request }) => {
  await onboard(page, 'Connectors Test Co');
  await page.goto('/settings');
  const drawer = page.locator('dialog.drawer');
  const slack = drawer.locator('.conn').filter({ hasText: 'Slack' });
  await slack.getByRole('button', { name: 'Connect with a token' }).click();
  await slack.getByLabel('Slack token').fill('xoxp-mock-writer');
  await slack.getByRole('button', { name: 'Connect', exact: true }).click();
  await expect(slack.locator('.field-error')).toContainText(/read-only|more than read/i);
  await slack.getByLabel('Slack token').fill('xoxp-mock-ada');
  await slack.getByRole('button', { name: 'Connect', exact: true }).click();
  await expect(drawer.locator('.conn').filter({ hasText: 'Slack' }).locator('.ro-badge')).toHaveText('Read-only · scopes verified');

  await drawer.getByRole('button', { name: 'Connect Microsoft 365' }).click();
  await expect(page.getByText('Microsoft 365 is connected. Access is read-only.')).toBeVisible({ timeout: 30_000 });
  await page.goto('/settings');
  await page.locator('dialog.drawer').getByRole('button', { name: 'Access log' }).click();
  await expect(page.locator('.audit li').first()).toBeVisible();

  // A refresh reads the connected tools and can surface internal opportunities.
  const run = await page.request.post('/api/runs', { data: { kind: 'discover' } });
  expect(run.ok(), await run.text()).toBeTruthy();
  await idle(page);
  const list = await (await page.request.get('/api/opportunities')).json();
  expect(list.items.length).toBeGreaterThan(0);
  // Nothing was ever written to a connected tool.
  const v = await request.get(`${process.env.MOCK_URL}/_violations`);
  expect(await v.json()).toEqual([]);

  await page.goto('/settings');
  const ms = page.locator('dialog.drawer .conn').filter({ hasText: 'Microsoft 365' });
  await ms.getByRole('button', { name: 'Disconnect' }).click();
  await expect(page.locator('dialog.drawer .conn').filter({ hasText: 'Microsoft 365' }).getByRole('button', { name: 'Connect Microsoft 365' })).toBeVisible();
});

test('refresh opportunities and refresh intelligence run to completion', async ({ page }) => {
  await onboard(page, 'Refresh Test Co');
  await page.goto('/');
  await page.getByRole('button', { name: /Prospect again/ }).first().click();
  await expect(page.locator('.refresh-bar')).toBeVisible();
  await expect(page.locator('.refresh-bar')).toHaveCount(0, { timeout: 90_000 });
  await expect(page.locator('.card').first()).toBeVisible();
  await page.goto('/?tab=overview');
  await page.getByRole('button', { name: 'Refresh intelligence' }).click();
  await expect(page.getByText('Intelligence updated')).toBeVisible({ timeout: 90_000 });
});

test('works at phone width with no sideways scrolling', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await onboard(page, 'Phone Test Co');
  await page.goto('/');
  await expect(page.locator('.card').first()).toBeVisible();
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
  expect(overflow).toBeLessThanOrEqual(0);
  await page.getByRole('tab', { name: /Competitors/ }).click();
  await expect(page.locator('.bullseye')).toBeVisible();
});
