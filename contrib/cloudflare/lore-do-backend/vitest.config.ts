// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

import path from "node:path";
import { fileURLToPath } from "node:url";
import { cloudflareTest } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";

const projectRoot = path.dirname(fileURLToPath(import.meta.url));

export default defineConfig({
  plugins: [
    cloudflareTest(() => ({
      miniflare: { bindings: { AUTH_SHARED_SECRET: "test-secret" } },
      wrangler: { configPath: path.join(projectRoot, "wrangler.jsonc") },
    })),
  ],
});
