// Before every page: a Content Security Policy with a nonce of its own --
// scripts the panel's alone, nothing framed, nothing posted elsewhere --
// and a page asked for with no session cookie sent to the login. The
// session itself is checked where it is used (`route.ts`, the pages'
// layout): this is the optimistic part.

import { NextRequest, NextResponse } from 'next/server';

const COOKIE = '__Host-nos-panel';
const LOCALES = ['en', 'ru'];

function policy(nonce: string, dev: boolean): string {
  return [
    "default-src 'self'",
    `script-src 'self' 'nonce-${nonce}' 'strict-dynamic'${dev ? " 'unsafe-eval'" : ''}`,
    /* xterm.js and uPlot set styles inline */
    "style-src 'self' 'unsafe-inline'",
    "img-src 'self' data: blob:",
    "font-src 'self'",
    "connect-src 'self'",
    "object-src 'none'",
    "base-uri 'none'",
    "form-action 'self'",
    "frame-ancestors 'none'",
  ].join('; ');
}

export function proxy(request: NextRequest) {
  const { pathname } = request.nextUrl;
  const locale = LOCALES.find((l) => pathname === '/' + l || pathname.startsWith('/' + l + '/'));
  if (!locale) {
    const want = (request.headers.get('accept-language') || '').toLowerCase().startsWith('ru') ? 'ru' : 'en';
    return NextResponse.redirect(new URL('/' + want + (pathname === '/' ? '' : pathname), request.url));
  }
  const login = pathname === `/${locale}/login`;
  if (!login && !request.cookies.get(COOKIE)) {
    return NextResponse.redirect(new URL(`/${locale}/login`, request.url));
  }
  const nonce = Buffer.from(crypto.randomUUID()).toString('base64');
  const csp = policy(nonce, process.env.NODE_ENV === 'development');
  const headers = new Headers(request.headers);
  headers.set('x-nonce', nonce);
  headers.set('Content-Security-Policy', csp);
  const res = NextResponse.next({ request: { headers } });
  res.headers.set('Content-Security-Policy', csp);
  res.headers.set('X-Frame-Options', 'DENY');
  res.headers.set('X-Content-Type-Options', 'nosniff');
  res.headers.set('Referrer-Policy', 'same-origin');
  return res;
}

export const config = {
  /* Pages alone: not the panel's API, not Next's own files */
  matcher: ['/((?!api/|_next/static|_next/image|favicon.ico|icon.svg).*)'],
};
