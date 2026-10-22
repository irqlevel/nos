// SITE_URL is checked and inlined by next.config.ts, so it is always an origin here.
const url = process.env.SITE_URL ?? "http://localhost:3000";

export const site = {
    name: "nos",
    url,
    author: "irqlevel",
    since: 2017,
    github: "https://github.com/irqlevel/nos",
    releases: "https://github.com/irqlevel/nos/releases",
    // What a relative link in docs/ resolves to when it leaves docs/.
    repoBlob: "https://github.com/irqlevel/nos/blob/master/",
    repoRaw: "https://raw.githubusercontent.com/irqlevel/nos/master/",
    // The square logo search engines are given (app/logo.png): Google wants 112 px or more.
    logo: { path: "/logo.png", size: 512 },
    verification: {
        google: process.env.GOOGLE_SITE_VERIFICATION,
        yandex: process.env.YANDEX_VERIFICATION,
    },
    // The Yandex Metrica counter, when the build has one (YANDEX_METRIKA_ID, digits only).
    metrika: process.env.YANDEX_METRIKA_ID,
} as const;

export function absoluteUrl(path: string): string {
    return new URL(path, site.url).toString();
}
