// End-to-end flows for the business edition. Runs against the mock by default (see the config).
const { test, expect, email, isMock, seed, signUp } = require('./fixtures');

test.describe('onboarding', () => {
  test('sign up → research → review → find opportunities → dashboard', async ({ page }) => {
    await signUp(page, { name: 'Northwind Studio' });
    await expect(page.getByRole('heading', { name: /Getting to know Northwind Studio/ })).toBeVisible();
    await expect(page.locator('.feed li').first()).toBeVisible();
    await expect(page.getByRole('heading', { name: /Does this look right|Tell us about your business/ })).toBeVisible({ timeout: 60_000 });
    await expect(page.getByLabel('Business name')).toHaveValue('Northwind Studio');
    await page.getByRole('button', { name: 'Looks right, start prospecting' }).click();
    await expect(page.locator('.hero.is-stage')).toBeVisible();
    await expect(page.getByText('Prospecting', { exact: true })).toBeVisible();
    await expect(page.locator('.card').first()).toBeVisible({ timeout: 60_000 });
    await expect(page.getByRole('heading', { level: 1, name: 'Northwind Studio' })).toBeVisible();
    await expect(page.getByRole('tab', { name: /Opportunities/ })).toHaveAttribute('aria-selected', 'true');
    await expect(page.getByText(/opportunit(y|ies) for your business/)).toBeVisible();
  });

  test('the review screen shows what research found, with sources', async ({ page, request }) => {
    test.skip(!(await isMock(request)), 'needs the mock');
    await seed(page, 'review');
    await page.goto('/');
    const note = page.locator('.research-note');
    await expect(note).toContainText('Here’s what we found');
    await expect(note.locator('.confidence')).toContainText('% sure');
    await expect(note.locator('.sources a').first()).toHaveAttribute('rel', 'noopener noreferrer');
    await expect(note.locator('.found li').first()).toBeVisible();
  });

  test('a failed research run still lets the profile be filled in by hand', async ({ page, request }) => {
    test.skip(!(await isMock(request)), 'needs the mock');
    await seed(page, 'review_failed');
    await page.goto('/');
    await expect(page.getByRole('alert')).toContainText(/couldn.t read enough/);
    await page.getByLabel('What does your business do?').fill('Design studio for hotels');
    await page.getByRole('button', { name: 'Looks right, start prospecting' }).click();
    await expect(page.locator('.hero')).toBeVisible();
  });

  test('sign-up validation errors appear next to the right field', async ({ page }) => {
    await page.goto('/signup');
    await page.getByLabel('Business name').fill('Acme');
    await page.getByLabel('Website').fill('not a site');
    await page.getByLabel('Work email').fill(email());
    await page.getByLabel('Password', { exact: true }).fill('long enough');
    await page.getByRole('button', { name: 'Create account' }).click();
    await expect(page.locator('.field-error')).toContainText(/website/i);
    await expect(page.getByLabel('Website')).toHaveAttribute('aria-invalid', 'true');
    await page.getByLabel('Website').fill('acme.example');
    await page.getByLabel('Password', { exact: true }).fill('short');
    await page.getByRole('button', { name: 'Create account' }).click();
    await expect(page.getByLabel('Password', { exact: true })).toHaveAttribute('aria-invalid', 'true');
  });

  test('a taken email is reported on the email field', async ({ page }) => {
    const first = await signUp(page);
    await expect(page.getByRole('heading', { name: /Getting to know/ })).toBeVisible();
    await page.getByRole('button', { name: 'Sign out' }).click();
    await signUp(page, { mail: first.mail });
    await expect(page.getByLabel('Work email')).toHaveAttribute('aria-invalid', 'true');
    await expect(page.locator('.field-error')).toContainText(/already (an account|exists)/);
  });
});

