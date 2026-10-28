import { defineConfig, devices } from '@playwright/test';

/* The panel against the mock host API (mock/server.mjs): the mock first,
 * then a seed made from its keys, then the panel built for production. */
const MOCK = 39298;
const PANEL = 3101;
const STATE = 'test-results/mock-state';
export const PASSWORD = 'playwright-only-password';

export default defineConfig({
  testDir: 'tests',
  timeout: 60_000,
  fullyParallel: false,
  workers: 1,
  reporter: [['list']],
  use: { baseURL: `http://localhost:${PANEL}`, trace: 'retain-on-failure' },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
  webServer: [
    {
      command: `node mock/server.mjs --port ${MOCK} --state ${STATE}`,
      port: MOCK,
      reuseExistingServer: false,
    },
    {
      command: `rm -rf test-results/panel-data && node mock/seed.mjs ${STATE} ${MOCK} ${PASSWORD} > test-results/seed.json`
        + ` && PANEL_SEED=test-results/seed.json PANEL_DATA=test-results/panel-data npx next start -p ${PANEL}`,
      port: PANEL,
      reuseExistingServer: false,
      timeout: 120_000,
    },
  ],
});
