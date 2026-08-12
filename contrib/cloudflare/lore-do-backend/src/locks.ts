// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

import { DurableObject } from "cloudflare:workers";
import type {
  LockDataDto,
  LockQueryDto,
  LockRecoveryAuditCursorDto,
  LockRecoveryAuditDto,
  LockRecoveryAuditPageDto,
  LockResourceDto,
} from "./contracts";

interface LockRow extends Record<string, SqlStorageValue> {
  readonly hash: string;
  readonly repository_id: string;
  readonly branch_id: string;
  readonly description: string;
  readonly owner_id: string;
  readonly locked_at: number;
}

interface LockRecoveryAuditRow extends Record<string, SqlStorageValue> {
  readonly event_id: string;
  readonly actor_id: string;
  readonly expected_owner_id: string;
  readonly repository_id: string;
  readonly resources_json: string;
  readonly recorded_at: number;
}

interface LockSchemaVersionRow extends Record<string, SqlStorageValue> {
  readonly version: number;
}

const CURRENT_LOCK_SCHEMA_VERSION = 2;

export interface LockMutationResult {
  readonly status: "ok" | "not_owned" | "not_found";
  readonly locks?: readonly LockDataDto[];
  readonly resources?: readonly LockResourceDto[];
}

export class LockCoordinator extends DurableObject<Cloudflare.Env> {
  public constructor(state: DurableObjectState, env: Cloudflare.Env) {
    super(state, env);
    state.blockConcurrencyWhile(async () => {
      this.ctx.storage.transactionSync(() => {
        migrateLockSchema(this.ctx.storage.sql);
      });
    });
  }

  public lockResources(
    owner: string,
    repository: string,
    resources: readonly LockResourceDto[],
    lockedAt: number,
    leaseDurationMs?: number,
  ): LockMutationResult {
    return this.ctx.storage.transactionSync(() => {
      if (leaseDurationMs !== undefined) {
        this.deleteExpired(lockedAt, leaseDurationMs);
      }
      const unique = deduplicate(resources);
      const existingByKey = new Map<string, LockRow>();
      for (const resource of unique) {
        const existing = this.get(repository, resource);
        if (existing !== undefined) {
          if (existing.owner_id !== owner) return { status: "not_owned" };
          existingByKey.set(`${resource.hash}:${resource.branch}`, existing);
        }
      }
      const newlyLocked: LockDataDto[] = [];
      for (const resource of unique) {
        if (existingByKey.has(`${resource.hash}:${resource.branch}`)) {
          if (leaseDurationMs !== undefined) {
            this.ctx.storage.sql.exec(
              "UPDATE locks SET description = ?, locked_at = ? WHERE hash = ? AND repository_id = ? AND branch_id = ? AND owner_id = ?",
              resource.description,
              lockedAt,
              resource.hash,
              repository,
              resource.branch,
              owner,
            );
          }
          continue;
        }
        this.ctx.storage.sql.exec(
          "INSERT INTO locks(hash, repository_id, branch_id, description, owner_id, locked_at) VALUES (?, ?, ?, ?, ?, ?)",
          resource.hash,
          repository,
          resource.branch,
          resource.description,
          owner,
          lockedAt,
        );
        newlyLocked.push({ resource, owner, lockedAt });
      }
      return { status: "ok", locks: newlyLocked };
    });
  }

  public unlockResources(
    owner: string,
    validateUser: boolean,
    repository: string,
    resources: readonly LockResourceDto[],
    now?: number,
    leaseDurationMs?: number,
  ): LockMutationResult {
    return this.ctx.storage.transactionSync(() => {
      this.deleteExpiredIfEnabled(now, leaseDurationMs);
      return this.releaseComparedResources(
        owner,
        validateUser,
        repository,
        resources,
      );
    });
  }

  public recoverResources(
    actor: string,
    expectedOwner: string,
    repository: string,
    resources: readonly LockResourceDto[],
    now: number,
    leaseDurationMs: number,
  ): LockMutationResult {
    if (actor === expectedOwner) {
      throw new Error(
        "CONFLICT: administrative recovery requires a foreign owner",
      );
    }
    return this.ctx.storage.transactionSync(() => {
      this.deleteExpired(now, leaseDurationMs);
      const result = this.releaseComparedResources(
        expectedOwner,
        true,
        repository,
        resources,
      );
      if (result.status !== "ok") return result;
      this.ctx.storage.sql.exec(
        "INSERT INTO lock_recovery_audit(event_id, actor_id, expected_owner_id, repository_id, resources_json, recorded_at) VALUES (?, ?, ?, ?, ?, ?)",
        crypto.randomUUID(),
        actor,
        expectedOwner,
        repository,
        JSON.stringify(result.resources ?? []),
        now,
      );
      return result;
    });
  }

