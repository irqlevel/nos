import type { NextConfig } from 'next';

const config: NextConfig = {
  output: 'standalone',
  /* panel/ is its own project: trace and bundle from here */
  outputFileTracingRoot: process.cwd(),
  turbopack: { root: process.cwd() },
  poweredByHeader: false,
  reactStrictMode: true,
  images: { unoptimized: true },
  /* node:sqlite and the TLS client are the server's own, never bundled */
  serverExternalPackages: [],
};

export default config;
