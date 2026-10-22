import { notFound } from "next/navigation";
import { isLocale, type Locale } from "./i18n";

// The locale of a page's params; the layout has already 404ed anything else.
export async function pageLocale(params: Promise<{ locale: string }>): Promise<Locale> {
    const { locale } = await params;
    if (!isLocale(locale)) {
        notFound();
    }
    return locale;
}
