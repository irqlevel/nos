import { NextResponse, type NextRequest } from "next/server";
import { defaultLocale, isLocale, localeCookie, negotiateLocale } from "@/lib/i18n";

/*
 * Every page lives under /en or /ru. A path without a locale -- "/" above all,
 * or a link someone shared without one -- is redirected to the visitor's
 * language: the one they picked with the switcher (a cookie), else the best
 * match for their browser's Accept-Language, else English. The redirect is
 * temporary and varies on both, so no cache keeps one visitor's answer for
 * another, and crawlers index the prefixed pages, which never redirect.
 */
export function proxy(request: NextRequest) {
    const { pathname } = request.nextUrl;
    if (isLocale(pathname.split("/")[1])) {
        return NextResponse.next();
    }
    const chosen = request.cookies.get(localeCookie)?.value;
    const locale = isLocale(chosen) ? chosen : (negotiateLocale(request.headers.get("accept-language")) ?? defaultLocale);
    const url = request.nextUrl.clone();
    url.pathname = `/${locale}${pathname === "/" ? "" : pathname}`;
    const response = NextResponse.redirect(url, 307);
    response.headers.set("Vary", "Accept-Language, Cookie");
    response.headers.set("Cache-Control", "private, no-cache");
    return response;
}

export const config = {
    // Not the framework's files, the apple-icon route, or files (robots.txt, sitemap.xml, icon.svg).
    matcher: ["/((?!_next/|apple-icon|.*\\.[a-zA-Z0-9]+$).*)"],
};
