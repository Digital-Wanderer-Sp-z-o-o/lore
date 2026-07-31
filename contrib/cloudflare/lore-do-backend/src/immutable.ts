// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

import { DurableObject } from "cloudflare:workers";
import type { AddressDto, FragmentDto, QueryResultDto, StoreMatch } from "./contracts";

const OBLITERATED = 1 << 8;
const OBLITERATING = 1 << 9;

interface MetadataRow extends Record<string, SqlStorageValue> {
  readonly flags: number;
  readonly size_payload: number;
  readonly size_content: number;
}

export interface ObliterationStart {
  readonly status: "started" | "already_obliterating" | "not_found";
  readonly fragment?: FragmentDto;
}

export interface AssociationRemoval {
  readonly remainingAssociations: number;
}

export class ImmutableMetadataShard extends DurableObject<Cloudflare.Env> {
  public constructor(state: DurableObjectState, env: Cloudflare.Env) {
    super(state, env);
    state.blockConcurrencyWhile(async () => {
      this.ctx.storage.sql.exec(`
        CREATE TABLE IF NOT EXISTS schema_version (
          version INTEGER PRIMARY KEY
        );
        INSERT OR IGNORE INTO schema_version(version) VALUES (1);
        CREATE TABLE IF NOT EXISTS fragments (
          hash TEXT PRIMARY KEY,
          flags INTEGER NOT NULL,
          size_payload INTEGER NOT NULL,
          size_content INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS associations (
          hash TEXT NOT NULL,
          partition_id TEXT NOT NULL,
          context_id TEXT NOT NULL,
          PRIMARY KEY(hash, partition_id, context_id),
          FOREIGN KEY(hash) REFERENCES fragments(hash)
        );
        CREATE INDEX IF NOT EXISTS associations_partition
          ON associations(hash, partition_id);
      `);
    });
  }

  public existBatch(
    partition: string,
    addresses: readonly AddressDto[],
    matchRequested: StoreMatch,
  ): StoreMatch[] {
    return addresses.map((address) => this.bestMatch(partition, address, matchRequested));
  }

  public query(
    partition: string,
    address: AddressDto,
    matchRequested: StoreMatch,
  ): QueryResultDto {
    const matchMade = this.bestMatch(partition, address, matchRequested);
    if (matchMade === 0) return { matchMade };
    const row = this.metadata(address.hash);
    if (row === undefined) return { matchMade: 0 };
    return { matchMade, fragment: fragmentFromRow(row) };
  }

  public put(partition: string, address: AddressDto, fragment: FragmentDto): void {
    this.ctx.storage.transactionSync(() => {
      const existing = this.metadata(address.hash);
      if (existing !== undefined && !sameFragment(existing, fragment)) {
        throw new Error("CONFLICT: fragment metadata differs for an existing content hash");
      }
      if (existing === undefined || (existing.flags & OBLITERATED) !== 0) {
        this.ctx.storage.sql.exec(
          "INSERT INTO fragments(hash, flags, size_payload, size_content) VALUES (?, ?, ?, ?) " +
            "ON CONFLICT(hash) DO UPDATE SET flags=excluded.flags, size_payload=excluded.size_payload, size_content=excluded.size_content",
          address.hash,
          fragment.flags,
          fragment.sizePayload,
          fragment.sizeContent,
        );
      }
      this.ctx.storage.sql.exec(
        "INSERT OR IGNORE INTO associations(hash, partition_id, context_id) VALUES (?, ?, ?)",
        address.hash,
        partition,
        address.context,
      );
    });
  }

  public associate(partition: string, address: AddressDto): void {
    this.ctx.storage.transactionSync(() => {
      const existing = this.metadata(address.hash);
      if (existing === undefined || (existing.flags & (OBLITERATING | OBLITERATED)) !== 0) {
        throw new Error("NOT_FOUND: fragment metadata is unavailable");
      }
      this.ctx.storage.sql.exec(
        "INSERT OR IGNORE INTO associations(hash, partition_id, context_id) VALUES (?, ?, ?)",
        address.hash,
        partition,
        address.context,
      );
    });
  }

