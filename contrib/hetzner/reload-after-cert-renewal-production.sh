#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
# SPDX-License-Identifier: MIT

set -euo pipefail

container="archigma-lore-production"
health_url="http://127.0.0.1:41339/health_check"

if [[ "$(docker inspect --format '{{.State.Running}}' "${container}")" != "true" ]]; then
  echo "LORE container ${container} is not running" >&2
  exit 1
fi

docker restart --time 30 "${container}" >/dev/null

if ! curl \
  --fail \
  --silent \
  --show-error \
  --retry 12 \
  --retry-delay 1 \
  --retry-connrefused \
  "${health_url}" >/dev/null; then
  docker logs --tail 100 "${container}" >&2
  exit 1
fi

echo "LORE production reloaded the renewed certificate and passed health check"
