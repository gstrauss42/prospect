// Playwright config for the Prospect web client.
//
//   npm test                       # starts web/dev/mock_api.py on :9941 (5x speed) and runs everything
//   BASE_URL=http://127.0.0.1:8080 npm test   # run the same suite against a real server instead
//
// Tests that need the mock's /__mock/ controls skip themselves when BASE_URL isn't the mock.
const { defineConfig } = require('@playwright/test');

const PORT = Number(process.env.MOCK_PORT || 9941);
const BASE_URL = process.env.BASE_URL || `http://127.0.0.1:${PORT}`;

module.exports = defineConfig({
  testDir: __dirname,
  testMatch: /.*\.spec\.js$/,
  timeout: 90_000,
  expect: { timeout: 20_000 },
  fullyParallel: true,
  workers: process.env.CI ? 2 : 4,
  reporter: [['list']],
  use: {
    baseURL: BASE_URL,
    viewport: { width: 1440, height: 900 },
    trace: 'retain-on-failure',
  },
  projects: [{ name: 'chromium', use: { browserName: 'chromium' } }],
  webServer: process.env.BASE_URL ? undefined : {
    command: `python3 ../../web/dev/mock_api.py --port ${PORT} --speed 5`,
    url: `http://127.0.0.1:${PORT}/healthz`,
    reuseExistingServer: false,
    timeout: 20_000,
  },
});
