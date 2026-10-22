# The nos website, built into Next.js's standalone server.
#
# docker-compose.yml builds it with web/ as the context and the repository's
# docs/ as a second, named context ("docs"): the site's documentation pages
# are rendered from that directory at build time, so each build carries the
# docs of the checkout it was built from. Without compose:
#
#   docker build --build-context docs=../docs --build-arg SITE_URL=https://nos.example.org -t nos-web .
#
# Node is pinned to a minor version: a rebuild picks up its patch releases.

ARG NODE_IMAGE=node:24.22-alpine

FROM ${NODE_IMAGE} AS deps
WORKDIR /src/web
COPY package.json package-lock.json ./
RUN npm ci --no-audit --no-fund

FROM ${NODE_IMAGE} AS build
WORKDIR /src/web
ENV NEXT_TELEMETRY_DISABLED=1
COPY --from=deps /src/web/node_modules ./node_modules
COPY . .
# The layout of the repository, so "../docs" is what it is in a checkout.
COPY --from=docs . /src/docs
# Baked into every page: canonical URLs, the sitemap, the feeds. next.config.ts
# refuses to build without SITE_URL rather than ship a sitemap of localhost.
ARG SITE_URL
ARG GOOGLE_SITE_VERIFICATION=""
ARG YANDEX_VERIFICATION=""
RUN mkdir -p public && \
    SITE_URL="$SITE_URL" \
    GOOGLE_SITE_VERIFICATION="$GOOGLE_SITE_VERIFICATION" \
    YANDEX_VERIFICATION="$YANDEX_VERIFICATION" \
    npm run build

FROM ${NODE_IMAGE} AS run
WORKDIR /app
ENV NODE_ENV=production \
    NEXT_TELEMETRY_DISABLED=1 \
    PORT=3000 \
    HOSTNAME=0.0.0.0
COPY --from=build --chown=node:node /src/web/.next/standalone ./
COPY --from=build --chown=node:node /src/web/.next/static ./.next/static
COPY --from=build --chown=node:node /src/web/public ./public
USER node
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD wget -q -O /dev/null http://127.0.0.1:3000/robots.txt || exit 1
CMD ["node", "server.js"]
