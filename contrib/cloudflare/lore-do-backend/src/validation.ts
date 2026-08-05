// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

import type { AddressDto, FragmentDto, StoreMatch } from "./contracts";

const HASH = /^[0-9a-f]{64}$/;
const CONTEXT = /^[0-9a-f]{32}$/;

export class ValidationError extends Error {}

export function record(value: unknown, name = "body"): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new ValidationError(`${name} must be an object`);
  }
  return value as Record<string, unknown>;
}

export function stringField(value: Record<string, unknown>, name: string): string {
  const field = value[name];
  if (typeof field !== "string") throw new ValidationError(`${name} must be a string`);
  return field;
}

export function boolField(value: Record<string, unknown>, name: string): boolean {
  const field = value[name];
  if (typeof field !== "boolean") throw new ValidationError(`${name} must be a boolean`);
  return field;
}

export function hash(value: unknown, name = "hash"): string {
  if (typeof value !== "string" || !HASH.test(value)) {
    throw new ValidationError(`${name} must be 64 lowercase hex characters`);
  }
  return value;
}

export function context(value: unknown, name = "context"): string {
  if (typeof value !== "string" || !CONTEXT.test(value)) {
    throw new ValidationError(`${name} must be 32 lowercase hex characters`);
  }
  return value;
}

export function address(value: unknown): AddressDto {
  const input = record(value, "address");
  return { hash: hash(input.hash), context: context(input.context) };
}

export function storeMatch(value: unknown): StoreMatch {
  if (value !== 0 && value !== 1 && value !== 2 && value !== 3) {
    throw new ValidationError("store match must be an integer from 0 through 3");
  }
  return value;
}

export function keyType(value: unknown): number {
  if (!Number.isInteger(value) || typeof value !== "number" || value < 0 || value > 6) {
    throw new ValidationError("keyType must be an integer from 0 through 6");
  }
  return value;
}

export function uintField(
  value: Record<string, unknown>,
  name: string,
  max = Number.MAX_SAFE_INTEGER,
): number {
  return uint(value[name], name, max);
}

export function fragment(value: unknown): FragmentDto {
  const input = record(value, "fragment");
  const flags = uint(input.flags, "flags", 0xffff_ffff);
  const sizePayload = uint(input.sizePayload, "sizePayload", 256 * 1024);
  const sizeContent = uint(input.sizeContent, "sizeContent", Number.MAX_SAFE_INTEGER);
  if (sizePayload === 0 || sizePayload > sizeContent) {
    throw new ValidationError("fragment payload size must be non-zero and not exceed content size");
  }
  return { flags, sizePayload, sizeContent };
}

export function boundedArray(value: unknown, max: number, name: string): readonly unknown[] {
  if (!Array.isArray(value) || value.length === 0 || value.length > max) {
    throw new ValidationError(`${name} must contain between 1 and ${max} entries`);
  }
  return value;
}

function uint(value: unknown, name: string, max: number): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0 || value > max) {
    throw new ValidationError(`${name} must be an integer from 0 through ${max}`);
  }
  return value;
}
