// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT
// @ts-check

import { createHash, createHmac } from "node:crypto";

const baseUrl = requiredUrl("LORE_WORKER_BASE_URL");
const workerName = requiredString("LORE_WORKER_NAME");
const versionId = requiredUuid("LORE_WORKER_VERSION_ID");
const repositoryId = requiredHex("LORE_SMOKE_REPOSITORY_ID", 16);
const secret = requiredString("LORE_CLOUDFLARE_SHARED_SECRET");
const versionOverride = `${workerName}="${versionId}"`;

await verifyHealth();
const auditEventCount = await verifySignedRecoveryAudit();
console.log(
  JSON.stringify({
    status: "ok",
    workerName,
    versionId,
    repositoryId,
    auditEventCount,
  }),
);

async function verifyHealth() {
  const response = await fetch(new URL("/health", baseUrl), {
    headers: versionHeaders(),
  });
  const body = await jsonObject(response, "worker health");
  if (
    body.status !== "ok" ||
    body.apiVersion !== "v1" ||
    body.durableObjects !== "configured" ||
    !isRecord(body.deployment) ||
    body.deployment.id !== versionId ||
    !isRecord(body.capabilities) ||
    body.capabilities.lockRecoveryAudit !== "v1" ||
    body.capabilities.lockRecoveryOwnerCas !== true
  ) {
    throw new Error(
      "worker health did not prove the requested deployment capabilities",
    );
  }
}

async function verifySignedRecoveryAudit() {
  const path = "/v1/locks/recovery-audit";
  const body = Buffer.from(
    JSON.stringify({ repository: repositoryId, limit: 1 }),
    "utf8",
  );
  const timestamp = Math.floor(Date.now() / 1_000).toString();
  const digest = createHash("sha256").update(body).digest("hex");
  const signature = createHmac("sha256", secret)
    .update(`${timestamp}\nPOST\n${path}\n${digest}`)
    .digest("hex");
  const response = await fetch(new URL(path, baseUrl), {
    method: "POST",
    body,
    headers: {
      ...versionHeaders(),
      "content-type": "application/json",
      "x-lore-signature": signature,
      "x-lore-timestamp": timestamp,
    },
  });
  const result = await jsonObject(response, "signed recovery-audit query");
  if (!Array.isArray(result.events)) {
    throw new Error("signed recovery-audit query omitted its events array");
  }
  return result.events.length;
}

function versionHeaders() {
  return { "Cloudflare-Workers-Version-Overrides": versionOverride };
}

/** @param {Response} response @param {string} operation */
async function jsonObject(response, operation) {
  const text = await response.text();
  if (!response.ok) {
    throw new Error(
      `${operation} failed with HTTP ${response.status}: ${text}`,
    );
  }
  let value;
  try {
    value = JSON.parse(text);
  } catch {
    throw new Error(`${operation} returned invalid JSON`);
  }
  if (!isRecord(value)) throw new Error(`${operation} returned a non-object`);
  return value;
}

/** @param {unknown} value @returns {value is Record<string, unknown>} */
function isRecord(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** @param {string} name */
function requiredString(name) {
  const value = process.env[name]?.trim();
  if (!value) throw new Error(`${name} is required`);
  return value;
}

/** @param {string} name */
function requiredUrl(name) {
  const value = new URL(requiredString(name));
  if (value.protocol !== "https:" || value.username || value.password) {
    throw new Error(
      `${name} must be an HTTPS URL without embedded credentials`,
    );
  }
  return value;
}

/** @param {string} name */
function requiredUuid(name) {
  const value = requiredString(name).toLowerCase();
  if (
    !/^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(
      value,
    )
  ) {
    throw new Error(`${name} must be a UUID`);
  }
  return value;
}

/** @param {string} name @param {number} byteLength */
function requiredHex(name, byteLength) {
  const value = requiredString(name).toLowerCase();
  if (!new RegExp(`^[0-9a-f]{${byteLength * 2}}$`).test(value)) {
    throw new Error(
      `${name} must contain exactly ${byteLength} hexadecimal bytes`,
    );
  }
  return value;
}
