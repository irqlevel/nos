'use client';

import Link from 'next/link';
import { useEffect, useState } from 'react';

import { JobWatch } from './JobWatch';
import { call, startJob } from '@/lib/client';
import { bytes } from '@/lib/descriptor';
import type { Words } from '@/lib/i18n';
import { CLOUD_IMAGES as IMAGES, NbsVolume as Volume, VOLUME_NAME as VOLUME } from '@/lib/images';

const GIB = 1024 ** 3;
const NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,31}$/;
/** What `onto` names a volume of NBS's by. */
const NBS = 'nbs:';
/** The firewall a VM that names none is judged by (hv's `firewalls::DEFAULT`). */
const DEFAULT_FIREWALL = 'default';
/** What a network of the cluster's is called on a VM's `network` (hv's
 * `config::CLUSTER_PREFIX`): its port, address and firewall vnet's. */
const CLUSTER = 'vnet:';
/** An IPv4 address as a person writes it. */
const IPV4 = /^(25[0-5]|2[0-4]\d|1?\d?\d)(\.(25[0-5]|2[0-4]\d|1?\d?\d)){3}$/;

/** A device of the host's a VM's disk may be (the vm provider's `devices`):
 * the image poured onto it in place of a file. `refused` says why it is not
 * given as it is -- force gives it over what is on it, or another VM's; one
 * the machine boots from or something holds is never given. */
interface Device {
  name: string; id: string | null; size_bytes: number; holds: string | null; in_use_by: string | null;
  boots_the_machine: string | null; refused: string | null;
}

function usable(d: Device): boolean {
  return d.id !== null && d.in_use_by === null && d.boots_the_machine === null;
}

