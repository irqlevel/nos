# The nos website, built into Next.js's standalone server.
#
# docker-compose.yml builds it with web/ as the context and the repository's
# docs/ as a second, named context ("docs"): the site's documentation pages
# are rendered from that directory at build time, so each build carries the
# docs of the checkout it was built from. scripts/release.sh builds it through
# compose, tagged with the release's version and for the server's platform.
# Without either:
#
#   docker build --build-context docs=../docs --build-arg SITE_URL=https://nos.example.org -t nos-web .
#
# The dependencies and the build run on the builder's own platform -- natively,
# so an x86-64 image builds at full speed on an arm64 Mac -- and only the last
# stage is the target's. That holds because all the last stage takes from them
# is JavaScript: sharp, the one native package the standalone server carries,
# is removed (the site turns Next's image optimizer, its only user, off), and
# the build fails if anything else native turns up there.
#
# Node is pinned to a minor version: a rebuild picks up its patch releases.

ARG NODE_IMAGE=node:24.22-alpine

FROM --platform=$BUILDPLATFORM ${NODE_IMAGE} AS deps
WORKDIR /src/web
COPY package.json package-lock.json ./
RUN npm ci --no-audit --no-fund

FROM --platform=$BUILDPLATFORM ${NODE_IMAGE} AS build
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
ARG BUILDPLATFORM
RUN mkdir -p public && \
    SITE_URL="$SITE_URL" \
    GOOGLE_SITE_VERIFICATION="$GOOGLE_SITE_VERIFICATION" \
    YANDEX_VERIFICATION="$YANDEX_VERIFICATION" \
    npm run build && \
    rm -rf .next/standalone/node_modules/sharp \
           .next/standalone/node_modules/@img \
           .next/standalone/node_modules/@emnapi && \
    if find .next/standalone -name '*.node' | grep .; then \
        echo "native code in the standalone output: it is $BUILDPLATFORM's, not the image's" >&2; \
        exit 1; \
    fi

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
# Checked every second while it starts (an engine older than 25 ignores
# --start-interval and waits the 10 s), so a release sees it healthy at once.
HEALTHCHECK --interval=10s --timeout=3s --start-period=30s --start-interval=1s --retries=3 \
    CMD wget -q -O /dev/null http://127.0.0.1:3000/robots.txt || exit 1
# What the image is: `docker image inspect` on the server tells the release apart.
ARG VERSION=dev
ARG REVISION=""
LABEL org.opencontainers.image.title="nos-web" \
      org.opencontainers.image.description="The nos website" \
      org.opencontainers.image.source="https://github.com/irqlevel/nos" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.revision="$REVISION"
CMD ["node", "server.js"]
