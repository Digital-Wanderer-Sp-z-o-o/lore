// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

import { DurableObject } from "cloudflare:workers";
import type {
  AddressDto,
  FragmentDto,
  ObliterationAuditCursorDto,
  ObliterationAuditDto,
  ObliterationAuditPageDto,
  ObliterationAuditStatus,
  QueryResultDto,
  StoreMatch,
} from "./contracts";

const OBLITERATED = 1 << 8;
const OBLITERATING = 1 << 9;

interface MetadataRow extends Record<string, SqlStorageValue> {
  readonly flags: number;
  readonly size_payload: number;
  readonly size_content: number;
}

interface SchemaVersionRow extends Record<string, SqlStorageValue> {
  readonly version: number;
}

interface ObliterationAuditRow extends Record<string, SqlStorageValue> {
  readonly event_id: string;
  readonly actor_id: string;
  readonly correlation_id: string;
  readonly repository_id: string;
  readonly hash: string;
  readonly context_id: string;
  readonly original_flags: number;
  readonly original_size_payload: number;
  readonly original_size_content: number;
  readonly status: ObliterationAuditStatus;
  readonly remaining_associations: number | null;
  readonly recorded_at: number;
  readonly completed_at: number | null;
}

const CURRENT_IMMUTABLE_SCHEMA_VERSION = 2;

export interface ObliterationBeginRequest {
  readonly partition: string;
  readonly address: AddressDto;
  readonly actor: string;
  readonly correlationId: string;
  readonly recordedAt: number;
}

export interface ObliterationStart {
  readonly status: "started" | "resuming" | "already_obliterated" | "not_found";
  readonly eventId?: string;
  readonly stage?: "association_pending" | "association_removed";
  readonly remainingAssociations?: number;
  readonly fragment?: FragmentDto;
}

export interface AssociationRemoval {
  readonly remainingAssociations: number;
}

