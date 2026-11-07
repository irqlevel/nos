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

/** How many connections a host has reset for going past what it lets in at
 * once, as its api.status says (`busy`): asked through the panel. */
async function pastLimits(page: Page, host: string): Promise<number> {
  const res = await page.request.post(`/api/h/${host}/call/api/status`, {
    headers: { Origin: new URL(page.url()).origin }, data: {},
  });
  expect(res.status()).toBe(200);
  return (await res.json()).busy;
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

test('the cluster: its hosts listed, a host invited and its token shown, the invitation taken back', async ({ page }) => {
  const host = await login(page);
  await page.goto(`/en/h/${host}/p/cluster`);
  await expect(page.getByRole('heading', { name: 'Cluster' })).toBeVisible();
  await expect(page.getByTestId('resource-hosts')).toContainText('mock-a');
  await expect(page.getByTestId('resource-hosts')).toContainText('mock-b');
  await page.getByRole('button', { name: /Invite a host/ }).click();
  const form = page.getByTestId('form-cluster-invite');
  await form.locator('input[name=host]').fill('mock-c');
  await form.locator('input[name=ip]').fill('10.9.0.3');
  await form.locator('input[name=key]').fill('ab'.repeat(32));
  await form.getByRole('button', { name: /Invite a host/ }).click();
  await expect(page.getByTestId('answer')).toContainText('nos1-');
  await page.goto(`/en/h/${host}/p/cluster`);
  const invitations = page.getByTestId('resource-invitations');
  await expect(invitations).toContainText('mock-c');
  await page.goto(`/ru/h/${host}/p/cluster`);
  await expect(page.getByRole('heading', { name: 'Кластер' })).toBeVisible();
  await expect(page.getByTestId('resource-invitations')).toContainText('mock-c');
  /* Taken back, with the confirm the descriptor says */
  await page.goto(`/en/h/${host}/p/cluster`);
  await invitations.getByRole('button', { name: 'Take back' }).click();
  const revoke = page.getByTestId('form-cluster-revoke');
  await revoke.getByRole('button', { name: 'Take back' }).click();
  await expect(revoke).toContainText('good for nothing');
  await revoke.getByRole('button', { name: 'Confirm' }).click();
  await expect(invitations).not.toContainText('mock-c');
});

test('block storage: volumes listed, one made, grown, refused smaller, deleted with its name typed', async ({ page }) => {
  const host = await login(page);
  await page.goto(`/en/h/${host}/p/nbs`);
  await expect(page.getByRole('heading', { name: 'Block storage' })).toBeVisible();
  await expect(page.getByTestId('resource-volumes')).toContainText('web-root');
  await expect(page.getByTestId('resource-cs')).toContainText('mock-d');
  await expect(page.getByTestId('resource-hosts')).toContainText('mock-b');
  await page.getByRole('button', { name: /New volume/ }).click();
  const form = page.getByTestId('form-nbs-volume_create');
  await form.locator('input[name=name]').fill('pw-vol');
  await form.locator('input[name=size_mib]').fill('64');
  await form.getByRole('button', { name: /New volume/ }).click();
  await expect(page.getByTestId('answer')).toContainText('"volume"');
  /* Made again with another size: refused, said in the form */
  await form.locator('input[name=size_mib]').fill('65');
  await form.getByRole('button', { name: /New volume/ }).click();
  await expect(form.getByRole('alert')).toContainText('another size');

  await page.goto(`/en/h/${host}/p/nbs`);
  const volumes = page.getByTestId('resource-volumes');
  await expect(volumes).toContainText('pw-vol');
  await volumes.getByRole('row', { name: /pw-vol/ }).getByRole('button', { name: 'Grow' }).click();
  const grow = page.getByTestId('form-nbs-volume_resize');
  await grow.locator('input[name=size_mib]').fill('32');
  await grow.getByRole('button', { name: 'Grow' }).click();
  await expect(grow.getByRole('alert')).toContainText('never made smaller');
  await grow.locator('input[name=size_mib]').fill('128');
  await grow.getByRole('button', { name: 'Grow' }).click();
  await expect(page.getByTestId('answer')).toContainText('"volume"');

  await page.goto(`/ru/h/${host}/p/nbs`);
  await expect(page.getByRole('heading', { name: 'Блочное хранилище' })).toBeVisible();
  await expect(page.getByTestId('resource-volumes')).toContainText('128');

  /* Deleted only once its name is typed again */
  await page.goto(`/en/h/${host}/p/nbs`);
  await volumes.getByRole('row', { name: /pw-vol/ }).getByRole('button', { name: 'Delete' }).click();
  const del = page.getByTestId('form-nbs-volume_delete');
  await del.getByRole('button', { name: 'Delete' }).click();
  await expect(del).toContainText('gone for good');
  const confirm = del.getByRole('button', { name: 'Confirm' });
  await expect(confirm).toBeDisabled();
  await del.getByLabel('Type it here').fill('pw-vo');
  await expect(confirm).toBeDisabled();
  await del.getByLabel('Type it here').fill('pw-vol');
  await confirm.click();
  await expect(volumes).not.toContainText('pw-vol');
  await expect(volumes).toContainText('web-root');
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

test('a VM made onto a device: its disk the device, one with something on it given only with force', async ({ page }) => {
  const host = await login(page);
  await page.goto(`/en/h/${host}/vms/new`);
  /* What the host may give is offered, what it may not is not: its root's
   * disk, its EFI System partition and its mounted root are left out. */
  const onto = page.locator('select[name=onto]');
  await expect(onto.locator('option')).toHaveText([/A file in its folder/, /nvme16/, /nvme17/]);
  await page.fill('input[name=name]', 'dv1');
  await onto.selectOption('part:704db7dd-0007-4e69-94e6-46e271b44854');
  await expect(page.locator('input[name=disk]')).toBeDisabled();
  await expect(page.getByTestId('onto-refused')).toContainText('holds an ext4 filesystem labelled "data"');
  /* Refused before anything is fetched: a job that ends at once, saying why. */
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page.getByTestId('job')).toContainText('give it anyway with force', { timeout: 20_000 });
  await expect(page.getByTestId('job-status')).not.toHaveText('ok');
  /* With force, the form afresh. */
  await page.goto(`/en/h/${host}/vms/new`);
  await page.fill('input[name=name]', 'dv1');
  await onto.selectOption('part:704db7dd-0007-4e69-94e6-46e271b44854');
  await page.locator('input[name=force]').check();
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page.getByTestId('job-status')).toHaveText('ok', { timeout: 20_000 });
  await expect(page.getByTestId('job')).toContainText('onto nvme17');

  /* Its disks: the device first, by its id, this boot's name beside it. */
  await page.goto(`/en/h/${host}/vms/dv1`);
  await page.getByRole('tab', { name: 'Disks' }).click();
  await expect(page.getByTestId('vm-disks')).toContainText('part:704db7dd-0007');
  await expect(page.getByTestId('vm-disks')).toContainText('nvme17');
  /* And a device given as it is, then taken off again. */
  const attach = page.getByTestId('form-vm-disk_attach');
  await attach.locator('input[name=device]').fill('part:704db7dd-0006-4e69-94e6-46e271b44854');
  await attach.getByRole('button', { name: 'Give to a VM' }).click();
  await attach.getByRole('button', { name: 'Confirm' }).click();
  await page.reload();
  await page.getByRole('tab', { name: 'Disks' }).click();
  await expect(page.getByTestId('vm-disks')).toContainText('nvme16');
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

  /* Its own pages, its own numbers: the panel reaches it with its key --
   * from cold, within what the host lets in at once */
  await b.getByRole('link', { name: 'nos-b' }).click();
  await expect(page.getByTestId('host-state')).toHaveText('online', { timeout: 15_000 });
  await expect(page.getByTestId('log')).toContainText('boot: complete', { timeout: 15_000 });
  const hostB = page.url().split('/h/')[1]!.split('/')[0]!;
  expect(await pastLimits(page, hostB)).toBe(0);

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

test('a page\'s requests at once: every one answered, none past what the host lets in', async ({ page }) => {
  /* The mock resets a connection past api.ko's limits -- two in their
   * handshake, four of one key -- as nos does: a panel that opened them as it
   * pleased had its pages fail with ECONNRESET on a real host. Here, after
   * every page the tests before opened, and a burst of a dozen at once. */
  const host = await login(page);
  const answers = await Promise.all(Array.from({ length: 12 }, () => page.request.get(`/api/h/${host}/providers`)));
  expect(answers.map((a) => a.status())).toEqual(Array(12).fill(200));
  expect(await pastLimits(page, host)).toBe(0);
});

test('the charts are drawn in the theme the page is in, switched on an open page too', async ({ page }) => {
  /* A canvas knows no CSS variables: a chart made once kept the light
   * theme's grid on the dark page, until a reload. */
  await login(page);
  const charts = page.getByTestId('chart');
  await expect(charts.first()).toHaveAttribute('data-theme', /^(dark|light)$/, { timeout: 15_000 });
  const theme = async () => ((await page.locator('html').getAttribute('class')) ?? '').includes('dark') ? 'dark' : 'light';
  for (let i = 0; i < 2; i++) {
    await page.getByRole('button', { name: 'Theme' }).click();
    const now = await theme();
    for (const chart of await charts.all()) {
      await expect(chart).toHaveAttribute('data-theme', now);
    }
  }
});

test('a table\'s values keep to one line in a narrow window: the table scrolls instead', async ({ page }) => {
  const host = await login(page);
  await page.setViewportSize({ width: 900, height: 800 });
  await page.goto(`/en/h/${host}/vms`);
  const cells = page.getByTestId('resource-vms').locator('tbody td');
  await expect(cells.first()).toBeVisible({ timeout: 15_000 });
  /* Every cell one line high: "8,192 MiB" and "38 min 32 s" were broken over
   * two and four */
  const tall = await cells.evaluateAll((tds) => tds.filter((td) => {
    /* Its text's own lines, by where each line box starts: a row is as tall
     * as its tallest cell, so a cell's height would name the whole row */
    const range = document.createRange();
    range.selectNodeContents(td);
    const tops = new Set([...range.getClientRects()].map((rect) => Math.round(rect.top)));
    return tops.size > 1 && !td.querySelector('form, button');
  }).map((td) => td.textContent));
  expect(tall).toEqual([]);
});

