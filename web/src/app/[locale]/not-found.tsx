import Link from "next/link";
import { locale as rootLocale } from "next/root-params";
import { getDictionary } from "@/lib/dictionaries";
import { defaultLocale, isLocale } from "@/lib/i18n";

export default async function NotFound() {
    const value = await rootLocale();
    const locale = isLocale(value) ? value : defaultLocale;
    const dict = getDictionary(locale);
    return (
        <div className="mx-auto max-w-3xl px-4 py-24 sm:px-6">
            <p className="font-mono text-sm text-accent">404</p>
            <h1 className="mt-3 text-3xl font-bold tracking-tight sm:text-4xl">{dict.notFound.title}</h1>
            <p className="mt-4 text-lg text-muted">{dict.notFound.text}</p>
            <div className="mt-8 flex flex-wrap gap-3 text-sm">
                <Link href={`/${locale}`} className="rounded-lg bg-fg px-4 py-2 font-medium text-bg hover:opacity-90">
                    {dict.notFound.home}
                </Link>
                <Link href={`/${locale}/docs`} className="rounded-lg border border-line px-4 py-2 font-medium hover:border-line-strong">
                    {dict.nav.docs}
                </Link>
                <Link href={`/${locale}/blog`} className="rounded-lg border border-line px-4 py-2 font-medium hover:border-line-strong">
                    {dict.nav.blog}
                </Link>
            </div>
        </div>
    );
}