export class ImmutableMetadataShard extends DurableObject<Cloudflare.Env> {
  public constructor(state: DurableObjectState, env: Cloudflare.Env) {
    super(state, env);
    state.blockConcurrencyWhile(async () => {
      this.ctx.storage.transactionSync(() => {
        migrateImmutableSchema(this.ctx.storage.sql);
      });
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

  public beginAuditedObliteration(
    request: ObliterationBeginRequest,
  ): ObliterationStart {
    return this.ctx.storage.transactionSync(() => {
      const { partition, address, actor, correlationId, recordedAt } = request;
      const existing = this.metadata(address.hash);
      if (existing === undefined) return { status: "not_found" };
      if ((existing.flags & OBLITERATED) !== 0) {
        return { status: "already_obliterated" };
      }
      if ((existing.flags & OBLITERATING) !== 0) {
        const audit = this.pendingObliteration(address.hash);
        if (audit === undefined) {
          throw new Error("CONFLICT: legacy obliteration has no durable recovery audit");
        }
        if (audit.repository_id !== partition || audit.context_id !== address.context) {
          throw new Error("CONFLICT: another repository association is being obliterated");
        }
        return {
          status: "resuming",
          eventId: audit.event_id,
          stage: audit.status === "association_pending"
            ? "association_pending"
            : "association_removed",
          ...(audit.remaining_associations === null
            ? {}
            : { remainingAssociations: audit.remaining_associations }),
          fragment: originalFragment(audit),
        };
      }
      if (!this.hasAssociation(address.hash, partition, address.context)) {
        return this.completedObliteration(address.hash, partition, address.context) === undefined
          ? { status: "not_found" }
          : { status: "already_obliterated" };
      }
      const eventId = crypto.randomUUID();
      this.ctx.storage.sql.exec(
        "UPDATE fragments SET flags = flags | ? WHERE hash = ?",
        OBLITERATING,
        address.hash,
      );
      this.ctx.storage.sql.exec(
        "INSERT INTO obliteration_audit(event_id, actor_id, correlation_id, repository_id, hash, context_id, original_flags, original_size_payload, original_size_content, status, remaining_associations, recorded_at, completed_at) " +
          "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'association_pending', NULL, ?, NULL)",
        eventId,
        actor,
        correlationId,
        partition,
        address.hash,
        address.context,
        existing.flags,
        existing.size_payload,
        existing.size_content,
        recordedAt,
      );
      return {
        status: "started",
        eventId,
        stage: "association_pending",
        fragment: fragmentFromRow(existing),
      };
    });
  }

  public beginObliteration(hash: string): {
    readonly status: "started" | "already_obliterating" | "not_found";
    readonly fragment?: FragmentDto;
  } {
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

  public removeAuditedAssociation(
    eventId: string,
    partition: string,
    address: AddressDto,
  ): AssociationRemoval {
    return this.ctx.storage.transactionSync(() => {
      const audit = this.requiredObliteration(eventId, partition, address);
      if (audit.status === "association_removed") {
        return { remainingAssociations: audit.remaining_associations ?? 0 };
      }
      if (audit.status !== "association_pending") {
        throw new Error("CONFLICT: obliteration audit event is already complete");
      }
      if (!this.hasAssociation(address.hash, partition, address.context)) {
        throw new Error("CONFLICT: obliteration association disappeared before audited removal");
      }
      this.ctx.storage.sql.exec(
        "DELETE FROM associations WHERE hash = ? AND partition_id = ? AND context_id = ?",
        address.hash,
        partition,
        address.context,
      );
      const row = this.ctx.storage.sql
        .exec<{ count: number }>("SELECT COUNT(*) AS count FROM associations WHERE hash = ?", address.hash)
        .one();
      this.ctx.storage.sql.exec(
        "UPDATE obliteration_audit SET status = 'association_removed', remaining_associations = ?, completed_at = NULL WHERE event_id = ?",
        row.count,
        eventId,
      );
      return { remainingAssociations: row.count };
    });
  }

  public removeAssociation(
    partition: string,
    address: AddressDto,
  ): AssociationRemoval {
    return this.ctx.storage.transactionSync(() => {
      this.ctx.storage.sql.exec(
        "DELETE FROM associations WHERE hash = ? AND partition_id = ? AND context_id = ?",
        address.hash,
        partition,
        address.context,
      );
      const row = this.ctx.storage.sql
        .exec<{ count: number }>(
          "SELECT COUNT(*) AS count FROM associations WHERE hash = ?",
          address.hash,
        )
        .one();
      return { remainingAssociations: row.count };
    });
  }

  public completeRetainedAuditedObliteration(
    eventId: string,
    partition: string,
    address: AddressDto,
    completedAt: number,
  ): void {
    this.ctx.storage.transactionSync(() => {
      const audit = this.requiredObliteration(eventId, partition, address);
      if (audit.status === "payload_retained") return;
      if (audit.status !== "association_removed" || (audit.remaining_associations ?? 0) === 0) {
        throw new Error("CONFLICT: retained payload completion requires remaining associations");
      }
      this.ctx.storage.sql.exec(
        "UPDATE fragments SET flags = ?, size_payload = ?, size_content = ? WHERE hash = ?",
        audit.original_flags,
        audit.original_size_payload,
        audit.original_size_content,
        address.hash,
      );
      this.ctx.storage.sql.exec(
        "UPDATE obliteration_audit SET status = 'payload_retained', completed_at = ? WHERE event_id = ?",
        completedAt,
        eventId,
      );
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

  public finishAuditedObliteration(
    eventId: string,
    partition: string,
    address: AddressDto,
    completedAt: number,
  ): void {
    this.ctx.storage.transactionSync(() => {
      const audit = this.requiredObliteration(eventId, partition, address);
      if (audit.status === "payload_obliterated") return;
      if (audit.status !== "association_removed" || audit.remaining_associations !== 0) {
        throw new Error("CONFLICT: payload obliteration requires zero remaining associations");
      }
      this.ctx.storage.sql.exec(
        "UPDATE fragments SET flags = ?, size_payload = 0, size_content = 0 WHERE hash = ?",
        OBLITERATED,
        address.hash,
      );
      this.ctx.storage.sql.exec(
        "UPDATE obliteration_audit SET status = 'payload_obliterated', completed_at = ? WHERE event_id = ?",
        completedAt,
        eventId,
      );
    });
  }

  public finishObliteration(hash: string): void {
    this.ctx.storage.sql.exec(
      "UPDATE fragments SET flags = ?, size_payload = 0, size_content = 0 WHERE hash = ?",
      OBLITERATED,
      hash,
    );
  }

  public queryObliterationAudit(
    repository: string,
    address: AddressDto,
    limit: number,
    cursor?: ObliterationAuditCursorDto,
  ): ObliterationAuditPageDto {
    const rows = this.queryObliterationAuditRows(repository, address, limit + 1, cursor);
    const hasNextPage = rows.length > limit;
    const events = rows.slice(0, limit).map(obliterationAuditFromRow);
    const last = events.at(-1);
    return {
      events,
      ...(hasNextPage && last !== undefined
        ? { nextCursor: { recordedAt: last.recordedAt, eventId: last.eventId } }
        : {}),
    };
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

  private pendingObliteration(hash: string): ObliterationAuditRow | undefined {
    return this.ctx.storage.sql
      .exec<ObliterationAuditRow>(
        "SELECT * FROM obliteration_audit WHERE hash = ? AND status IN ('association_pending', 'association_removed') ORDER BY recorded_at DESC, event_id DESC LIMIT 1",
        hash,
      )
      .toArray()[0];
  }

  private completedObliteration(
    hash: string,
    partition: string,
    context: string,
  ): ObliterationAuditRow | undefined {
    return this.ctx.storage.sql
      .exec<ObliterationAuditRow>(
        "SELECT * FROM obliteration_audit WHERE hash = ? AND repository_id = ? AND context_id = ? AND status IN ('payload_retained', 'payload_obliterated') ORDER BY recorded_at DESC, event_id DESC LIMIT 1",
        hash,
        partition,
        context,
      )
      .toArray()[0];
  }

  private requiredObliteration(
    eventId: string,
    partition: string,
    address: AddressDto,
  ): ObliterationAuditRow {
    const audit = this.ctx.storage.sql
      .exec<ObliterationAuditRow>(
        "SELECT * FROM obliteration_audit WHERE event_id = ?",
        eventId,
      )
      .toArray()[0];
    if (audit === undefined) throw new Error("NOT_FOUND: obliteration audit event does not exist");
    if (
      audit.repository_id !== partition ||
      audit.hash !== address.hash ||
      audit.context_id !== address.context
    ) {
      throw new Error("CONFLICT: obliteration audit event does not match the requested association");
    }
    return audit;
  }

  private queryObliterationAuditRows(
    repository: string,
    address: AddressDto,
    limit: number,
    cursor?: ObliterationAuditCursorDto,
  ): ObliterationAuditRow[] {
    if (cursor === undefined) {
      return this.ctx.storage.sql
        .exec<ObliterationAuditRow>(
          "SELECT * FROM obliteration_audit WHERE repository_id = ? AND hash = ? AND context_id = ? ORDER BY recorded_at DESC, event_id DESC LIMIT ?",
          repository,
          address.hash,
          address.context,
          limit,
        )
        .toArray();
    }
    return this.ctx.storage.sql
      .exec<ObliterationAuditRow>(
        "SELECT * FROM obliteration_audit WHERE repository_id = ? AND hash = ? AND context_id = ? AND (recorded_at < ? OR (recorded_at = ? AND event_id < ?)) ORDER BY recorded_at DESC, event_id DESC LIMIT ?",
        repository,
        address.hash,
        address.context,
        cursor.recordedAt,
        cursor.recordedAt,
        cursor.eventId,
        limit,
      )
      .toArray();
  }
}

export function migrateImmutableSchema(sql: SqlStorage): void {
  sql.exec("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY)");
  const current = sql
    .exec<SchemaVersionRow>(
      "SELECT COALESCE(MAX(version), 0) AS version FROM schema_version",
    )
    .one().version;
  if (current > CURRENT_IMMUTABLE_SCHEMA_VERSION) {
    throw new Error(
      `immutable schema version ${current} is newer than supported version ${CURRENT_IMMUTABLE_SCHEMA_VERSION}`,
    );
  }
  if (current < 1) {
    sql.exec(`
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
      INSERT INTO schema_version(version) VALUES (1);
    `);
  }
  if (current < 2) {
    sql.exec(`
      CREATE TABLE IF NOT EXISTS obliteration_audit (
        event_id TEXT PRIMARY KEY,
        actor_id TEXT NOT NULL,
        correlation_id TEXT NOT NULL,
        repository_id TEXT NOT NULL,
        hash TEXT NOT NULL,
        context_id TEXT NOT NULL,
        original_flags INTEGER NOT NULL,
        original_size_payload INTEGER NOT NULL,
        original_size_content INTEGER NOT NULL,
        status TEXT NOT NULL CHECK(status IN ('association_pending', 'association_removed', 'payload_retained', 'payload_obliterated')),
        remaining_associations INTEGER,
        recorded_at INTEGER NOT NULL,
        completed_at INTEGER
      );
      CREATE INDEX IF NOT EXISTS obliteration_audit_repository_hash_time
        ON obliteration_audit(repository_id, hash, context_id, recorded_at DESC, event_id DESC);
      CREATE INDEX IF NOT EXISTS obliteration_audit_pending_hash
        ON obliteration_audit(hash, recorded_at DESC)
        WHERE status IN ('association_pending', 'association_removed');
      INSERT INTO schema_version(version) VALUES (2);
    `);
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

function originalFragment(row: ObliterationAuditRow): FragmentDto {
  return {
    flags: row.original_flags,
    sizePayload: row.original_size_payload,
    sizeContent: row.original_size_content,
  };
}

function obliterationAuditFromRow(row: ObliterationAuditRow): ObliterationAuditDto {
  return {
    eventId: row.event_id,
    actor: row.actor_id,
    correlationId: row.correlation_id,
    repository: row.repository_id,
    address: { hash: row.hash, context: row.context_id },
    status: row.status,
    ...(row.remaining_associations === null
      ? {}
      : { remainingAssociations: row.remaining_associations }),
    recordedAt: row.recorded_at,
    ...(row.completed_at === null ? {} : { completedAt: row.completed_at }),
  };
}
