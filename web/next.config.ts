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

/*
 * Inline scripts stay allowed: the App Router streams its payload through them,
 * and a nonce would make every page dynamic. Everything else is this origin's
 * own -- fonts are bundled, nothing is loaded from a third party.
 */
const contentSecurityPolicy = [
    "default-src 'self'",
    "script-src 'self' 'unsafe-inline'",
    "style-src 'self' 'unsafe-inline'",
    "img-src 'self' data: https://raw.githubusercontent.com",
    "font-src 'self'",
    "connect-src 'self'",
    "object-src 'none'",
    "base-uri 'self'",
    "form-action 'self'",
    "frame-ancestors 'none'",
].join("; ");

export default function config(phase: string): NextConfig {
    const env: Record<string, string> = { SITE_URL: siteUrl(phase) };
    for (const key of ["GOOGLE_SITE_VERIFICATION", "YANDEX_VERIFICATION"]) {
        const value = process.env[key];
        if (value) {
            env[key] = value;
        }
    }

    const headers = [
        { key: "X-Content-Type-Options", value: "nosniff" },
        { key: "Referrer-Policy", value: "strict-origin-when-cross-origin" },
        { key: "X-Frame-Options", value: "DENY" },
        { key: "Permissions-Policy", value: "camera=(), microphone=(), geolocation=(), browsing-topics=()" },
    ];
    // The dev server needs eval and a websocket for hot reload.
    if (phase !== PHASE_DEVELOPMENT_SERVER) {
        headers.push({ key: "Content-Security-Policy", value: contentSecurityPolicy });
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
