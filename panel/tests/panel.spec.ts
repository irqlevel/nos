import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

import { expect, Page, test } from '@playwright/test';

import { MOCK_B, PASSWORD, STATE_B } from '../playwright.config';

async function login(page: Page, locale = 'en') {
  await page.goto(`/${locale}/login`);
  await page.fill('input[autocomplete=current-password]', PASSWORD);
  await page.click('button[type=submit]');
  await page.waitForURL(new RegExp(`/${locale}/h/`));
  return page.url().split('/h/')[1]!.split('/')[0]!;
}

test('a wrong password is refused, the right one lets in', async ({ page }) => {
  await page.goto('/en/login');
  await page.fill('input[autocomplete=current-password]', 'not-it');
  await page.click('button[type=submit]');
  await expect(page.getByText(/wrong user or password/i)).toBeVisible();
  await login(page);
  await expect(page.getByRole('link', { name: 'Dashboard' })).toBeVisible();
});

test('a page asked for with no session goes to the login', async ({ page }) => {
  await page.goto('/en/hosts');
  await expect(page).toHaveURL(/\/en\/login$/);
  const res = await page.request.get('/api/hosts');
  expect(res.status()).toBe(401);
});

test('a change from another origin is refused', async ({ page }) => {
  await login(page);
  const res = await page.request.post('/api/logout', { headers: { Origin: 'https://evil.example' } });
  expect(res.status()).toBe(403);
});

test('the dashboard: the host online, its numbers moving', async ({ page }) => {
  const host = await login(page);
  await expect(page.getByTestId('host-state')).toHaveText('online', { timeout: 15_000 });
  await expect(page.getByText(/^\d+(\.\d)? %$/).first()).toBeVisible({ timeout: 15_000 });
  await expect(page.getByTestId('log')).toContainText('boot: complete', { timeout: 15_000 });
  expect(host).toMatch(/^[0-9a-f-]{36}$/);
});

test('a provider\'s page is made of its descriptor, in both languages', async ({ page }) => {
  const host = await login(page);
  await page.goto(`/en/h/${host}/p/system`);
  await expect(page.getByRole('heading', { name: 'System' })).toBeVisible();
  await expect(page.getByTestId('resource-tasks')).toContainText('hv/vm0');
  await expect(page.getByTestId('resource-modules')).toContainText('sshd');
  await page.goto(`/ru/h/${host}/p/system`);
  await expect(page.getByRole('heading', { name: 'Система' })).toBeVisible();
  await expect(page.getByTestId('resource-modules')).toContainText('загружен');
});

test('a read run from its form shows its answer', async ({ page }) => {
  const host = await login(page);
  await page.goto(`/en/h/${host}/p/system`);
  await page.getByRole('button', { name: /Overview/ }).click();
  await page.getByTestId('form-system-info').getByRole('button', { name: /Overview/ }).click();
  await expect(page.getByTestId('answer')).toContainText('"arch": "x86_64"');
});

test('VMs: listed, created as a job, cloned, deleted with a confirm', async ({ page }) => {
  const host = await login(page);
  await page.goto(`/en/h/${host}/vms`);
  await expect(page.getByTestId('resource-vms')).toContainText('panel');
  await page.getByRole('link', { name: 'Create a VM' }).click();
  await page.fill('input[name=name]', 'pw1');
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page.getByTestId('job-status')).toHaveText('ok', { timeout: 20_000 });
  await expect(page.getByTestId('job')).toContainText('vm.toml written');

  await page.goto(`/en/h/${host}/vms/pw1`);
  await page.getByRole('button', { name: 'Clone' }).click();
  await page.fill('input[name=to]', 'pw2');
  await page.getByTestId('form-vm-clone').getByRole('button', { name: 'Clone' }).click();
  await expect(page.getByTestId('job-status')).toHaveText('ok', { timeout: 20_000 });

  await page.goto(`/en/h/${host}/vms/pw2`);
  await page.getByRole('button', { name: 'Delete' }).click();
  const form = page.getByTestId('form-vm-delete');
  await form.getByRole('button', { name: 'Delete' }).click();
  await expect(form).toContainText('cannot be brought back');
  /* Its name typed again, or nothing is deleted */
  const confirm = form.getByRole('button', { name: 'Confirm' });
  await expect(confirm).toBeDisabled();
  await form.getByLabel('Type it here').fill('pw');
  await expect(confirm).toBeDisabled();
  await form.getByLabel('Type it here').fill('pw2');
  await confirm.click();
  await expect(page.getByTestId('job-status')).toHaveText('ok', { timeout: 20_000 });
  await page.goto(`/en/h/${host}/vms`);
  await expect(page.getByTestId('resource-vms')).toContainText('pw1');
  await expect(page.getByTestId('resource-vms')).not.toContainText('pw2');
});

