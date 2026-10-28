# The nos panel

The management panel of a nos host -- or of several: a Next.js application
that runs in a VM of its own on the host (the appliance), and manages the
host through its API (docs/api.md) as any client does, TLS 1.3 with its own
key, the host's key pinned. Everything a browser touches -- the login, the
sessions, the pages -- is here, in Linux; nos keeps its API alone.

- `src/lib/nosapi.ts` -- a host's API: the pinned TLS connection (the
  host's key checked before a byte of HTTP is sent), HTTP/1.1 kept alive, a
  few connections a host, and the stream (a WebSocket) on one of its own.
- `src/lib/hub.ts` -- one stream a host, kept open: its metrics, log and
  jobs handed to every page watching (server-sent events), and the numbers
  kept as history in SQLite -- an hour a second at a time, thirty days a
  minute at a time.
- `src/lib/auth.ts` -- `root` and the password nos-install made (scrypt),
  sessions in a `__Host-` cookie (Secure, HttpOnly, SameSite=Strict), a
  change asked from the panel's own origin alone, logins counted per address.
- `src/lib/hosts.ts` -- the hosts, and the seed: what `api panel create`
  hands the appliance (the host's API and key, the panel's identity there,
  root's password's hash), read once on the first start.
- `src/proxy.ts` -- a Content Security Policy with a nonce of its own on
  every page, nothing framed; a page with no session sent to the login.
- Pages: the dashboard; every provider's page made of its descriptor alone
  (`ProviderView`: its resources as tables, their rows' actions, every
  other method a form) -- a provider the panel has never heard of gets its
  pages the same way; the VMs' own (a list, a create from a cloud image, a
  VM's overview, its console in xterm.js, its vm.toml, disks and forwards);
  the jobs; the hosts; the settings. English and Russian.

## On the mock

`mock/server.mjs` is the host API as the panel sees it, with the providers'
own descriptors read from the tree and numbers that move, and what api.ko
lets in at once with no api.toml -- two connections in their handshake, four
of one key -- one past them reset as nos resets it. To run the panel against
it:

```
cd panel && npm install
node mock/server.mjs --port 39299 --state /tmp/nos-mock &
node mock/seed.mjs /tmp/nos-mock 39299 'a password' > /tmp/nos-seed.json
PANEL_SEED=/tmp/nos-seed.json PANEL_DATA=/tmp/nos-panel npx next dev -p 3100
```

and log in at http://localhost:3100 as `root`. `nosctl` works against the
mock too: its keys are in the state folder, and it prints the host key.

A second host, for the Hosts page: another state folder is another host,
`--name` names it, and `--trust` lets the panel's key in -- the first
mock's admin's, which the seed made of it -- as `api client add` would on a
second nos:

```
node mock/server.mjs --port 39300 --state /tmp/nos-mock-b --name nos-b --trust /tmp/nos-mock/admin.crt &
```

then add it on the Hosts page by `127.0.0.1:39300` and the key it prints.

## More hosts

The panel manages every host added on its Hosts page by its API's address
and key, each its own dashboard, pages and history; the panel is one client
of all of them, by the key its seed gave it. On a host to add:

```
api client add panel-3fe5cf7a admin 3fe5cf7a...   # what the Hosts page prints: the panel's key
edit /etc/api/api.toml                            # listen = ["hv0", "eth0"], allow_eth0 = [<the panel's host's address>]
api stop; api start
api status                                        # the host's key, which the panel pins
```

The panel's VM reaches another host from its own host's address, through
hv's NAT -- the address to let in on eth0. `panel` is that host's own
panel's name, where it has one; the panel's is `panel-` and its key's first
digits.

## Tests

`npx playwright test` builds nothing itself: `npm run build` first. It
starts the mock and the panel (`playwright.config.ts`) and drives a browser
through the login, the dashboard, the providers' pages in both languages, a
VM made, cloned and deleted as jobs -- its name typed again before it goes
-- a protected one's delete refused, a console typed at, a second host
added and reached by the panel's key while one showing another key is not,
the theme and the language -- and that through all of it, and a dozen
requests at once, the panel never went past what a host lets in at once
(the mock's `busy`). `npm run typecheck` is the
translations' gate: the Russian dictionary is the English one's shape.
CI runs all of it (`.github/workflows/panel.yml`).
