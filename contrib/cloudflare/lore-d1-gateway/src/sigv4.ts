// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

import { RequestError } from "./errors";

const ALGORITHM = "AWS4-HMAC-SHA256";
const MAX_CLOCK_SKEW_MILLIS = 5 * 60 * 1_000;
const encoder = new TextEncoder();

interface ParsedAuthorization {
  readonly accessKeyId: string;
  readonly credentialDate: string;
  readonly region: string;
  readonly service: string;
  readonly signature: Uint8Array;
  readonly signedHeaders: readonly string[];
}

export interface VerifiedRequest {
  readonly body: ArrayBuffer;
  readonly target: string;
}

export async function verifySigV4(
  request: Request,
  expectedAccessKeyId: string,
  secretAccessKey: string,
): Promise<VerifiedRequest> {
  const authorization = parseAuthorization(requiredHeader(request, "authorization"));
  verifyCredentialScope(authorization, expectedAccessKeyId);

  const amzDate = requiredHeader(request, "x-amz-date");
  verifyRequestTime(amzDate, authorization.credentialDate);

  const body = await request.arrayBuffer();
  const payloadHash = await sha256Hex(body);
  const signedPayloadHash = request.headers.get("x-amz-content-sha256");
  if (
    signedPayloadHash !== null &&
    !timingSafeTextEqual(payloadHash, signedPayloadHash.toLowerCase())
  ) {
    throw signatureError("The request payload hash does not match");
  }

  const canonicalRequest = await buildCanonicalRequest(
    request,
    authorization.signedHeaders,
    payloadHash,
  );
  const credentialScope = `${authorization.credentialDate}/${authorization.region}/${authorization.service}/aws4_request`;
  const stringToSign = `${ALGORITHM}\n${amzDate}\n${credentialScope}\n${await sha256Hex(encoder.encode(canonicalRequest))}`;
  const signingKey = await deriveSigningKey(
    secretAccessKey,
    authorization.credentialDate,
    authorization.region,
    authorization.service,
  );
  const expectedSignature = new Uint8Array(await hmac(signingKey, encoder.encode(stringToSign)));

  if (!timingSafeBytesEqual(expectedSignature, authorization.signature)) {
    throw signatureError("The calculated request signature does not match");
  }

  return {
    body,
    target: requiredHeader(request, "x-amz-target"),
  };
}

function parseAuthorization(value: string): ParsedAuthorization {
  const match = value.match(
    /^AWS4-HMAC-SHA256 Credential=([^/]+)\/(\d{8})\/([^/]+)\/([^/]+)\/aws4_request, SignedHeaders=([^,]+), Signature=([0-9a-fA-F]{64})$/,
  );
  if (match === null) {
    throw signatureError("Malformed Authorization header");
  }

  const [, accessKeyId, credentialDate, region, service, signedHeaders, signature] = match;
  if (
    accessKeyId === undefined ||
    credentialDate === undefined ||
    region === undefined ||
    service === undefined ||
    signedHeaders === undefined ||
    signature === undefined
  ) {
    throw signatureError("Malformed Authorization header");
  }

  return {
    accessKeyId,
    credentialDate,
    region,
    service,
    signature: hexToBytes(signature),
    signedHeaders: signedHeaders.split(";"),
  };
}

function verifyCredentialScope(
  authorization: ParsedAuthorization,
  expectedAccessKeyId: string,
): void {
  if (!timingSafeTextEqual(authorization.accessKeyId, expectedAccessKeyId)) {
    throw signatureError("Unknown access key");
  }
  if (authorization.service !== "dynamodb") {
    throw signatureError("Credential scope must target DynamoDB");
  }
  if (!authorization.signedHeaders.includes("host")) {
    throw signatureError("The host header must be signed");
  }
  if (!authorization.signedHeaders.includes("x-amz-date")) {
    throw signatureError("The x-amz-date header must be signed");
  }
  if (!authorization.signedHeaders.includes("x-amz-target")) {
    throw signatureError("The x-amz-target header must be signed");
  }
}