  public beginObliteration(hash: string): ObliterationStart {
    return this.ctx.storage.transactionSync(() => {
      const existing = this.metadata(hash);
      if (existing === undefined) return { status: "not_found" };
      if ((existing.flags & (OBLITERATING | OBLITERATED)) !== 0) {
        return { status: "already_obliterating", fragment: fragmentFromRow(existing) };
      }
      this.ctx.storage.sql.exec(
        "UPDATE fragments SET flags = flags | ? WHERE hash = ?",
        OBLITERATING,
        hash,
      );
      return { status: "started", fragment: fragmentFromRow(existing) };
    });
  }

  public removeAssociation(partition: string, address: AddressDto): AssociationRemoval {
    return this.ctx.storage.transactionSync(() => {
      this.ctx.storage.sql.exec(
        "DELETE FROM associations WHERE hash = ? AND partition_id = ? AND context_id = ?",
        address.hash,
        partition,
        address.context,
      );
      const row = this.ctx.storage.sql
        .exec<{ count: number }>("SELECT COUNT(*) AS count FROM associations WHERE hash = ?", address.hash)
        .one();
      return { remainingAssociations: row.count };
    });
  }

  public cancelObliteration(hash: string, fragment: FragmentDto): void {
    this.ctx.storage.sql.exec(
      "UPDATE fragments SET flags = ?, size_payload = ?, size_content = ? WHERE hash = ?",
      fragment.flags,
      fragment.sizePayload,
      fragment.sizeContent,
      hash,
    );
  }

  public finishObliteration(hash: string): void {
    this.ctx.storage.sql.exec(
      "UPDATE fragments SET flags = ?, size_payload = 0, size_content = 0 WHERE hash = ?",
      OBLITERATED,
      hash,
    );
  }

  public associationCount(hash: string): number {
    return this.ctx.storage.sql
      .exec<{ count: number }>("SELECT COUNT(*) AS count FROM associations WHERE hash = ?", hash)
      .one().count;
  }

  private bestMatch(
    partition: string,
    address: AddressDto,
    matchRequested: StoreMatch,
  ): StoreMatch {
    if (matchRequested === 0 || this.metadata(address.hash) === undefined) return 0;
    if (matchRequested >= 3 && this.hasAssociation(address.hash, partition, address.context)) return 3;
    if (matchRequested >= 2 && this.hasPartition(address.hash, partition)) return 2;
    return 1;
  }

  private metadata(hash: string): MetadataRow | undefined {
    return this.ctx.storage.sql
      .exec<MetadataRow>(
        "SELECT flags, size_payload, size_content FROM fragments WHERE hash = ?",
        hash,
      )
      .toArray()[0];
  }

  private hasAssociation(hash: string, partition: string, context: string): boolean {
    return this.ctx.storage.sql
      .exec<{ present: number }>(
        "SELECT 1 AS present FROM associations WHERE hash = ? AND partition_id = ? AND context_id = ? LIMIT 1",
        hash,
        partition,
        context,
      )
      .toArray().length > 0;
  }

  private hasPartition(hash: string, partition: string): boolean {
    return this.ctx.storage.sql
      .exec<{ present: number }>(
        "SELECT 1 AS present FROM associations WHERE hash = ? AND partition_id = ? LIMIT 1",
        hash,
        partition,
      )
      .toArray().length > 0;
  }
}

function fragmentFromRow(row: MetadataRow): FragmentDto {
  return { flags: row.flags, sizePayload: row.size_payload, sizeContent: row.size_content };
}

function sameFragment(row: MetadataRow, fragment: FragmentDto): boolean {
  return row.flags === fragment.flags &&
    row.size_payload === fragment.sizePayload &&
    row.size_content === fragment.sizeContent;
}