test.describe('signed in', () => {
  test('sign in, wrong password, sign out', async ({ page, request }) => {
    test.skip(!(await isMock(request)), 'needs the mock');
    const mail = email('login');
    await page.request.post('/__mock/seed', { data: { state: 'dashboard', email: mail, password: 'correct horse' } });
    await page.context().clearCookies();
    await page.goto('/');
    await page.getByLabel('Email').fill(mail);
    await page.getByLabel('Password', { exact: true }).fill('wrong password');
    await page.getByRole('button', { name: 'Sign in' }).click();
    await expect(page.getByRole('alert')).toContainText('Email or password is incorrect');
    await page.getByLabel('Password', { exact: true }).fill('correct horse');
    await page.getByRole('button', { name: 'Sign in' }).click();
    await expect(page.locator('.card').first()).toBeVisible();
    await page.getByRole('button', { name: 'Sign out' }).last().click();
    await expect(page.getByRole('heading', { name: 'Welcome back' })).toBeVisible();
  });

  test.beforeEach(async ({ request }, info) => {
    if (info.title.startsWith('sign in')) return;
    test.skip(!(await isMock(request)), 'needs the mock');
  });

  test('intelligence tabs: only non-empty sections, sources on every item', async ({ page }) => {
    await seed(page, 'dashboard');
    await page.goto('/');
    await expect(page.getByRole('tab')).toHaveCount(8);
    const names = await page.getByRole('tab').allInnerTexts();
    expect(names.map((t) => t.replace(/\d+/g, '').trim())).toEqual(['Opportunities', 'Overview', 'Products & services', 'Customers', 'Competitors', 'Market', 'Regulation', 'News']);
    await page.getByRole('tab', { name: /Competitors/ }).click();
    await expect(page).toHaveURL(/tab=competitors/);
    await expect(page.locator('.bullseye')).toBeVisible();
    await expect(page.locator('.pill-hi').first()).toContainText('High overlap');
    for (const tab of ['Products & services', 'Customers', 'Market', 'Regulation', 'News']) {
      await page.getByRole('tab', { name: new RegExp(tab) }).click();
      const cards = page.locator('.icard');
      await expect(cards.first()).toBeVisible();
      const n = await cards.count();
      await expect(page.locator('.icard .src a')).toHaveCount(n);
    }
    await expect(page.locator('.tl-when b').first()).toBeVisible();
    await page.goBack();
    await expect(page.getByRole('tab', { name: /Regulation/ })).toHaveAttribute('aria-selected', 'true');
    await page.getByRole('tab', { name: 'Overview' }).click();
    await page.locator('.jump-card').filter({ hasText: 'Customers' }).click();
    await expect(page.getByRole('tab', { name: /Customers/ })).toHaveAttribute('aria-selected', 'true');
    await expect(page.getByText('Named customers').first()).toBeVisible();
  });

  test('refresh intelligence', async ({ page }) => {
    await seed(page, 'dashboard');
    await page.goto('/?tab=overview');
    await page.getByRole('button', { name: 'Refresh intelligence' }).click();
    await expect(page.locator('.refresh-bar')).toContainText('Researching');
    await expect(page.getByText('Intelligence updated')).toBeVisible({ timeout: 30_000 });
  });

  test('filter by focus area, and badges for internal sources', async ({ page }) => {
    await seed(page, 'dashboard');
    await page.goto('/');
    await expect(page.locator('.badge-src').filter({ hasText: 'From your Slack' })).toBeVisible();
    await expect(page.locator('.badge-src').filter({ hasText: 'From your Outlook' })).toBeVisible();
    await page.locator('.chips .chip').filter({ hasText: 'Sales' }).click();
    await expect(page).toHaveURL(/category=sales/);
    const cats = await page.locator('.card .card-cat').allInnerTexts();
    expect(cats.length).toBeGreaterThan(0);
    expect(cats.every((c) => /sales/i.test(c))).toBeTruthy();
    await page.locator('.chips .chip').filter({ hasText: 'All' }).click();
    await expect(page.locator('.card')).toHaveCount(14);
  });

  test('save, dismiss with undo, and the Saved view', async ({ page }) => {
    await seed(page, 'dashboard');
    await page.goto('/');
    const first = page.locator('.card').first();
    const title = await first.locator('.card-title').innerText();
    await first.getByRole('button', { name: 'Save' }).click();
    await expect(page.locator('.views button').nth(1)).toContainText('3');
    await page.getByRole('button', { name: /^Saved/ }).first().click();
    await expect(page).toHaveURL(/\/saved$/);
    await expect(page.locator('.card-title').filter({ hasText: title })).toBeVisible();
    await page.getByRole('button', { name: /^For you/ }).click();
    await expect(page).toHaveURL(/\/$/);
    await expect(page.locator('.card')).toHaveCount(14);
    await page.waitForTimeout(400);
    const second = page.locator('.card').nth(1);
    const gone = await second.locator('.card-title').innerText();
    await second.getByRole('button', { name: 'Not for me' }).click();
    await expect(page.locator('.card-title').filter({ hasText: gone })).toHaveCount(0);
    await page.getByRole('button', { name: 'Undo' }).click();
    await page.waitForTimeout(400);
    await expect(page.locator('.card-title').filter({ hasText: gone })).toHaveCount(1);
    await first.getByRole('button', { name: 'More like this' }).click();
    const up = page.locator('.card').first().getByRole('button', { name: 'More like this' });
    await expect(up).toHaveAttribute('aria-pressed', 'true');
    await up.click();
    await expect(up).toHaveAttribute('aria-pressed', 'false');
  });

  test('steering notes can be added and removed', async ({ page }) => {
    await seed(page, 'dashboard');
    await page.goto('/');
    await page.getByRole('button', { name: 'Steer' }).click();
    await page.getByLabel('What should we look for, or skip?').fill('More government tenders');
    await page.getByRole('button', { name: 'Add note' }).click();
    await expect(page.locator('.notes .tag').filter({ hasText: 'More government tenders' })).toBeVisible();
    await page.getByRole('button', { name: 'Remove note: More government tenders' }).click();
    await expect(page.locator('.notes .tag').filter({ hasText: 'More government tenders' })).toHaveCount(0);
  });

  test('settings persist, and /settings opens the panel', async ({ page }) => {
    await seed(page, 'dashboard');
    await page.goto('/settings');
    const drawer = page.locator('dialog.drawer');
    await expect(drawer).toBeVisible();
    await expect(page).toHaveURL(/\/$/);
    await drawer.getByLabel('Every day').check();
    await expect(drawer.locator('.saved-flag.is-on').first()).toBeVisible();
    await drawer.getByRole('switch').uncheck();
    await expect(drawer.getByText('Email isn’t switched on')).toBeVisible();
    await page.reload();
    await page.getByRole('button', { name: 'Settings and account' }).click();
    await expect(page.locator('dialog.drawer').getByLabel('Every day')).toBeChecked();
    await expect(page.locator('dialog.drawer').getByRole('switch')).not.toBeChecked();
  });

  test('edit the business profile in a modal', async ({ page }) => {
    await seed(page, 'dashboard');
    await page.goto('/settings');
    await page.getByRole('button', { name: 'Edit business profile' }).click();
    const modal = page.locator('dialog.modal');
    await expect(modal.getByLabel('Business name')).toHaveValue('Salt & Ember Catering');
    await modal.getByLabel('Goals for the next year').fill('Win three corporate contracts this quarter.');
    await modal.getByRole('button', { name: 'Save changes' }).click();
    await expect(page.getByText(/Saved\./)).toBeVisible();
  });

  test('AI model selector: DeepSeek, then Bedrock with validation, key hint and test', async ({ page }) => {
    await seed(page, 'dashboard');
    await page.goto('/');
    const chip = page.locator('.model-chip');
    await expect(chip).toContainText('DeepSeek Flash');
    await chip.click();
    const dlg = page.locator('dialog.modal');
    await dlg.getByText('DeepSeek V4 Pro').click();
    await dlg.getByRole('button', { name: 'Save' }).click();
    await expect(chip).toContainText('DeepSeek V4 Pro');
    await chip.click();
    await dlg.getByText('Claude on Amazon Bedrock').click();
    await dlg.getByLabel('Region').fill('nowhere');
    await dlg.getByRole('textbox', { name: 'Model ID or inference profile ARN' }).fill('anthropic.claude-sonnet');
    await dlg.getByLabel('Bedrock API key').fill('ABSK-test-key-7Q2c');
    await dlg.getByRole('button', { name: 'Save' }).click();
    await expect(dlg.getByLabel('Region')).toHaveAttribute('aria-invalid', 'true');
    await dlg.getByLabel('Region').fill('us-east-1');
    await dlg.getByRole('button', { name: 'Test connection' }).click();
    await expect(dlg.locator('.test-ok')).toContainText('Bedrock answered');
    await expect(chip).toContainText('Claude on Bedrock');
    await dlg.getByRole('button', { name: 'Close' }).click();
    await chip.click();
    await expect(dlg.getByText('Saved: ••••7Q2c')).toBeVisible();
  });

  test('connectors: Slack token, Microsoft sign-in, access log, disconnect', async ({ page }) => {
    await seed(page, 'dashboard');
    await page.goto('/settings');
    const drawer = page.locator('dialog.drawer');
    const slack = drawer.locator('.conn').filter({ hasText: 'Slack' });
    await slack.getByRole('button', { name: 'Connect with a token' }).click();
    await slack.getByLabel('Slack token').fill('xoxp-write-123');
    await slack.getByRole('button', { name: 'Connect', exact: true }).click();
    await expect(slack.locator('.field-error')).toContainText('read-only');
    await slack.getByLabel('Slack token').fill('xoxp-123456');
    await slack.getByRole('button', { name: 'Connect', exact: true }).click();
    await expect(drawer.locator('.conn').filter({ hasText: 'Slack' }).locator('.ro-badge')).toHaveText('Read-only · scopes verified');
    await drawer.getByRole('button', { name: 'Connect Microsoft 365' }).click();
    await expect(page.getByText('Microsoft 365 is connected. Access is read-only.')).toBeVisible();
    await expect(page).not.toHaveURL(/connected=/);
    await page.goto('/settings');
    await page.locator('dialog.drawer').getByRole('button', { name: 'Access log' }).click();
    await expect(page.locator('.audit li').first()).toBeVisible();
    const ms = page.locator('dialog.drawer .conn').filter({ hasText: 'Microsoft 365' });
    await ms.getByRole('button', { name: 'Disconnect' }).click();
    await expect(page.locator('dialog.drawer .conn').filter({ hasText: 'Microsoft 365' }).getByRole('button', { name: 'Connect Microsoft 365' })).toBeVisible();
  });

  test('a provider the administrator hasn’t enabled gets a calm line, not an error', async ({ page }) => {
    await page.request.post('/__mock/next', { data: { microsoft_oauth: false } });
    try {
      await seed(page, 'dashboard');
      await page.goto('/settings');
      await expect(page.locator('dialog.drawer .conn').filter({ hasText: 'Microsoft 365' })).toContainText('Your administrator can enable this.');
    } finally {
      await page.request.post('/__mock/next', { data: { microsoft_oauth: true } });
    }
  });

  test('a connector error on return is shown', async ({ page }) => {
    await seed(page, 'dashboard');
    await page.goto('/?connector_error=The%20sign-in%20was%20cancelled.');
    await expect(page.getByText('That connection didn’t go through: The sign-in was cancelled.')).toBeVisible();
  });

  test('empty and failed states', async ({ page }) => {
    await seed(page, 'empty');
    await page.goto('/');
    await expect(page.getByRole('heading', { name: 'Nothing worth acting on this round' })).toBeVisible();
    await seed(page, 'failed');
    await page.goto('/');
    await expect(page.getByRole('heading', { name: 'This search didn’t finish' })).toBeVisible();
    await page.getByRole('button', { name: 'Try again' }).last().click();
    await expect(page.locator('.card').first()).toBeVisible({ timeout: 60_000 });
  });

  test('works at phone width', async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await seed(page, 'dashboard');
    await page.goto('/');
    await expect(page.locator('.card').first()).toBeVisible();
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
    expect(overflow).toBeLessThanOrEqual(0);
    await page.getByRole('tab', { name: /Competitors/ }).click();
    await expect(page.locator('.bullseye')).toBeVisible();
  });
});
