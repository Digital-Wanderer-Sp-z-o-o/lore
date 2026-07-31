// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

import { DurableObject } from "cloudflare:workers";

export interface MutableEntryDto {
  readonly key: string;
  readonly value: string;
}

interface MutableRow extends Record<string, SqlStorageValue> {
  readonly key_hash: string;
  readonly value_hash: string;
}

export interface CompareAndSwapResult {
  readonly previous: string;
  readonly swapped: boolean;
}

const NULL_HASH = "0".repeat(64);

export class MutablePartitionStore extends DurableObject<Cloudflare.Env> {
  public constructor(state: DurableObjectState, env: Cloudflare.Env) {
    super(state, env);
    state.blockConcurrencyWhile(async () => {
      this.ctx.storage.sql.exec(`
        CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);
        INSERT OR IGNORE INTO schema_version(version) VALUES (1);
        CREATE TABLE IF NOT EXISTS mutable_entries (
          key_hash TEXT NOT NULL,
          key_type INTEGER NOT NULL,
          value_hash TEXT NOT NULL,
          PRIMARY KEY(key_hash, key_type)
        );
        CREATE INDEX IF NOT EXISTS mutable_entries_type ON mutable_entries(key_type, key_hash);
      `);
    });
  }

  public load(key: string, keyType: number): string | null {
    return this.ctx.storage.sql
      .exec<{ value_hash: string }>(
        "SELECT value_hash FROM mutable_entries WHERE key_hash = ? AND key_type = ?",
        key,
        keyType,
      )
      .toArray()[0]?.value_hash ?? null;
  }

  public store(key: string, value: string, keyType: number): void {
    if (value === NULL_HASH) {
      this.ctx.storage.sql.exec(
        "DELETE FROM mutable_entries WHERE key_hash = ? AND key_type = ?",
        key,
        keyType,
      );
      return;
    }
    this.ctx.storage.sql.exec(
      "INSERT INTO mutable_entries(key_hash, key_type, value_hash) VALUES (?, ?, ?) " +
        "ON CONFLICT(key_hash, key_type) DO UPDATE SET value_hash=excluded.value_hash",
      key,
      keyType,
      value,
    );
  }

  public compareAndSwap(
    key: string,
    expected: string,
    value: string,
    keyType: number,
  ): CompareAndSwapResult {
    return this.ctx.storage.transactionSync(() => {
      const previous = this.load(key, keyType) ?? NULL_HASH;
      if (previous !== expected) return { previous, swapped: false };
      this.store(key, value, keyType);
      return { previous, swapped: true };
    });
  }

  public list(keyType: number): MutableEntryDto[] {
    return this.ctx.storage.sql
      .exec<MutableRow>(
        "SELECT key_hash, value_hash FROM mutable_entries WHERE key_type = ? ORDER BY key_hash",
        keyType,
      )
      .toArray()
      .map((row) => ({ key: row.key_hash, value: row.value_hash }));
  }
}