export function CreateVm({ host, locale, w, image: given }: { host: string; locale: string; w: Words; image?: string }) {
  const ru = locale === 'ru';
  const [name, setName] = useState('');
  /* An image or a snapshot named in the address -- its row's "Create a VM"
   * -- is the one picked */
  const [source, setSource] = useState<'url' | 'image'>(given ? 'image' : 'url');
  const [url, setUrl] = useState(IMAGES[0]!.url);
  const [image, setImage] = useState(given ?? '');
  const [clone, setClone] = useState('');
  const [disk, setDisk] = useState(given ? '' : '20');
  const [mem, setMem] = useState('1024');
  const [cpus, setCpus] = useState('1');
  const [password, setPassword] = useState('');
  const [devices, setDevices] = useState<Device[]>([]);
  /* Null where the host has no NBS, or is in no NBS cluster */
  const [volumes, setVolumes] = useState<Volume[] | null>(null);
  const [onto, setOnto] = useState('');
  const [force, setForce] = useState(false);
  /* The host's firewalls (the fw provider's `list`): null where it has
   * none to say, and the VM is judged by `default` */
  const [firewalls, setFirewalls] = useState<string[] | null>(null);
  const [firewall, setFirewall] = useState(DEFAULT_FIREWALL);
  /* The networks it may be on (the vm provider's `networks`): the host's in
   * use and the cluster's; '' alone. On one of the cluster's its firewall
   * is one of the cluster's (vnet's `firewalls`). */
  const [networks, setNetworks] = useState<string[]>([]);
  const [network, setNetwork] = useState('');
  const [clusterFirewalls, setClusterFirewalls] = useState<string[] | null>(null);
  const cluster = network.startsWith(CLUSTER);
  /* A public address of the world's routed to the host, mapped one to one */
  const [publicIp, setPublicIp] = useState('');
  const [error, setError] = useState('');
  const [job, setJob] = useState<number | null>(null);

  useEffect(() => {
    let live = true;
    call(host, 'vm', 'devices').then((r) => {
      if (live && r.ok) setDevices(((r.data as { devices?: Device[] }).devices ?? []).filter(usable));
    });
    call(host, 'nbs', 'volumes').then((r) => {
      if (live && r.ok) setVolumes((r.data as { volumes?: Volume[] }).volumes ?? []);
    });
    call(host, 'fw', 'list').then((r) => {
      if (!live || !r.ok) return;
      const names = ((r.data as { firewalls?: Array<{ name?: unknown }> }).firewalls ?? [])
        .map((f) => f.name).filter((n): n is string => typeof n === 'string');
      setFirewalls(names.includes(DEFAULT_FIREWALL) ? names : [DEFAULT_FIREWALL, ...names]);
    });
    call(host, 'vm', 'networks').then((r) => {
      if (!live || !r.ok) return;
      setNetworks(((r.data as { networks?: Array<{ name?: unknown }> }).networks ?? [])
        .map((n) => n.name).filter((n): n is string => typeof n === 'string'));
    });
    call(host, 'vnet', 'firewalls').then((r) => {
      if (!live || !r.ok) return;
      setClusterFirewalls(((r.data as { firewalls?: Array<{ name?: unknown }> }).firewalls ?? [])
        .map((f) => f.name).filter((n): n is string => typeof n === 'string'));
    });
    return () => { live = false; };
  }, [host]);
  const images = (volumes ?? []).filter((v) => v.kind !== 'volume');
  const targets = (volumes ?? []).filter((v) => v.kind === 'volume');
  const device = devices.find((d) => d.id === onto);
  const volume = onto.startsWith(NBS) ? targets.find((v) => NBS + v.name === onto) : undefined;
  const picked = images.find((v) => v.name === image);
  /* A file's size is asked; a device's and a volume's disk is theirs; an
   * image's clone is the image's size unless asked bigger. */
  const sized = source === 'image' || onto === '';

  function pickSource(s: 'url' | 'image') {
    setSource(s);
    setOnto('');
    setForce(false);
    setDisk(s === 'url' ? '20' : '');
    if (s === 'image' && !image && images.length) setImage(images[0]!.name);
  }

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!NAME.test(name)) {
      setError(ru ? 'Имя: буква или цифра, затем буквы, цифры, . _ и -, до 32.' :
        'A name: a letter or a digit, then letters, digits, . _ and -, at most 32.');
      return;
    }
    const params: Record<string, unknown> = { name, memory_mib: Number(mem), cpus: Number(cpus) };
    if (source === 'image') {
      if (!picked) {
        setError(ru ? 'Выберите образ NBS.' : 'Pick an image of NBS\'s.');
        return;
      }
      const cloneName = clone || name;
      if (!VOLUME.test(cloneName)) {
        setError(ru ? 'Имя клона: строчные буквы, цифры, . _ и -, до 63.' :
          'The clone\'s name: lowercase letters, digits, . _ and -, at most 63.');
        return;
      }
      params.image = picked.name;
      if (cloneName !== name) params.onto = NBS + cloneName;
      if (disk) params.size_bytes = Math.round(Number(disk) * GIB);
    } else {
      params.url = url;
      if (onto) {
        params.onto = onto;
        if (force) params.force = true;
      } else if (disk) {
        params.size_bytes = Math.round(Number(disk) * GIB);
      }
    }
    if (password) params.password = password;
    if (network) params.network = network;
    if (cluster ? clusterFirewalls !== null : firewalls !== null) params.firewall = firewall;
    if (publicIp && !cluster) {
      if (!IPV4.test(publicIp)) {
        setError(ru ? 'Публичный адрес: a.b.c.d.' : 'A public address: a.b.c.d.');
        return;
      }
      params.public = publicIp;
    }
    setError('');
    const r = await startJob(host, 'vm', 'create', params);
    if (r.ok) setJob(r.id);
    else setError(r.error.message);
  }

  const input = 'mt-1 w-full rounded-lg border border-line bg-bg px-3 py-2 text-fg';
  return (
    <div className="max-w-xl space-y-4">
      <h1 className="text-2xl font-semibold text-fg">{w.createVm}</h1>
      <form onSubmit={submit} className="space-y-3 rounded-xl border border-line bg-card p-4">
        <label className="block text-sm text-fg-soft">{w.name}
          <input value={name} onChange={(e) => setName(e.target.value)} name="name" className={input} />
        </label>
        {volumes !== null && (
          <fieldset className="flex gap-4 text-sm text-fg-soft" data-testid="source">
            <legend className="sr-only">{ru ? 'Из чего диск' : 'Its disk made of'}</legend>
            <label className="flex items-center gap-2">
              <input type="radio" name="source" value="url" checked={source === 'url'} onChange={() => pickSource('url')} />
              {ru ? 'Образ по URL' : 'An image by its URL'}
            </label>
            <label className="flex items-center gap-2">
              <input type="radio" name="source" value="image" checked={source === 'image'} onChange={() => pickSource('image')} />
              {ru ? 'Образ NBS' : 'An image of NBS\'s'}
            </label>
          </fieldset>
        )}
        {source === 'url' ? (
          <>
            <label className="block text-sm text-fg-soft">{w.imageUrl}
              <select value={IMAGES.some((i) => i.url === url) ? url : ''} onChange={(e) => e.target.value && setUrl(e.target.value)}
                className={input}>
                {IMAGES.map((i) => <option key={i.url} value={i.url}>{i.label}</option>)}
                <option value="">…</option>
              </select>
              <input value={url} onChange={(e) => setUrl(e.target.value)} name="url" className={input + ' font-mono text-xs'} />
            </label>
            <label className="block text-sm text-fg-soft">{ru ? 'Где диск' : 'Its disk'}
              <select value={onto} onChange={(e) => { setOnto(e.target.value); setForce(false); }} name="onto" className={input}>
                <option value="">{ru ? 'Файл в папке машины' : 'A file in its folder'}</option>
                {devices.length > 0 && (
                  <optgroup label={ru ? 'Устройство хоста' : 'A device of the host\'s'}>
                    {devices.map((d) => (
                      <option key={d.id!} value={d.id!}>
                        {d.name} · {(d.size_bytes / GIB).toFixed(1)} GiB{d.holds ? ' · ' + d.holds : ''}
                      </option>
                    ))}
                  </optgroup>
                )}
                {targets.length > 0 && (
                  <optgroup label={ru ? 'Том NBS' : 'A volume of NBS\'s'}>
                    {targets.map((v) => (
                      <option key={v.name} value={NBS + v.name}>
                        {NBS + v.name} · {bytes(v.size)}{v.writer ? ' · ' + (ru ? 'подключён на ' : 'attached on ') + v.writer : ''}
                      </option>
                    ))}
                  </optgroup>
                )}
              </select>
            </label>
            {device?.refused && (
              <div className="space-y-1 rounded-lg border border-line p-2 text-sm" data-testid="onto-refused">
                <p className="text-bad">{device.refused}</p>
                <label className="flex items-center gap-2 text-fg-soft">
                  <input type="checkbox" checked={force} onChange={(e) => setForce(e.target.checked)} name="force" />
                  {ru ? 'Записать образ поверх данных' : 'Pour the image over what is on it'}
                </label>
              </div>
            )}
            {volume && (
              <div className="space-y-1 rounded-lg border border-line p-2 text-sm" data-testid="onto-volume">
                <p className="text-fg-soft">
                  {ru ? 'Образ запишется поверх всего, что есть на томе; диск образа должен в нём поместиться.' :
                    'The image is poured over whatever the volume holds; its disk must fit in the volume.'}
                </p>
                {volume.writer && (
                  <label className="flex items-center gap-2 text-fg-soft">
                    <input type="checkbox" checked={force} onChange={(e) => setForce(e.target.checked)} name="force" />
                    {ru ? `Забрать том у ${volume.writer}` : `Take the volume from ${volume.writer}`}
                  </label>
                )}
              </div>
            )}
          </>
        ) : (
          <>
            {volumes === null ? null : images.length === 0 ? (
              <p className="text-sm text-muted" data-testid="no-images">
                {ru ? 'В NBS нет ни образов, ни снимков: образ делается на странице «Образы», снимок -- на странице машины.' :
                  'NBS holds no image or snapshot yet: the Images page makes an image, a VM\'s page a snapshot.'}
              </p>
            ) : (
              <label className="block text-sm text-fg-soft">{ru ? 'Образ NBS' : 'An image of NBS\'s'}
                <select value={image} onChange={(e) => setImage(e.target.value)} name="image" className={input}>
                  {images.map((v) => (
                    <option key={v.name} value={v.name}>
                      {v.name} · {v.kind === 'image' ? (ru ? 'образ' : 'image') : (ru ? 'снимок' : 'snapshot')} · {bytes(v.size)}
                    </option>
                  ))}
                </select>
              </label>
            )}
            <label className="block text-sm text-fg-soft">{ru ? 'Том-клон для диска (необязательно)' : 'Its disk\'s clone (optional)'}
              <input value={clone} onChange={(e) => setClone(e.target.value)} name="clone" placeholder={name}
                className={input + ' font-mono text-xs'} />
            </label>
          </>
        )}
        <div className="grid grid-cols-3 gap-3">
          <label className="block text-sm text-fg-soft">{ru ? 'Диск, ГиБ' : 'Disk, GiB'}
            <input value={sized ? disk : ''} onChange={(e) => setDisk(e.target.value)} inputMode="numeric" name="disk"
              disabled={!sized}
              placeholder={!sized ? (volume ? (ru ? 'размер тома' : "the volume's") : (ru ? 'размер устройства' : "the device's"))
                : source === 'image' && picked ? (picked.size / GIB).toFixed(1) : ''}
              className={input} />
          </label>
          <label className="block text-sm text-fg-soft">{ru ? 'Память, МиБ' : 'Memory, MiB'}
            <input value={mem} onChange={(e) => setMem(e.target.value)} inputMode="numeric" name="memory" className={input} />
          </label>
          <label className="block text-sm text-fg-soft">{w.cpu}
            <input value={cpus} onChange={(e) => setCpus(e.target.value)} inputMode="numeric" name="cpus" className={input} />
          </label>
        </div>
        <label className="block text-sm text-fg-soft">
          {ru ? 'Пароль пользователя образа (необязательно)' : "Password of the image's user (optional)"}
          <span className="block text-xs text-muted">
            {ru ? 'cloud-init задаёт его пользователю образа по умолчанию: ubuntu, debian, alpine…; root остаётся закрыт.'
              : "cloud-init gives it to the image's default user: ubuntu, debian, alpine…; root stays locked."}
          </span>
          <input value={password} onChange={(e) => setPassword(e.target.value)} type="password" name="password" className={input} />
        </label>
        {networks.length > 0 && (
          <label className="block text-sm text-fg-soft">{ru ? 'Сеть' : 'Network'}
            <span className="block text-xs text-muted">
              {ru ? 'Сеть хоста, общая для её машин, или сеть кластера (vnet:): порт, адрес и фаервол машины тогда держит vnet.'
                : "A host's network its VMs share, or one of the cluster's (vnet:): its port, address and firewall vnet's."}
            </span>
            <select value={network} name="network" className={input} onChange={(e) => {
              setNetwork(e.target.value);
              setFirewall(DEFAULT_FIREWALL);
            }}>
              <option value="">{ru ? '— одна, без сети' : '— alone, on no network'}</option>
              {networks.map((n) => <option key={n} value={n}>{n}</option>)}
            </select>
          </label>
        )}
        {(cluster ? clusterFirewalls : firewalls) !== null && (
          <label className="block text-sm text-fg-soft">{ru ? 'Фаервол' : 'Firewall'}
            <span className="block text-xs text-muted">
              {ru ? 'Чем судится её сеть. default: входящее закрыто, исходящее открыто; порт открывает правило.'
                : "What its network is judged by. default: what comes in denied, what goes out allowed; a rule opens a port."}
              {cluster && (ru ? ' Фаервол кластера.' : " One of the cluster's.")}
            </span>
            <select value={firewall} onChange={(e) => setFirewall(e.target.value)} name="firewall" className={input}>
              {((cluster ? clusterFirewalls : firewalls) ?? []).map((n) => <option key={n} value={n}>{n}</option>)}
            </select>
          </label>
        )}
        {!cluster && <label className="block text-sm text-fg-soft">{ru ? 'Публичный адрес (необязательно)' : 'Public address (optional)'}
          <span className="block text-xs text-muted">
            {ru ? 'Адрес, направленный на этот хост (арендованный): машина отвечает по нему один к одному, на портах, которые открывает её фаервол.'
              : 'An address routed to this host -- a rented one: the VM is reached at it one to one, at the ports its firewall opens.'}
          </span>
          <input value={publicIp} onChange={(e) => setPublicIp(e.target.value.trim())} name="public" inputMode="decimal"
            placeholder="203.0.113.9" className={input + ' font-mono text-xs'} />
        </label>}
        {error && <p role="alert" className="text-sm text-bad">{error}</p>}
        <button type="submit" disabled={job !== null || (source === 'image' && !picked)}
          className="rounded-lg bg-accent px-3 py-2 text-sm font-medium text-bg hover:bg-accent-strong disabled:opacity-60">
          {w.create}
        </button>
      </form>
      {job !== null && (
        <>
          <JobWatch host={host} id={job} w={w} />
          <Link href={`/${locale}/h/${host}/vms/${name}`} className="text-sm text-accent hover:underline">{name} →</Link>
        </>
      )}
    </div>
  );
}
