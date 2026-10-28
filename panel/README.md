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
own descriptors read from the tree and numbers that move. To run the panel
against it:

```
cd panel && npm install
node mock/server.mjs --port 39299 --state /tmp/nos-mock &
node mock/seed.mjs /tmp/nos-mock 39299 'a password' > /tmp/nos-seed.json
PANEL_SEED=/tmp/nos-seed.json PANEL_DATA=/tmp/nos-panel npx next dev -p 3100
```

and log in at http://localhost:3100 as `root`. `nosctl` works against the
mock too: its keys are in the state folder, and it prints the host key.

## Tests

`npx playwright test` builds nothing itself: `npm run build` first. It
starts the mock and the panel (`playwright.config.ts`) and drives a browser
through the login, the dashboard, the providers' pages in both languages, a
VM made, cloned and deleted as jobs, a protected one's delete refused, a
console typed at, the theme and the language. `npm run typecheck` is the
translations' gate: the Russian dictionary is the English one's shape.
CI runs all of it (`.github/workflows/panel.yml`).
