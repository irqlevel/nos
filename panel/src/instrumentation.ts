// When the panel's server starts: its seed read into the store -- the first
// time, the host it was made on and root's password -- and a stream opened
// to every host it manages (`hub.ts`).

export async function register() {
  if (process.env.NEXT_RUNTIME !== 'nodejs') {
    return;
  }
  const { readSeed } = await import('./lib/hosts');
  const { syncHubs } = await import('./lib/hub');
  try {
    console.log('panel: ' + (await readSeed()));
    syncHubs();
  } catch (e) {
    console.error('panel: the seed could not be read --', e);
  }
}
