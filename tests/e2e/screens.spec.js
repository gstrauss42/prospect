// Screenshots of every screen and state at desktop and phone sizes, into tests/e2e/screens/.
// Mock only (it uses /__mock/seed to reach each state quickly).
const path = require('path');
const { test, isMock, seed } = require('./fixtures');

const SIZES = { desktop: { width: 1440, height: 900 }, phone: { width: 390, height: 844 } };
const toSheet = (page) => page.evaluate(() => window.scrollTo(0, document.querySelector('.sheet').offsetTop - 10));
const SCREENS = {
  'signin': async (page) => { await page.goto('/'); await page.waitForSelector('.auth-form'); },
  'signup': async (page) => { await page.goto('/signup'); await page.waitForSelector('.auth-form'); },
  'research': async (page) => { await seed(page, 'researching'); await page.goto('/'); await page.waitForSelector('.feed li'); await page.waitForTimeout(700); },
  'review': async (page) => { await seed(page, 'review'); await page.goto('/'); await page.waitForSelector('.research-note'); },
  'review-failed': async (page) => { await seed(page, 'review_failed'); await page.goto('/'); await page.waitForSelector('.pf-form'); },
  'loading': async (page) => { await seed(page, 'loading', { theme_ready: true }); await page.goto('/'); await page.waitForSelector('.feed li'); await page.waitForTimeout(900); },
  'dashboard': async (page) => { await seed(page, 'dashboard'); await page.goto('/'); await page.waitForSelector('.card'); await page.waitForTimeout(1200); },
  'opportunities': async (page) => { await seed(page, 'dashboard'); await page.goto('/'); await page.waitForSelector('.card'); await page.waitForTimeout(900); await toSheet(page); },
  'refreshing': async (page) => { await seed(page, 'refreshing'); await page.goto('/'); await page.waitForSelector('.refresh-bar'); await toSheet(page); },
  'empty': async (page) => { await seed(page, 'empty'); await page.goto('/'); await page.waitForSelector('.state'); },
  'failed': async (page) => { await seed(page, 'failed'); await page.goto('/'); await page.waitForSelector('.state'); },
  'settings': async (page) => { await seed(page, 'dashboard', { connections: true }); await page.goto('/settings'); await page.waitForSelector('.conn'); },
  'model': async (page) => { await seed(page, 'dashboard'); await page.goto('/'); await page.waitForSelector('.card'); await page.click('.model-chip'); await page.click('text=Claude on Amazon Bedrock'); },
};
for (const tab of ['overview', 'offerings', 'segments', 'competitors', 'trends', 'regulation', 'news']) {
  SCREENS[`tab-${tab}`] = async (page) => { await seed(page, 'dashboard'); await page.goto(`/?tab=${tab}`); await page.waitForSelector('.intel-head'); await page.waitForTimeout(600); await toSheet(page); };
}

for (const [size, viewport] of Object.entries(SIZES)) {
  test.describe(`screens at ${size}`, () => {
    test.use({ viewport });
    for (const [name, go] of Object.entries(SCREENS)) {
      test(name, async ({ page, request }) => {
        test.skip(!(await isMock(request)), 'needs the mock');
        await go(page);
        await page.waitForTimeout(300);
        await page.screenshot({ path: path.join(__dirname, 'screens', `${name}-${size}.png`) });
      });
    }
  });
}
