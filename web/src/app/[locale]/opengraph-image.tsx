import { getDictionary } from "@/lib/dictionaries";
import { isLocale, locales } from "@/lib/i18n";
import { ogImage, ogSize } from "@/lib/og";

export const size = ogSize;
export const contentType = "image/png";
export const dynamicParams = false;
export const alt = "nos";

// Drawn at build time, one per locale: the server never reads a font or a file for it.
export function generateStaticParams() {
    return locales.map((locale) => ({ locale }));
}

export default async function Image({ params }: { params: Promise<{ locale: string }> }) {
    const { locale } = await params;
    const dict = getDictionary(isLocale(locale) ? locale : "en");
    return ogImage({ eyebrow: dict.home.eyebrow, title: dict.home.title, footer: dict.footer.tagline });
}
