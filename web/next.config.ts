import type { NextConfig } from "next";
import { PHASE_DEVELOPMENT_SERVER, PHASE_PRODUCTION_BUILD } from "next/constants";

/*
 * SITE_URL is baked into every page at build time: the canonical links, the
 * hreflang alternates, the sitemap, robots.txt, the feeds and the Open Graph
 * tags all carry it. A production build without it would ship a sitemap full
 * of localhost, so it fails here instead -- loudly, before anything is built.
 */
function siteUrl(phase: string): string {
    const raw = process.env.SITE_URL;
    if (!raw) {
        if (phase === PHASE_PRODUCTION_BUILD) {
            throw new Error(
                "SITE_URL is not set. The sitemap, canonical URLs and feeds are built " +
                    "with it: SITE_URL=https://nos.example.org npm run build " +
                    "(docker compose passes https://$DOMAIN)."
            );
        }
        return "http://localhost:3000";
    }
    let url: URL;
    try {
        url = new URL(raw);
    } catch {
        throw new Error(`SITE_URL is not a URL: ${raw}`);
    }
    if (url.protocol !== "https:" && url.protocol !== "http:") {
        throw new Error(`SITE_URL must be http(s): ${raw}`);
    }
    if (url.pathname !== "/" || url.search || url.hash) {
        throw new Error(`SITE_URL must be an origin with no path: ${raw}`);
    }
    return url.origin;
}

// A Yandex Metrica counter's number, or nothing: a build without one carries no counter at all.
function metrikaId(): string | undefined {
    const raw = process.env.YANDEX_METRIKA_ID;
    if (!raw) {
        return undefined;
    }
    if (!/^[0-9]{1,12}$/.test(raw)) {
        throw new Error(`YANDEX_METRIKA_ID must be the counter's number: ${raw}`);
    }
    return raw;
}

/*
 * Where Yandex Metrica reaches, from its CSP guide: the tag and its hits on
 * the mc.* hosts and yastatic.net, over HTTPS and WebSockets; and, for Session
 * Replay (Webvisor), the Metrica sites that show this site in a frame.
 */
const metrikaHosts = ["mc.yandex.ru", "mc.yandex.com", "mc.yandex.by", "mc.yandex.kz", "mc.yandex.com.tr", "mc.yandex.uz", "mc.webvisor.com", "mc.webvisor.org"];
const metrikaFrameAncestors = [
    "metrika.yandex.ru", "metrika.yandex.by", "metrika.yandex.kz", "metrika.yandex.uz", "metrika.yandex",
    "metrica.yandex.com", "metrica.yandex.com.tr", "metrica.yandex.ru", "metrica.yandex.by", "metrica.yandex.kz", "metrica.yandex",
    "metrica.ya.ru", "metrika.ya.ru",
    "analytics.yandex.ru", "analytics.yandex.com", "analytics.yandex.by", "analytics.yandex.kz", "analytics.yandex.com.tr",
    "metr.yandex.ru", "metr.yandex.com", "metr.yandex.by", "metr.yandex.kz", "metr.yandex.com.tr",
];

/*
 * Inline scripts stay allowed: the App Router streams its payload through them,
 * and a nonce would make every page dynamic. Everything else is this origin's
 * own -- fonts are bundled -- except Yandex Metrica's hosts when the build has
 * a counter, and then the Metrica sites may frame the pages for Session Replay.
 */
function contentSecurityPolicy(metrika: boolean): string {
    const https = metrika ? metrikaHosts.map((h) => `https://${h}`) : [];
    const wss = metrika ? metrikaHosts.map((h) => `wss://${h}`) : [];
    const ancestors = metrika ? ["'self'", ...metrikaFrameAncestors.map((h) => `https://${h}`)] : ["'none'"];
    return [
        "default-src 'self'",
        ["script-src 'self' 'unsafe-inline'", ...https, ...(metrika ? ["https://yastatic.net"] : [])].join(" "),
        "style-src 'self' 'unsafe-inline'",
        ["img-src 'self' data: https://raw.githubusercontent.com", ...https].join(" "),
        "font-src 'self'",
        ["connect-src 'self'", ...https, ...wss].join(" "),
        ["frame-src 'self'", ...(metrika ? ["blob:", ...https] : [])].join(" "),
        ["child-src 'self'", ...(metrika ? ["blob:", ...https] : [])].join(" "),
        "object-src 'none'",
        "base-uri 'self'",
        "form-action 'self'",
        `frame-ancestors ${ancestors.join(" ")}`,
    ].join("; ");
}

export default function config(phase: string): NextConfig {
    const env: Record<string, string> = { SITE_URL: siteUrl(phase) };
    for (const key of ["GOOGLE_SITE_VERIFICATION", "YANDEX_VERIFICATION"]) {
        const value = process.env[key];
        if (value) {
            env[key] = value;
        }
    }
    const metrika = metrikaId();
    if (metrika) {
        env.YANDEX_METRIKA_ID = metrika;
    }

    const headers = [
        { key: "X-Content-Type-Options", value: "nosniff" },
        { key: "Referrer-Policy", value: "strict-origin-when-cross-origin" },
        { key: "Permissions-Policy", value: "camera=(), microphone=(), geolocation=(), browsing-topics=()" },
    ];
    // Session Replay shows the pages in a frame on Metrica's sites; frame-ancestors says which.
    if (!metrika) {
        headers.push({ key: "X-Frame-Options", value: "DENY" });
    }
    // The dev server needs eval and a websocket for hot reload.
    if (phase !== PHASE_DEVELOPMENT_SERVER) {
        headers.push({ key: "Content-Security-Policy", value: contentSecurityPolicy(metrika !== undefined) });
    }

    return {
        output: "standalone",
        // web/ is its own project: trace and bundle from here, not from a lockfile further up.
        outputFileTracingRoot: process.cwd(),
        turbopack: { root: process.cwd() },
        poweredByHeader: false,
        reactStrictMode: true,
        /*
         * Nothing here goes through next/image, and its optimizer is the one
         * user of sharp, the only native code the standalone server would
         * carry -- the Dockerfile drops it, so the image's last stage holds
         * JavaScript alone and can be another platform's than the build's.
         */
        images: { unoptimized: true },
        env,
        experimental: {
            globalNotFound: true,
        },
        async headers() {
            return [{ source: "/:path*", headers }];
        },
    };
}
