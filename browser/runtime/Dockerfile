# Owned optional image; never added to Dockerfile.base or published by the launcher.
# Multi-architecture index includes native linux/arm64; see PROVENANCE.md.
FROM node:24.20.0-bookworm-slim@sha256:ba849c60be29959425b8734d57b8b4b7d56f98edd9504c9af091d5281095a71e
ENV DEBIAN_FRONTEND=noninteractive PLAYWRIGHT_BROWSERS_PATH=/opt/browsers LANG=C.UTF-8
WORKDIR /opt/pithos-browser
COPY browser/package.json browser/package-lock.json ./
RUN npm ci --omit=dev --ignore-scripts --no-audit --no-fund \
    && node node_modules/playwright/cli.js install --with-deps --no-shell chromium \
    && apt-get update && apt-get install -y --no-install-recommends tini xvfb openbox x11vnc x11-utils \
    && rm -rf /var/lib/apt/lists/* /root/.npm \
    && useradd --uid 501 --gid 20 --no-create-home --home-dir /tmp/browser-home browser
COPY browser/runtime/ ./runtime/
USER 501:20
HEALTHCHECK --interval=2s --timeout=2s --start-period=40s --retries=3 CMD ["node", "runtime/health.mjs"]
ENTRYPOINT ["/usr/bin/tini", "--", "node", "runtime/server.mjs"]