  public queryRecoveryAudit(
    repository: string,
    limit: number,
    cursor?: LockRecoveryAuditCursorDto,
  ): LockRecoveryAuditPageDto {
    const rows = this.queryRecoveryAuditRows(repository, limit + 1, cursor);
    const hasNextPage = rows.length > limit;
    const events = rows.slice(0, limit).map(auditFromRow);
    const last = events.at(-1);
    return {
      events,
      ...(hasNextPage && last !== undefined
        ? {
            nextCursor: {
              recordedAt: last.recordedAt,
              eventId: last.eventId,
            },
          }
        : {}),
    };
  }

  public checkLocksStatus(
    repository: string,
    resources: readonly LockResourceDto[],
    now?: number,
    leaseDurationMs?: number,
  ): LockDataDto[] {
    this.deleteExpiredIfEnabled(now, leaseDurationMs);
    return deduplicate(resources).flatMap((resource) => {
      const row = this.get(repository, resource);
      return row === undefined ? [] : [lockFromRow(row)];
    });
  }

  public queryLocks(
    query: LockQueryDto,
    now?: number,
    leaseDurationMs?: number,
  ): LockDataDto[] {
    this.deleteExpiredIfEnabled(now, leaseDurationMs);
    const [where, parameters] = querySql(query);
    return this.ctx.storage.sql
      .exec<LockRow>(
        `SELECT hash, repository_id, branch_id, description, owner_id, locked_at FROM locks WHERE ${where} ORDER BY repository_id, branch_id, hash`,
        ...parameters,
      )
      .toArray()
      .map(lockFromRow);
  }

  private get(
    repository: string,
    resource: LockResourceDto,
  ): LockRow | undefined {
    return this.ctx.storage.sql
      .exec<LockRow>(
        "SELECT hash, repository_id, branch_id, description, owner_id, locked_at FROM locks " +
          "WHERE hash = ? AND repository_id = ? AND branch_id = ?",
        resource.hash,
        repository,
        resource.branch,
      )
      .toArray()[0];
  }

  private deleteExpired(now: number, leaseDurationMs: number): void {
    if (!Number.isFinite(leaseDurationMs) || leaseDurationMs <= 0) {
      throw new Error("lock lease duration must be a positive finite number");
    }
    const expiresBefore = now - leaseDurationMs;
    this.ctx.storage.sql.exec(
      "DELETE FROM locks WHERE locked_at <= ?",
      expiresBefore,
    );
  }

  private deleteExpiredIfEnabled(
    now?: number,
    leaseDurationMs?: number,
  ): void {
    if (now === undefined && leaseDurationMs === undefined) return;
    if (now === undefined || leaseDurationMs === undefined) {
      throw new Error("lock lease timestamp and duration must be supplied together");
    }
    this.deleteExpired(now, leaseDurationMs);
  }

  private releaseComparedResources(
    expectedOwner: string,
    validateOwner: boolean,
    repository: string,
    resources: readonly LockResourceDto[],
  ): LockMutationResult {
    const unique = deduplicate(resources);
    for (const resource of unique) {
      const existing = this.get(repository, resource);
      if (existing === undefined) return { status: "not_found" };
      if (validateOwner && existing.owner_id !== expectedOwner) {
        return { status: "not_owned" };
      }
    }
    for (const resource of unique) {
      this.ctx.storage.sql.exec(
        "DELETE FROM locks WHERE hash = ? AND repository_id = ? AND branch_id = ?",
        resource.hash,
        repository,
        resource.branch,
      );
    }
    return { status: "ok", resources: unique };
  }

  private queryRecoveryAuditRows(
    repository: string,
    limit: number,
    cursor?: LockRecoveryAuditCursorDto,
  ): LockRecoveryAuditRow[] {
    const columns =
      "event_id, actor_id, expected_owner_id, repository_id, resources_json, recorded_at";
    if (cursor === undefined) {
      return this.ctx.storage.sql
        .exec<LockRecoveryAuditRow>(
          `SELECT ${columns} FROM lock_recovery_audit WHERE repository_id = ? ORDER BY recorded_at DESC, event_id DESC LIMIT ?`,
          repository,
          limit,
        )
        .toArray();
    }
    return this.ctx.storage.sql
      .exec<LockRecoveryAuditRow>(
        `SELECT ${columns} FROM lock_recovery_audit WHERE repository_id = ? AND (recorded_at < ? OR (recorded_at = ? AND event_id < ?)) ORDER BY recorded_at DESC, event_id DESC LIMIT ?`,
        repository,
        cursor.recordedAt,
        cursor.recordedAt,
        cursor.eventId,
        limit,
      )
      .toArray();
  }
}

