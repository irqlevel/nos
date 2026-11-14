// The cloud images a VM, or an image of NBS's, is made of in a click: the
// distributions' own, which nos fetches itself.

export const CLOUD_IMAGES = [
  { label: 'Debian 13 (trixie)', url: 'https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2' },
  { label: 'Ubuntu 24.04 LTS', url: 'https://cloud-images.ubuntu.com/releases/noble/release/ubuntu-24.04-server-cloudimg-amd64.img' },
];

/** A volume of NBS's as the nbs provider's `volumes` lists it. */
export interface NbsVolume {
  name: string; kind: 'volume' | 'image' | 'snapshot'; size: number; origin: string | null; writer: string | null;
  host: string; health?: string; chunks_at_two?: number; chunks_at_one?: number; chunks_unreachable?: number;
  chunks_copying?: number; chunks_unlike?: number;
}

/** A volume's name, as NBS takes one: an image's, a snapshot's, a clone's. */
export const VOLUME_NAME = /^[a-z0-9][a-z0-9._-]{0,62}$/;