function verifyRequestTime(amzDate: string, credentialDate: string): void {
  const match = amzDate.match(/^(\d{4})(\d{2})(\d{2})T(\d{2})(\d{2})(\d{2})Z$/);
  if (match === null || amzDate.slice(0, 8) !== credentialDate) {
    throw signatureError("Invalid x-amz-date header");
  }

  const numbers = match.slice(1).map(Number);
  const [year, month, day, hour, minute, second] = numbers;
  if (
    year === undefined ||
    month === undefined ||
    day === undefined ||
    hour === undefined ||
    minute === undefined ||
    second === undefined
  ) {
    throw signatureError("Invalid x-amz-date header");
  }
  const requestTime = Date.UTC(year, month - 1, day, hour, minute, second);
  if (!Number.isFinite(requestTime) || Math.abs(Date.now() - requestTime) > MAX_CLOCK_SKEW_MILLIS) {
    throw new RequestError(
      400,
      "RequestExpired",
      "Request timestamp is more than five minutes from the current time",
    );
  }
}

async function buildCanonicalRequest(
  request: Request,
  signedHeaders: readonly string[],
  payloadHash: string,
): Promise<string> {
  const url = new URL(request.url);
  const canonicalHeaders = signedHeaders
    .map((name) => {
      // Workers exposes the HTTP authority through Request.url and may omit
      // the transport-level Host header from Request.headers.
      const value = name === "host" ? url.host : request.headers.get(name);
      if (value === null) {
        throw signatureError(`Signed header is missing: ${name}`);
      }
      return `${name}:${value.trim().replace(/\s+/g, " ")}\n`;
    })
    .join("");

  return [
    request.method.toUpperCase(),
    canonicalUri(url.pathname),
    canonicalQuery(url),
    canonicalHeaders,
    signedHeaders.join(";"),
    payloadHash,
  ].join("\n");
}

function canonicalUri(pathname: string): string {
  return pathname
    .split("/")
    .map((segment) => awsPercentEncode(decodeURIComponent(segment)))
    .join("/");
}

function canonicalQuery(url: URL): string {
  return [...url.searchParams.entries()]
    .map(([key, value]) => [awsPercentEncode(key), awsPercentEncode(value)] as const)
    .sort(([leftKey, leftValue], [rightKey, rightValue]) =>
      leftKey === rightKey ? leftValue.localeCompare(rightValue) : leftKey.localeCompare(rightKey),
    )
    .map(([key, value]) => `${key}=${value}`)
    .join("&");
}

function awsPercentEncode(value: string): string {
  return encodeURIComponent(value).replace(/[!'()*]/g, (character) =>
    `%${character.charCodeAt(0).toString(16).toUpperCase()}`,
  );
}

async function deriveSigningKey(
  secretAccessKey: string,
  date: string,
  region: string,
  service: string,
): Promise<ArrayBuffer> {
  const dateKey = await hmac(encoder.encode(`AWS4${secretAccessKey}`), encoder.encode(date));
  const regionKey = await hmac(dateKey, encoder.encode(region));
  const serviceKey = await hmac(regionKey, encoder.encode(service));
  return hmac(serviceKey, encoder.encode("aws4_request"));
}

async function hmac(key: BufferSource, data: BufferSource): Promise<ArrayBuffer> {
  const cryptoKey = await crypto.subtle.importKey(
    "raw",
    key,
    { hash: "SHA-256", name: "HMAC" },
    false,
    ["sign"],
  );
  return crypto.subtle.sign("HMAC", cryptoKey, data);
}

async function sha256Hex(value: BufferSource): Promise<string> {
  return bytesToHex(new Uint8Array(await crypto.subtle.digest("SHA-256", value)));
}

function requiredHeader(request: Request, name: string): string {
  const value = request.headers.get(name);
  if (value === null || value.length === 0) {
    throw signatureError(`Missing required header: ${name}`);
  }
  return value;
}

function timingSafeTextEqual(left: string, right: string): boolean {
  return timingSafeBytesEqual(encoder.encode(left), encoder.encode(right));
}

function timingSafeBytesEqual(left: Uint8Array, right: Uint8Array): boolean {
  if (left.byteLength !== right.byteLength) {
    return false;
  }
  return crypto.subtle.timingSafeEqual(left, right);
}

function bytesToHex(bytes: Uint8Array): string {
  return [...bytes].map((value) => value.toString(16).padStart(2, "0")).join("");
}

function hexToBytes(value: string): Uint8Array {
  const bytes = new Uint8Array(value.length / 2);
  for (let index = 0; index < value.length; index += 2) {
    bytes[index / 2] = Number.parseInt(value.slice(index, index + 2), 16);
  }
  return bytes;
}

function signatureError(message: string): RequestError {
  return new RequestError(403, "UnrecognizedClientException", message);
}