test('a protected VM is not deleted without force', async ({ page }) => {
  const host = await login(page);
  await page.goto(`/en/h/${host}/vms/panel`);
  await page.getByRole('button', { name: 'Delete' }).click();
  const form = page.getByTestId('form-vm-delete');
  await form.getByRole('button', { name: 'Delete' }).click();
  await form.getByLabel('Type it here').fill('panel');
  await form.getByRole('button', { name: 'Confirm' }).click();
  await expect(page.getByTestId('job-status')).toHaveText('conflict', { timeout: 20_000 });
  await expect(page.getByTestId('job')).toContainText('protected');
});

test('a VM\'s console: what it printed, and what is typed goes to it', async ({ page }) => {
  const host = await login(page);
  await page.goto(`/en/h/${host}/vms/web`);
  await page.getByRole('tab', { name: 'Console' }).click();
  const term = page.getByTestId('terminal');
  await expect(term).toContainText('hvboot', { timeout: 15_000 });
  await term.click();
  await page.keyboard.type('root');
  await page.keyboard.press('Enter');
  await expect(term).toContainText('root', { timeout: 10_000 });
});

/** A mock host's key, as nos pins one: what `api status` prints there. */
function pinOf(state: string): string {
  const cert = new crypto.X509Certificate(fs.readFileSync(path.join(state, 'host.crt')));
  return crypto.createHash('sha256').update(cert.publicKey.export({ type: 'spki', format: 'der' })).digest('hex');
}

test('a cluster: a second host added by its address and its key, the panel let in there by its own', async ({ page }) => {
  await login(page);
  await page.goto('/en/hosts');
  /* What lets the panel in there: its key, by a name of its own */
  const key = (await page.getByTestId('panel-key').textContent()) ?? '';
  expect(key).toMatch(/api client add panel-([0-9a-f]{8}) admin \1[0-9a-f]{56}/);

  /* A host that shows a key other than the one given is not reached, and
   * says why */
  const form = page.locator('form');
  const pinB = pinOf(STATE_B);
  const wrong = pinB.replace(/^./, (c) => (c === '0' ? '1' : '0'));
  await form.locator('input[name=name]').fill('wrong');
  await form.locator('input[name=url]').fill(`127.0.0.1:${MOCK_B}`);
  await form.locator('input[name=pin]').fill(wrong);
  await form.getByRole('button', { name: 'Add a host' }).click();
  await expect(page.getByTestId('host-wrong')).toContainText('offline', { timeout: 20_000 });
  await expect(page.getByTestId('host-wrong')).toContainText(pinB, { timeout: 20_000 });

  await form.locator('input[name=name]').fill('nos-b');
  await form.locator('input[name=url]').fill(`127.0.0.1:${MOCK_B}`);
  await form.locator('input[name=pin]').fill(pinB);
  await form.getByRole('button', { name: 'Add a host' }).click();
  const b = page.getByTestId('host-nos-b');
  await expect(b).toContainText('online', { timeout: 20_000 });

  /* Its own pages, its own numbers: the panel reaches it with its key */
  await b.getByRole('link', { name: 'nos-b' }).click();
  await expect(page.getByTestId('host-state')).toHaveText('online', { timeout: 15_000 });
  await expect(page.getByTestId('log')).toContainText('boot: complete', { timeout: 15_000 });

  await page.goto('/en/hosts');
  await page.getByTestId('host-wrong').getByRole('button', { name: 'Remove' }).click();
  await page.getByTestId('host-nos-b').getByRole('button', { name: 'Remove' }).click();
  await expect(page.getByTestId('host-nos-b')).toHaveCount(0);
  await expect(page.getByTestId('host-wrong')).toHaveCount(0);
});

test('the theme switches, and the language', async ({ page }) => {
  const host = await login(page);
  const dark = await page.evaluate(() => document.documentElement.classList.contains('dark'));
  await page.getByRole('button', { name: 'Theme' }).click();
  expect(await page.evaluate(() => document.documentElement.classList.contains('dark'))).toBe(!dark);
  await page.getByRole('link', { name: 'RU' }).click();
  await expect(page.getByRole('link', { name: 'Обзор' })).toBeVisible();
  expect(host).toBeTruthy();
});
