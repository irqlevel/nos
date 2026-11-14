import { notFound } from 'next/navigation';

import { Images } from '@/components/Images';
import { descriptor } from '@/lib/catalog';
import { isLocale, words } from '@/lib/i18n';

/* A host's images and snapshots of NBS's: beside its VMs, not under /vms,
 * where a VM of the name would have lost its page */
export default async function ImagesPage({ params }: { params: Promise<{ locale: string; host: string }> }) {
  const { locale, host } = await params;
  if (!isLocale(locale)) notFound();
  const vm = await descriptor(host, 'vm');
  if (!vm?.methods.image) notFound();
  const nbs = await descriptor(host, 'nbs');
  return <Images host={host} vm={vm} nbs={nbs} locale={locale} w={words(locale)} />;
}
