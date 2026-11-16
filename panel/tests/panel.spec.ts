import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

import { expect, Page, test } from '@playwright/test';

import { MOCK_B, PANEL_VERSION, PASSWORD, STATE_B } from '../playwright.config';
import { addressFor, copyable, ipOfUrl, isPublic } from '../src/lib/fills';
import { pastLimitWhy } from '../src/lib/nosapi';

async function login(page: Page, locale = 'en') {
  await page.goto(`/${locale}/login`);
  await page.fill('input[autocomplete=current-password]', PASSWORD);
  await page.click('button[type=submit]');
  await page.waitForURL(new RegExp(`/${locale}/h/`));
  return page.url().split('/h/')[1]!.split('/')[0]!;
}

/** How many times the page asks after a job's record in `ms`: none, once
 * the job is done -- a page drawn again began the polling anew, and a job
 * done was asked again in a loop. */
async function jobAsksIn(page: Page, ms: number): Promise<number> {
  let asked = 0;
  const count = (r: { url(): string }) => {
    if (/\/api\/h\/[^/]+\/jobs\/\d+/.test(r.url())) asked += 1;
  };
  page.on('request', count);
  await page.waitForTimeout(ms);
  page.off('request', count);
  return asked;
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

test('the panel\'s version in its footer, said to a session alone; the nos each host runs, on its page and among the hosts', async ({ page }) => {
  await page.goto('/en/login');
  await expect(page.getByTestId('panel-version')).toHaveCount(0);
  await login(page);
  await expect(page.getByTestId('panel-version')).toContainText(PANEL_VERSION);
  /* The host's system provider's info: its version, the commit, the architecture */
  await expect(page.getByTestId('host-nos')).toHaveText('nos 0.0.28-mock · rev-mock-a · x86_64', { timeout: 15_000 });
  await page.goto('/ru/hosts');
  await expect(page.getByTestId('panel-version')).toContainText(PANEL_VERSION);
  await expect(page.getByTestId('host-nos').first()).toHaveText('nos 0.0.28-mock · rev-mock-a · x86_64');
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

test('block storage: a snapshot of a volume, a clone of it, the clone made an image with its name typed', async ({ page }) => {
  const host = await login(page);
  await page.goto(`/en/h/${host}/p/nbs`);
  const volumes = page.getByTestId('resource-volumes');
  await volumes.getByRole('row', { name: /^\s*web-root\s/ }).getByRole('button', { name: 'Snapshot' }).click();
  const snap = page.getByTestId('form-nbs-volume_snapshot');
  await snap.locator('input[name=name]').fill('pw-snap');
  await snap.getByRole('button', { name: 'Snapshot' }).click();
  await expect(page.getByTestId('answer')).toContainText('"volume"');

  /* A volume is cloned once it is an image or a snapshot: refused of a volume */
  await page.goto(`/en/h/${host}/p/nbs`);
  await volumes.getByRole('row', { name: /^\s*web-root\s/ }).getByRole('button', { name: 'Clone' }).click();
  const refused = page.getByTestId('form-nbs-volume_clone');
  await refused.locator('input[name=name]').fill('pw-clone');
  await refused.getByRole('button', { name: 'Clone' }).click();
  await expect(refused.getByRole('alert')).toContainText('freeze or snapshot the volume first');

  await page.goto(`/en/h/${host}/p/nbs`);
  await expect(volumes.getByRole('row', { name: /^\s*pw-snap\s/ })).toContainText('snapshot');
  await expect(volumes.getByRole('row', { name: /^\s*pw-snap\s/ })).toContainText('web-root');
  await volumes.getByRole('row', { name: /^\s*pw-snap\s/ }).getByRole('button', { name: 'Clone' }).click();
  const clone = page.getByTestId('form-nbs-volume_clone');
  await clone.locator('input[name=name]').fill('pw-clone');
  await clone.getByRole('button', { name: 'Clone' }).click();
  await expect(page.getByTestId('answer')).toContainText('"volume"');

  /* The clone made an image only once its name is typed again */
  await page.goto(`/en/h/${host}/p/nbs`);
  await expect(volumes.getByRole('row', { name: /^\s*pw-clone\s/ })).toContainText('pw-snap');
  await volumes.getByRole('row', { name: /^\s*pw-clone\s/ }).getByRole('button', { name: 'Make an image' }).click();
  const freeze = page.getByTestId('form-nbs-volume_freeze');
  await freeze.getByRole('button', { name: 'Make an image' }).click();
  await expect(freeze).toContainText('read-only for good');
  await freeze.getByLabel('Type it here').fill('pw-clone');
  await freeze.getByRole('button', { name: 'Confirm' }).click();
  await page.goto(`/ru/h/${host}/p/nbs`);
  await expect(volumes.getByRole('row', { name: /^\s*pw-clone\s/ })).toContainText('образ');
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
  /* Its log said once, as the page around it is drawn again every three
   * seconds; and once it is done, nothing more asked of it */
  await expect(page.getByTestId('job').locator('pre')).toHaveText(
    'disk.raw -- 20480 MiB, 2215 MiB of it data\n1100 of 2215 MiB copied\nvm.toml written');
  expect(await jobAsksIn(page, 3500)).toBe(0);

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
   * disk, its EFI System partition and its mounted root are left out. NBS's
   * volumes are offered too, its images and snapshots not. */
  const onto = page.locator('select[name=onto]');
  await expect(onto.locator('option')).toContainText([/A file in its folder/, /nvme16/, /nvme17/, /sda/, /nbs:web-root/]);
  await expect(onto.locator('option', { hasText: /nvme0 |nvme01|nvme02|nbs:pw-snap/ })).toHaveCount(0);
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

test('firewalls: listed with their rules, one made, a rule refused and added, moved, taken out with a confirm, the firewall deleted', async ({ page }) => {
  const host = await login(page);
  await page.goto(`/en/h/${host}/p/fw`);
  await expect(page.getByRole('heading', { name: 'Firewalls', level: 1 })).toBeVisible();
  const list = page.getByTestId('resource-firewalls');
  await expect(list).toContainText('default');
  await expect(list.locator('tr', { hasText: 'web' })).toContainText('denied');
  await expect(page.getByTestId('resource-rules')).toContainText('allow in tcp 22');

  await page.getByRole('button', { name: /New firewall/ }).click();
  const create = page.getByTestId('form-fw-create');
  await create.locator('input[name=name]').fill('pw-fw');
  await create.getByRole('button', { name: /New firewall/ }).click();
  await expect(create.getByTestId('answer')).toContainText('"name": "pw-fw"');

  /* A rule from its row: one hv would not take said by its field, then two */
  await page.goto(`/en/h/${host}/p/fw`);
  await list.locator('tr', { hasText: 'pw-fw' }).getByRole('button', { name: 'Add a rule' }).click();
  const add = page.getByTestId('form-fw-add');
  await add.locator('input[name=rule]').fill('allow sideways');
  await add.getByRole('button', { name: 'Add a rule' }).click();
  await expect(add).toContainText('allow or deny, in or out');
  await add.locator('input[name=rule]').fill('allow in tcp 8080 from net');
  await add.getByRole('button', { name: 'Add a rule' }).click();
  await expect(add.getByTestId('answer')).toContainText('"n": 1');
  await add.locator('input[name=rule]').fill('deny out udp 53');
  await add.getByRole('button', { name: 'Add a rule' }).click();
  await expect(add.getByTestId('answer')).toContainText('"n": 2');

  /* Moved: the second first */
  await page.goto(`/en/h/${host}/p/fw`);
  const rules = page.getByTestId('resource-rules');
  await rules.locator('tr', { hasText: 'deny out udp 53' }).getByRole('button', { name: 'Move' }).click();
  const move = page.getByTestId('form-fw-move');
  await move.locator('input[name=to]').fill('1');
  await move.getByRole('button', { name: 'Move' }).click();
  await expect(move.getByTestId('answer')).toContainText('"n": 1');
  await page.goto(`/en/h/${host}/p/fw`);
  await expect(rules.locator('tr', { hasText: 'pw-fw' }).first()).toContainText('deny out udp 53');

  /* Taken out, with the confirm its descriptor says */
  await rules.locator('tr', { hasText: 'allow in tcp 8080' }).getByRole('button', { name: 'Take out' }).click();
  const remove = page.getByTestId('form-fw-remove');
  await remove.getByRole('button', { name: 'Take out' }).click();
  await expect(remove).toContainText('nothing else lets are cut');
  await remove.getByRole('button', { name: 'Confirm' }).click();
  /* Its row gone with it, the table read again */
  await expect(rules).not.toContainText('allow in tcp 8080');
  await expect(rules).toContainText('deny out udp 53');

  /* default is never deleted; pw-fw is */
  await page.goto(`/en/h/${host}/p/fw`);
  await list.locator('tr', { hasText: 'default' }).getByRole('button', { name: 'Delete' }).click();
  const del = page.getByTestId('form-fw-delete');
  await del.getByRole('button', { name: 'Delete' }).click();
  await del.getByRole('button', { name: 'Confirm' }).click();
  await expect(del.getByRole('alert')).toContainText('never deleted');
  await page.goto(`/en/h/${host}/p/fw`);
  await list.locator('tr', { hasText: 'pw-fw' }).getByRole('button', { name: 'Delete' }).click();
  await del.getByRole('button', { name: 'Delete' }).click();
  await del.getByRole('button', { name: 'Confirm' }).click();
  await expect(list).not.toContainText('pw-fw');
  await page.goto(`/ru/h/${host}/p/fw`);
  await expect(page.getByRole('heading', { name: 'Фаерволы', level: 1 })).toBeVisible();
  await expect(page.getByTestId('resource-firewalls')).not.toContainText('pw-fw');
});

test('the cluster\'s VM networks: listed, one made, one overlapping refused, a firewall of the cluster\'s given a rule', async ({ page }) => {
  const host = await login(page);
  await page.goto(`/en/h/${host}/p/vnet`);
  await expect(page.getByRole('heading', { name: 'VM networks', level: 1 })).toBeVisible();
  const networks = page.getByTestId('resource-networks');
  await expect(networks).toContainText('10.10.0.0/24');
  await expect(page.getByTestId('resource-ports')).toContainText('10.10.0.2');

  await page.getByRole('button', { name: /New network/ }).click();
  const create = page.getByTestId('form-vnet-network_create');
  await create.locator('input[name=name]').fill('pw-net');
  await create.locator('input[name=subnet]').fill('10.20.0.0/24');
  await create.getByRole('button', { name: /New network/ }).click();
  await expect(create.getByTestId('answer')).toContainText('"vni": 2');
  await create.locator('input[name=name]').fill('pw-over');
  await create.locator('input[name=subnet]').fill('10.20.0.128/25');
  await create.getByRole('button', { name: /New network/ }).click();
  await expect(create.getByRole('alert')).toContainText('overlaps');
  await page.goto(`/en/h/${host}/p/vnet`);
  await expect(networks).toContainText('pw-net');

  const firewalls = page.getByTestId('resource-firewalls');
  await expect(firewalls).toContainText('default');
  await firewalls.locator('tr', { hasText: 'default' }).getByRole('button', { name: 'Add a rule' }).click();
  const add = page.getByTestId('form-vnet-firewall_add');
  await add.locator('input[name=rule]').fill('allow in tcp 22');
  await add.getByRole('button', { name: 'Add a rule' }).click();
  await expect(add.getByTestId('answer')).toContainText('rule 1: allow in tcp 22');
  await page.goto(`/ru/h/${host}/p/vnet`);
  await expect(page.getByRole('heading', { name: 'Сети машин', level: 1 })).toBeVisible();
});

test('a VM\'s firewall and public address: chosen as it is made, on its page, the firewall changed from a list a read gives (choices)', async ({ page }) => {
  const host = await login(page);
  await page.goto(`/en/h/${host}/vms/new`);
  const chosen = page.locator('select[name=firewall]');
  await expect(chosen.locator('option')).toContainText(['default', 'panel', 'web']);
  await page.fill('input[name=name]', 'fw1');
  await chosen.selectOption('web');
  /* A public address, one to one: refused as no address first, then taken */
  await page.fill('input[name=public]', '203.0.113');
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page.getByRole('alert').filter({ hasText: 'public address' })).toContainText('a.b.c.d');
  await page.fill('input[name=public]', '203.0.113.9');
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page.getByTestId('job-status')).toHaveText('ok', { timeout: 20_000 });

  await page.goto(`/en/h/${host}/vms/fw1`);
  await expect(page.locator('dl div', { hasText: 'Public address' })).toContainText('203.0.113.9');
  const firewallRow = page.locator('dl div', { hasText: 'Firewall' });
  await expect(firewallRow).toContainText('web');
  await page.getByTestId('vm-firewall').click();
  const form = page.getByTestId('form-fw-assign');
  /* The VM is the page's; the firewall a list of the host's, from fw.list */
  await expect(form.locator('[name=vm]')).toHaveCount(0);
  const select = form.locator('select[name=firewall]');
  await expect(select.locator('option')).toContainText(['—', 'default', 'panel', 'web']);
  await select.selectOption('panel');
  await form.getByRole('button', { name: 'Change its firewall' }).click();
  await expect(form.getByTestId('answer')).toContainText('"firewall": "panel"');
  await expect(firewallRow).toContainText('panel');
});

test('a VM on a network of the cluster\'s: chosen as it is made, judged by one of the cluster\'s firewalls, its port made in vnet', async ({ page }) => {
  const host = await login(page);
  await page.goto(`/en/h/${host}/vms/new`);
  const network = page.locator('select[name=network]');
  await expect(network.locator('option')).toContainText(['vnet:lan']);
  await page.fill('input[name=name]', 'net1');
  await network.selectOption('vnet:lan');
  /* The cluster's firewalls offered, and no public address asked: vnet's */
  await expect(page.locator('select[name=firewall] option')).toContainText(['default']);
  await expect(page.locator('input[name=public]')).toHaveCount(0);
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page.getByTestId('job-status')).toHaveText('ok', { timeout: 20_000 });
  await page.goto(`/en/h/${host}/p/vnet`);
  await expect(page.getByTestId('resource-ports')).toContainText('10.10.0.3');
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

/** The streams a host has open on each VM's console, as the mock's
 * api.status says (`consoles`, the mock's own): asked through the panel. */
async function consoleStreams(page: Page, host: string): Promise<Record<string, number>> {
  const res = await page.request.post(`/api/h/${host}/call/api/status`, {
    headers: { Origin: new URL(page.url()).origin }, data: {},
  });
  expect(res.status()).toBe(200);
  return (await res.json()).consoles ?? {};
}

test('a VM\'s console in two tabs: one stream of the host\'s between them, both shown what it prints', async ({ page, context }) => {
  /* A stream a tab was a connection of the panel's key a tab: a second tab,
   * or a reload while the host held the last, went past api.ko's limit of
   * one key's, and the terminal said `read ECONNRESET` (2026-11-15). */
  const host = await login(page);
  await page.goto(`/en/h/${host}/vms/web`);
  await page.getByRole('tab', { name: 'Console' }).click();
  const one = page.getByTestId('terminal');
  await expect(one).toContainText('hvboot', { timeout: 15_000 });
  const second = await context.newPage();
  await second.goto(`/en/h/${host}/vms/web`);
  await second.getByRole('tab', { name: 'Console' }).click();
  const two = second.getByTestId('terminal');
  /* What it printed before the second tab came, given to it too */
  await expect(two).toContainText('hvboot', { timeout: 15_000 });
  expect((await consoleStreams(page, host)).web).toBe(1);
  await two.click();
  await second.keyboard.type('nos-two-tabs');
  await second.keyboard.press('Enter');
  await expect(two).toContainText('nos-two-tabs', { timeout: 10_000 });
  await expect(one).toContainText('nos-two-tabs', { timeout: 10_000 });
  /* Both gone: the stream closed a few seconds after */
  await second.close();
  await page.goto(`/en/h/${host}`);
  await expect.poll(async () => (await consoleStreams(page, host)).web ?? 0, { timeout: 20_000 }).toBe(0);
  expect(await pastLimits(page, host)).toBe(0);
});

test('a connection reset as it is made is said to be past the host\'s limit, not ECONNRESET', () => {
  const reset = Object.assign(new Error('read ECONNRESET'), { code: 'ECONNRESET' });
  expect(pastLimitWhy(reset, true)).toContain('max_per_key');
  /* Said as Node says it, the code lost on the way */
  expect(pastLimitWhy(new Error('10.0.100.1:39234: read ECONNRESET'), true)).toContain('limit');
  /* One that had answered before, or another failure, is not */
  expect(pastLimitWhy(reset, false)).toBeNull();
  expect(pastLimitWhy(new Error('connect ETIMEDOUT'), true)).toBeNull();
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
  /* The header's picker has it at once, the page not loaded again -- and
   * takes the panel to it */
  const picker = page.getByRole('banner').getByRole('combobox', { name: 'Hosts' });
  await expect(picker.locator('option', { hasText: 'nos-b' })).toHaveCount(1);
  await picker.selectOption({ label: 'nos-b' });
  await expect(page).toHaveURL(/\/en\/h\/[0-9a-f-]{36}$/);
  await expect(page.getByTestId('host-state')).toHaveText('online', { timeout: 15_000 });
  await page.goto('/en/hosts');

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

test('an invitation filled in from another host of the panel\'s, its token copied in a click', async ({ page, context }) => {
  const host = await login(page);
  await context.grantPermissions(['clipboard-read', 'clipboard-write']);
  await addHost(page, 'nos-b', MOCK_B, STATE_B);
  await page.goto(`/en/h/${host}/p/cluster`);
  await page.getByRole('button', { name: /Invite a host/ }).click();
  const form = page.getByTestId('form-cluster-invite');
  /* The panel's other hosts to choose from: not the one whose page it is */
  const from = form.locator('select[name=fill_from]');
  await expect(from.locator('option')).toHaveText(['—', 'nos-b']);
  await from.selectOption({ label: 'nos-b' });
  /* Its name, its ipcnet key and its port as its status says them; its
   * address its own -- a private one, the panel reaching it at a loopback */
  await expect(form.getByTestId('fill-note')).toHaveText('Filled in from nos-b.');
  await expect(form.locator('input[name=host]')).toHaveValue('nos-b');
  await expect(form.locator('input[name=key]')).toHaveValue(crypto.createHash('sha256').update('nos-b').digest('hex'));
  await expect(form.locator('input[name=ip]')).toHaveValue('10.9.0.5');
  await expect(form.locator('input[name=port]')).toHaveValue('');
  await form.getByRole('button', { name: /Invite a host/ }).click();
  const answer = form.getByTestId('answer');
  await expect(answer).toContainText('nos1-');
  const token = (JSON.parse((await answer.textContent()) ?? '{}') as { token: string }).token;
  /* The token copied in a click -- the one thing too long to select by eye */
  await expect(form.getByRole('button', { name: /^Copy / })).toHaveCount(1);
  const copy = form.getByRole('button', { name: 'Copy token' });
  await copy.click();
  await expect(copy).toContainText('Copied');
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(token);
  /* Taken back, and the host away again */
  const res = await page.request.post(`/api/h/${host}/call/cluster/revoke`, {
    headers: { Origin: new URL(page.url()).origin }, data: { host: 'nos-b' },
  });
  expect(res.status()).toBe(200);
  await removeHost(page, 'nos-b');
});

test('the address an invitation is filled in with: a public one first, the host\'s own before the panel\'s', () => {
  /* The demo stand's neighbour: its own address the AX41's bridge's, the
   * panel reaching it at the AX41's -- the NAT's */
  expect(addressFor('10.77.0.11', ipOfUrl('65.109.93.213:39251'))).toEqual({ ip: '65.109.93.213', from: 'reached' });
  /* A host of a public address of its own, the panel on its switch */
  expect(addressFor('65.108.38.184', ipOfUrl('10.0.0.1:39234'))).toEqual({ ip: '65.108.38.184', from: 'own' });
  /* Hosts of one private network */
  expect(addressFor('192.168.1.5', ipOfUrl('192.168.1.5:39234'))).toEqual({ ip: '192.168.1.5', from: 'own' });
  expect(addressFor('192.168.1.5', ipOfUrl('10.8.0.5:39234'))).toEqual({ ip: '192.168.1.5', from: 'own' });
  /* None of its own: the panel's, where it is an address */
  expect(addressFor(null, '203.0.113.7')).toEqual({ ip: '203.0.113.7', from: 'reached' });
  expect(addressFor(null, ipOfUrl('nos.example:39234'))).toEqual({ ip: '', from: 'none' });
  expect(ipOfUrl('[fd00::1]:39234')).toBeNull();
  expect(['10.1.2.3', '172.16.0.1', '172.31.255.255', '192.168.0.1', '100.64.0.1', '100.127.0.1', '127.0.0.1',
    '169.254.1.1', '0.0.0.0', '224.0.0.1', '255.255.255.255', '256.1.1.1', '1.2.3', 'x'].filter(isPublic)).toEqual([]);
  expect(['172.15.0.1', '172.32.0.1', '100.63.0.1', '100.128.0.1', '8.8.8.8', '65.109.93.213'].every(isPublic)).toBe(true);
  /* What a button copies: strings too long to select by eye, nested ones by
   * their path; a list's are not */
  expect(copyable({ host: 'ax41a', token: 'nos1-' + 'ab'.repeat(20), expires_s: 1,
    this_host: { name: 'n', key: 'cd'.repeat(32) }, hosts: ['x'.repeat(20)] }))
    .toEqual([['token', 'nos1-' + 'ab'.repeat(20)], ['this_host.key', 'cd'.repeat(32)]]);
  expect(copyable('x'.repeat(40))).toEqual([]);
});

/** A host added to the panel by its address and its key: its id, once it is
 * online and its page open. */
async function addHost(page: Page, name: string, port: number, state: string): Promise<string> {
  await page.goto('/en/hosts');
  const form = page.locator('form');
  await form.locator('input[name=name]').fill(name);
  await form.locator('input[name=url]').fill(`127.0.0.1:${port}`);
  await form.locator('input[name=pin]').fill(pinOf(state));
  await form.getByRole('button', { name: 'Add a host' }).click();
  const row = page.getByTestId(`host-${name}`);
  await expect(row).toContainText('online', { timeout: 20_000 });
  await row.getByRole('link', { name }).click();
  await expect(page.getByTestId('host-state')).toHaveText('online', { timeout: 15_000 });
  return page.url().split('/h/')[1]!.split('/')[0]!;
}

async function removeHost(page: Page, name: string) {
  await page.goto('/en/hosts');
  await page.getByTestId(`host-${name}`).getByRole('button', { name: 'Remove' }).click();
  await expect(page.getByTestId(`host-${name}`)).toHaveCount(0);
}

test('a host of no cluster: its registry made, then its NBS, a device given to its chunk server and drained', async ({ page }) => {
  await login(page);
  const b = await addHost(page, 'nos-b', MOCK_B, STATE_B);
  /* Added again after the test before took it away: the connections of then
   * closed with it, none of now reset past the four of one key */
  expect(await pastLimits(page, b)).toBe(0);

  /* No cluster yet: its tables say so, and its status says this host as an
   * invitation elsewhere asks for it */
  await page.goto(`/en/h/${b}/p/cluster`);
  await expect(page.getByTestId('resource-hosts')).toContainText('no member');
  await page.getByRole('button', { name: /^Member/ }).click();
  const status = page.getByTestId('form-cluster-status');
  await status.getByRole('button', { name: 'Member' }).click();
  await expect(status.getByTestId('answer')).toContainText('"member": false');
  await expect(status.getByTestId('answer')).toContainText('"name": "nos-b"');

  /* Made, of this host alone: a name not one refused first */
  await page.getByRole('button', { name: /^Make a cluster/ }).click();
  const init = page.getByTestId('form-cluster-init');
  await init.locator('input[name=name]').fill('Not One');
  await init.getByRole('button', { name: 'Make a cluster' }).click();
  await expect(init).toContainText("is not a cluster's name");
  await init.locator('input[name=name]').fill('pw-cluster');
  await init.getByRole('button', { name: 'Make a cluster' }).click();
  await expect(init.getByTestId('answer')).toContainText('"cluster": "pw-cluster"');
  await page.goto(`/en/h/${b}/p/cluster`);
  await expect(page.getByTestId('resource-hosts')).toContainText('nos-b');

  /* A join is a host's of no cluster: a job refused here now, saying why */
  await page.getByRole('button', { name: /^Join a cluster/ }).click();
  const join = page.getByTestId('form-cluster-join');
  await join.locator('input[name=token]').fill('nos1-' + 'ab'.repeat(40));
  await join.getByRole('button', { name: 'Join a cluster' }).click();
  await expect(join.getByTestId('job')).toContainText('a member of a cluster already', { timeout: 20_000 });

  /* Its NBS: none yet, then one of this host alone */
  await page.goto(`/en/h/${b}/p/nbs`);
  await expect(page.getByTestId('resource-hosts')).toContainText('in no NBS cluster');
  await page.getByRole('button', { name: /^Make an NBS cluster/ }).click();
  const nbsInit = page.getByTestId('form-nbs-init');
  await nbsInit.locator('input[name=name]').fill('pw-store');
  await nbsInit.getByRole('button', { name: 'Make an NBS cluster' }).click();
  await expect(nbsInit.getByTestId('answer')).toContainText('"cluster": "pw-store"');
  await page.goto(`/en/h/${b}/p/nbs`);
  await expect(page.getByTestId('resource-hosts')).toContainText('nos-b');
  await expect(page.getByTestId('resource-devices')).toContainText('Nothing');
  /* No device on any host yet: room on fewer than three, said first */
  const alerts = page.getByTestId('resource-alerts');
  await expect(alerts.getByRole('row', { name: /fewer than three hosts with room/ }).locator('[data-tone]'))
    .toHaveAttribute('data-tone', 'bad');

  /* A device with a filesystem on it: refused, then taken with wipe -- the
   * confirm asked each time */
  await page.getByRole('button', { name: /^Add a device/ }).click();
  const add = page.getByTestId('form-nbs-device_add');
  await add.locator('input[name=device]').fill('nvme17');
  await add.getByRole('button', { name: 'Add a device' }).click();
  await add.getByRole('button', { name: 'Confirm' }).click();
  await expect(add).toContainText("not NBS's");
  await add.locator('select[name=wipe]').selectOption('true');
  await add.getByRole('button', { name: 'Add a device' }).click();
  await add.getByRole('button', { name: 'Confirm' }).click();
  await expect(add.getByTestId('answer')).toContainText('"wiped": true');
  await page.goto(`/en/h/${b}/p/nbs`);
  const devices = page.getByTestId('resource-devices');
  await expect(devices).toContainText('part:704db7dd-0007');
  await expect(devices).toContainText('active');
  /* A VM is not given what NBS's chunk server has now */
  await page.goto(`/en/h/${b}/vms/new`);
  await expect(page.locator('select[name=onto]').locator('option')).toContainText([/nvme16/]);
  await expect(page.locator('select[name=onto]').locator('option', { hasText: 'nvme17' })).toHaveCount(0);

  /* Drained, with the confirm the descriptor says */
  await page.goto(`/en/h/${b}/p/nbs`);
  await devices.getByRole('row', { name: /704db7dd-0007/ }).getByRole('button', { name: 'Drain' }).click();
  const drain = page.getByTestId('form-nbs-device_drain');
  await drain.getByRole('button', { name: 'Drain' }).click();
  await expect(drain).toContainText('until it is added again');
  await drain.getByRole('button', { name: 'Confirm' }).click();
  await expect(devices).toContainText('draining');

  await removeHost(page, 'nos-b');
});

test('NBS\'s members: one removed with its name typed, a host off the registry not reached, one added and replaced as jobs', async ({ page }) => {
  const host = await login(page);
  await page.goto(`/en/h/${host}/p/nbs`);
  const hosts = page.getByTestId('resource-hosts');
  await expect(hosts).toContainText('mock-d');
  await hosts.getByRole('row', { name: /mock-d/ }).getByRole('button', { name: 'Remove' }).click();
  const remove = page.getByTestId('form-nbs-host_remove');
  await remove.getByRole('button', { name: 'Remove' }).click();
  await expect(remove).toContainText('for good');
  const confirm = remove.getByRole('button', { name: 'Confirm' });
  await expect(confirm).toBeDisabled();
  await remove.getByLabel('Type it here').fill('mock-d');
  await confirm.click();
  await expect(hosts).not.toContainText('mock-d');

  /* A host the registry has not: its offer reaches nobody, and says so */
  await page.getByRole('button', { name: /^Add a host/ }).click();
  const add = page.getByTestId('form-nbs-host_add');
  await add.locator('input[name=host]').fill('mock-q');
  await add.getByRole('button', { name: 'Add a host' }).click();
  await expect(add.getByTestId('job-status')).toHaveText('failed', { timeout: 20_000 });
  await expect(add.getByTestId('job')).toContainText('out of reach');

  /* The one removed, added again: a new member */
  await page.goto(`/en/h/${host}/p/nbs`);
  await page.getByRole('button', { name: /^Add a host/ }).click();
  await add.locator('input[name=host]').fill('mock-d');
  await add.getByRole('button', { name: 'Add a host' }).click();
  await expect(add.getByTestId('job-status')).toHaveText('ok', { timeout: 20_000 });
  await page.goto(`/en/h/${host}/p/nbs`);
  await expect(hosts).toContainText('mock-d');

  /* Replaced from its row, its name typed: the job's log said once, and
   * the table loaded again once at its end -- not a loop of it */
  await hosts.getByRole('row', { name: /mock-d/ }).getByRole('button', { name: 'Replace' }).click();
  const replace = page.getByTestId('form-nbs-host_replace');
  await replace.getByRole('button', { name: 'Replace' }).click();
  await replace.getByLabel('Type it here').fill('mock-d');
  await replace.getByRole('button', { name: 'Confirm' }).click();
  await expect(replace.getByTestId('job-status')).toHaveText('ok', { timeout: 20_000 });
  await expect(replace.getByTestId('job').locator('pre')).toHaveText(/^mock-d's old member \d+ removed\smock-d offered the NBS cluster anew$/);
  expect(await jobAsksIn(page, 3000)).toBe(0);
});

test('a VM of an image of NBS\'s: its disk a clone, named as asked; one onto a volume of NBS\'s', async ({ page }) => {
  const host = await login(page);
  /* A snapshot of its own to make VMs of */
  const snap = await page.request.post(`/api/h/${host}/call/nbs/volume_snapshot`, {
    headers: { Origin: new URL(page.url()).origin }, data: { volume: 'web-root', name: 'pw-base' },
  });
  expect(snap.status()).toBe(200);

  await page.goto(`/en/h/${host}/vms/new`);
  await page.fill('input[name=name]', 'nb1');
  await page.getByTestId('source').getByLabel(/An image of NBS/).check();
  const image = page.locator('select[name=image]');
  /* Images and snapshots, no volume */
  await expect(image.locator('option', { hasText: 'web-root' })).toHaveCount(0);
  await image.selectOption('pw-base');
  await expect(page.locator('input[name=disk]')).toHaveAttribute('placeholder', '2.0');
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page.getByTestId('job-status')).toHaveText('ok', { timeout: 20_000 });
  await expect(page.getByTestId('job')).toContainText('nbs:nb1 cloned from nbs:pw-base');
  await page.goto(`/en/h/${host}/p/nbs`);
  await expect(page.getByTestId('resource-volumes').getByRole('row', { name: /^\s*nb1\s/ })).toContainText('pw-base');
  await page.goto(`/en/h/${host}/vms/nb1`);
  await page.getByRole('tab', { name: 'Disks' }).click();
  await expect(page.getByTestId('vm-disks')).toContainText('nbs:nb1');

  /* Its clone named otherwise, grown -- and refused where that name is a
   * volume already, before anything is made */
  await page.goto(`/en/h/${host}/vms/new`);
  await page.fill('input[name=name]', 'nb2');
  await page.getByTestId('source').getByLabel(/An image of NBS/).check();
  await page.locator('select[name=image]').selectOption('pw-base');
  await page.fill('input[name=clone]', 'nb1');
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page.getByTestId('job')).toContainText('nbs:nb1 is there already', { timeout: 20_000 });
  await page.goto(`/en/h/${host}/vms/new`);
  await page.fill('input[name=name]', 'nb2');
  await page.getByTestId('source').getByLabel(/An image of NBS/).check();
  await page.locator('select[name=image]').selectOption('pw-base');
  await page.fill('input[name=clone]', 'nb2-disk');
  await page.fill('input[name=disk]', '4');
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page.getByTestId('job-status')).toHaveText('ok', { timeout: 20_000 });
  await expect(page.getByTestId('job')).toContainText('nbs:nb2-disk cloned from nbs:pw-base, 4096 MiB');

  /* A URL's image poured onto a volume of NBS's: no size asked, the volume's */
  await page.goto(`/en/h/${host}/vms/new`);
  await page.fill('input[name=name]', 'nb3');
  await page.locator('select[name=onto]').selectOption('nbs:web-root');
  await expect(page.locator('input[name=disk]')).toBeDisabled();
  await expect(page.getByTestId('onto-volume')).toContainText('poured over whatever the volume holds');
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page.getByTestId('job-status')).toHaveText('ok', { timeout: 20_000 });
  await expect(page.getByTestId('job')).toContainText('nbs:web-root: 3584 MiB poured');
});

test('images and snapshots: an image of a URL as a job, a VM of it from its row, its snapshot from the VM\'s page and a VM of that', async ({ page }) => {
  const host = await login(page);
  await page.goto(`/en/h/${host}/images`);
  await expect(page.getByRole('heading', { name: 'Images', exact: true })).toBeVisible();
  const form = page.getByTestId('new-image');
  /* A name NBS would refuse, refused before anything is fetched */
  await form.locator('input[name=name]').fill('Not One');
  await form.getByRole('button', { name: 'New image' }).click();
  await expect(form.getByRole('alert')).toContainText('lowercase');
  await form.locator('input[name=name]').fill('pw-img');
  await form.getByRole('button', { name: 'New image' }).click();
  await expect(form.getByTestId('job-status')).toHaveText('ok', { timeout: 20_000 });
  await expect(form.getByTestId('job')).toContainText('nbs:pw-img is an image now');
  const images = page.getByTestId('images');
  await expect(images.getByRole('row', { name: /pw-img/ })).toContainText('image');

  /* A VM of it from its row: the image picked already */
  await images.getByRole('row', { name: /pw-img/ }).getByRole('link', { name: 'Create a VM' }).click();
  await expect(page.getByTestId('source').getByLabel(/An image of NBS/)).toBeChecked();
  await expect(page.locator('select[name=image]')).toHaveValue('pw-img');
  await page.fill('input[name=name]', 'im1');
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page.getByTestId('job-status')).toHaveText('ok', { timeout: 20_000 });
  await expect(page.getByTestId('job')).toContainText('nbs:im1 cloned from nbs:pw-img');

  /* Its snapshot, from its page: listed there, a VM made of it from its row */
  await page.goto(`/en/h/${host}/vms/im1`);
  await expect(page.getByTestId('vm-snapshots')).toContainText('Nothing yet');
  await page.getByRole('button', { name: 'Snapshot' }).click();
  const snap = page.getByTestId('form-vm-snapshot');
  await snap.locator('input[name=snapshot]').fill('im1-s1');
  await snap.getByRole('button', { name: 'Snapshot' }).click();
  await expect(snap.getByTestId('answer')).toContainText('"snapshot": "im1-s1"');
  const snaps = page.getByTestId('vm-snapshots');
  await expect(snaps.getByRole('row', { name: /im1-s1/ })).toBeVisible();
  /* Taken again by the name: refused, the name NBS's already */
  await snap.getByRole('button', { name: 'Snapshot' }).click();
  await expect(snap.getByRole('alert')).toContainText('is a snapshot already');
  await snaps.getByRole('row', { name: /im1-s1/ }).getByRole('link', { name: 'Create a VM' }).click();
  await expect(page.locator('select[name=image]')).toHaveValue('im1-s1');
  await page.fill('input[name=name]', 'im2');
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(page.getByTestId('job-status')).toHaveText('ok', { timeout: 20_000 });
  await expect(page.getByTestId('job')).toContainText('nbs:im2 cloned from nbs:im1-s1');

  /* A VM of a file has no snapshot: said, though what it is said of -- the
   * VM's name -- is no field of the form */
  await page.goto(`/en/h/${host}/vms/web`);
  await page.getByRole('button', { name: 'Snapshot' }).click();
  const fileSnap = page.getByTestId('form-vm-snapshot');
  await fileSnap.locator('input[name=snapshot]').fill('web-s1');
  await fileSnap.getByRole('button', { name: 'Snapshot' }).click();
  await expect(fileSnap.getByRole('alert')).toContainText("no volume of NBS's");

  /* The images page has the snapshot too; the image deleted only once its
   * name is typed again */
  await page.goto(`/en/h/${host}/images`);
  await expect(images.getByRole('row', { name: /im1-s1/ })).toContainText('snapshot');
  await images.getByRole('row', { name: /pw-img/ }).getByRole('button', { name: 'Delete' }).click();
  const del = page.getByTestId('form-nbs-volume_delete');
  await del.getByRole('button', { name: 'Delete' }).click();
  const confirm = del.getByRole('button', { name: 'Confirm' });
  await expect(confirm).toBeDisabled();
  await del.getByLabel('Type it here').fill('pw-img');
  await confirm.click();
  await expect(images).not.toContainText('pw-img');
  await page.goto(`/ru/h/${host}/images`);
  await expect(page.getByRole('heading', { name: 'Образы', exact: true })).toBeVisible();
  await expect(page.getByTestId('images').getByRole('row', { name: /im1-s1/ })).toContainText('снимок');
});

test('a VM\'s disks: each one\'s IOPS and bandwidth charted as it runs, a volume of NBS\'s among them; one that is off says so', async ({ page }) => {
  const host = await login(page);
  await page.goto(`/en/h/${host}/vms/web`);
  await page.getByRole('tab', { name: 'Disks' }).click();
  const disk = page.getByTestId('disk-io').getByTestId('disk-io-disk.raw');
  await expect(disk.getByTestId('disk-io-now')).toContainText(/Reads [1-9]\d* IOPS · .+\/s\s+Writes [1-9]\d* IOPS/,
    { timeout: 15_000 });
  await expect(disk.getByTestId('chart')).toHaveCount(2, { timeout: 15_000 });

  /* A VM on a volume of NBS's, started: its disk's activity the same way */
  await page.goto(`/en/h/${host}/vms/im1`);
  await page.getByRole('button', { name: 'Start', exact: true }).click();
  await page.getByTestId('form-vm-start').getByRole('button', { name: 'Start', exact: true }).click();
  await expect(page.getByTestId('job-status')).toHaveText('ok', { timeout: 20_000 });
  await page.getByRole('tab', { name: 'Disks' }).click();
  const nbs = page.getByTestId('disk-io').getByTestId('disk-io-nbs:im1');
  await expect(nbs).toContainText('NBS');
  await expect(nbs.getByTestId('disk-io-now')).toContainText(/Reads [1-9]\d* IOPS/, { timeout: 15_000 });

  /* One that is off has nothing to chart, and says so */
  await page.goto(`/ru/h/${host}/vms/db`);
  await page.getByRole('tab', { name: 'Диски' }).click();
  await expect(page.getByTestId('disk-io')).toContainText('Не работает');
});

test('NBS\'s volumes: each one\'s health, coloured as its descriptor tones it', async ({ page }) => {
  const host = await login(page);
  await page.goto(`/en/h/${host}/p/nbs`);
  const volumes = page.getByTestId('resource-volumes');
  const row = (name: string) => volumes.getByRole('row', { name: new RegExp(`^\\s*${name}\\s`) });
  for (const [name, tone] of [['web-root', 'good'], ['deg-vol', 'warn'], ['copy-vol', 'warn'], ['crit-vol', 'alarm'],
    ['lost-vol', 'bad']]) {
    await expect(row(name!).locator('[data-tone]')).toHaveAttribute('data-tone', tone!);
  }
  await expect(row('crit-vol')).toContainText('one replica left');
  await page.goto(`/ru/h/${host}/p/nbs`);
  await expect(row('lost-vol')).toContainText('недоступен или реплики разные');
  await expect(row('web-root')).toContainText('в норме');
});

test('NBS\'s alerts: what to look at, the worst first, each coloured as its descriptor tones it', async ({ page }) => {
  const host = await login(page);
  await page.goto(`/en/h/${host}/p/nbs`);
  const alerts = page.getByTestId('resource-alerts');
  /* The page's first table */
  await expect(page.locator('[data-testid^="resource-"]').first()).toHaveAttribute('data-testid', 'resource-alerts');
  const row = (text: RegExp) => alerts.getByRole('row', { name: text });
  for (const [what, tone, count] of [[/no replica active/, 'bad', '1'], [/one replica left/, 'alarm', '2'],
    [/chunks at two replicas/, 'warn', '3'], [/being copied/, 'warn', '4']] as const) {
    await expect(row(what).locator('[data-tone]')).toHaveAttribute('data-tone', tone);
    await expect(row(what)).toContainText(count);
  }
  /* The worst first */
  await expect(alerts.locator('tbody tr').first()).toContainText('no replica active');
  await page.goto(`/ru/h/${host}/p/nbs`);
  await expect(row(/одной репликой/)).toBeVisible();
});

test('a VM\'s disk coloured as it is: its volume\'s health, and red while its I/O fails', async ({ page }) => {
  const host = await login(page);
  await page.goto(`/en/h/${host}/vms/web`);
  await page.getByRole('tab', { name: 'Disks' }).click();
  const file = page.getByTestId('disk-io-disk.raw');
  await expect(file).toHaveAttribute('data-tone', 'good', { timeout: 15_000 });
  await expect(file.getByTestId('disk-health')).toContainText('no I/O errors');
  await expect(file.getByTestId('disk-health')).toContainText(/latency \d+(\.\d)? ms/, { timeout: 15_000 });
  await expect(file.getByTestId('disk-slow')).toHaveCount(0);

  /* A replica down: yellow, the counts behind it said */
  await page.goto(`/en/h/${host}/vms/store`);
  await page.getByRole('tab', { name: 'Disks' }).click();
  const deg = page.getByTestId('disk-io-nbs:deg-vol');
  await expect(deg).toHaveAttribute('data-tone', 'warn', { timeout: 15_000 });
  await expect(deg.getByTestId('disk-health')).toContainText('a replica down or copying');
  await expect(deg.getByTestId('disk-health')).toContainText('Chunks at two replicas: 3');
  /* And slow: its requests' average latency past 50 ms, said */
  await expect(deg.getByTestId('disk-slow')).toContainText(/slow: average latency \d+(\.\d)? ms/, { timeout: 15_000 });

  /* A chunk unreachable, its guest's I/O failing: red */
  await page.goto(`/en/h/${host}/vms/broken`);
  await page.getByRole('tab', { name: 'Disks' }).click();
  const lost = page.getByTestId('disk-io-nbs:lost-vol');
  await expect(lost).toHaveAttribute('data-tone', 'bad', { timeout: 15_000 });
  await expect(lost.getByTestId('disk-health')).toContainText('I/O errors in the last minute', { timeout: 15_000 });
  await expect(lost.getByTestId('disk-health')).toContainText('Chunks unreachable: 1');
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