export function migrateLockSchema(sql: SqlStorage): void {
  sql.exec(
    "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY)",
  );
  const current = sql
    .exec<LockSchemaVersionRow>(
      "SELECT COALESCE(MAX(version), 0) AS version FROM schema_version",
    )
    .one().version;
  if (current > CURRENT_LOCK_SCHEMA_VERSION) {
    throw new Error(
      `lock schema version ${current} is newer than supported version ${CURRENT_LOCK_SCHEMA_VERSION}`,
    );
  }
  if (current < 1) {
    sql.exec(`
      CREATE TABLE IF NOT EXISTS locks (
        hash TEXT NOT NULL,
        repository_id TEXT NOT NULL,
        branch_id TEXT NOT NULL,
        description TEXT NOT NULL,
        owner_id TEXT NOT NULL,
        locked_at INTEGER NOT NULL,
        PRIMARY KEY(hash, repository_id, branch_id)
      );
      CREATE INDEX IF NOT EXISTS locks_owner ON locks(owner_id, repository_id, branch_id);
      CREATE INDEX IF NOT EXISTS locks_repository ON locks(repository_id, branch_id, description);
      INSERT INTO schema_version(version) VALUES (1);
    `);
  }
  if (current < 2) {
    sql.exec(`
      CREATE TABLE IF NOT EXISTS lock_recovery_audit (
        event_id TEXT PRIMARY KEY,
        actor_id TEXT NOT NULL,
        expected_owner_id TEXT NOT NULL,
        repository_id TEXT NOT NULL,
        resources_json TEXT NOT NULL,
        recorded_at INTEGER NOT NULL
      );
      CREATE INDEX IF NOT EXISTS lock_recovery_audit_repository_time
        ON lock_recovery_audit(repository_id, recorded_at DESC, event_id DESC);
      INSERT INTO schema_version(version) VALUES (2);
    `);
  }
}

function deduplicate(resources: readonly LockResourceDto[]): LockResourceDto[] {
  const unique = new Map<string, LockResourceDto>();
  for (const resource of resources) {
    unique.set(`${resource.hash}:${resource.branch}`, resource);
  }
  return [...unique.values()].sort((left, right) =>
    `${left.hash}:${left.branch}`.localeCompare(
      `${right.hash}:${right.branch}`,
    ),
  );
}

function lockFromRow(row: LockRow): LockDataDto {
  return {
    resource: {
      branch: row.branch_id,
      hash: row.hash,
      description: row.description,
    },
    owner: row.owner_id,
    lockedAt: row.locked_at,
  };
}

function auditFromRow(row: LockRecoveryAuditRow): LockRecoveryAuditDto {
  return {
    eventId: row.event_id,
    actor: row.actor_id,
    expectedOwner: row.expected_owner_id,
    repository: row.repository_id,
    resources: JSON.parse(row.resources_json) as LockResourceDto[],
    recordedAt: row.recorded_at,
  };
}

function querySql(
  query: LockQueryDto,
): readonly [string, readonly (string | number)[]] {
  switch (query.kind) {
    case "hash":
      return ["hash = ?", [query.hash]];
    case "hashRepository":
      return ["hash = ? AND repository_id = ?", [query.hash, query.repository]];
    case "hashRepositoryBranch":
      return [
        "hash = ? AND repository_id = ? AND branch_id = ?",
        [query.hash, query.repository, query.branch],
      ];
    case "owner":
      return ["owner_id = ?", [query.owner]];
    case "ownerRepository":
      return [
        "owner_id = ? AND repository_id = ?",
        [query.owner, query.repository],
      ];
    case "ownerRepositoryBranch":
      return [
        "owner_id = ? AND repository_id = ? AND branch_id = ?",
        [query.owner, query.repository, query.branch],
      ];
    case "repository":
      return ["repository_id = ?", [query.repository]];
    case "repositoryBranch":
      return [
        "repository_id = ? AND branch_id = ?",
        [query.repository, query.branch],
      ];
    case "repositoryBranchDescription":
      return [
        "repository_id = ? AND branch_id = ? AND description = ?",
        [query.repository, query.branch, query.description],
      ];
  }
}
