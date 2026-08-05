// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

import { DurableObject } from "cloudflare:workers";
import type { LockDataDto, LockQueryDto, LockResourceDto } from "./contracts";

interface LockRow extends Record<string, SqlStorageValue> {
  readonly hash: string;
  readonly repository_id: string;
  readonly branch_id: string;
  readonly description: string;
  readonly owner_id: string;
  readonly locked_at: number;
}

export interface LockMutationResult {
  readonly status: "ok" | "not_owned" | "not_found";
  readonly locks?: readonly LockDataDto[];
  readonly resources?: readonly LockResourceDto[];
}

export class LockCoordinator extends DurableObject<Cloudflare.Env> {
  public constructor(state: DurableObjectState, env: Cloudflare.Env) {
    super(state, env);
    state.blockConcurrencyWhile(async () => {
      this.ctx.storage.sql.exec(`
        CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);
        INSERT OR IGNORE INTO schema_version(version) VALUES (1);
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
      `);
    });
  }

  public lockResources(
    owner: string,
    repository: string,
    resources: readonly LockResourceDto[],
    lockedAt: number,
    leaseDurationMs: number,
  ): LockMutationResult {
    return this.ctx.storage.transactionSync(() => {
      this.deleteExpired(lockedAt, leaseDurationMs);
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
          this.ctx.storage.sql.exec(
            "UPDATE locks SET description = ?, locked_at = ? WHERE hash = ? AND repository_id = ? AND branch_id = ? AND owner_id = ?",
            resource.description,
            lockedAt,
            resource.hash,
            repository,
            resource.branch,
            owner,
          );
        } else {
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
      }
      return { status: "ok", locks: newlyLocked };
    });
  }

  public unlockResources(
    owner: string,
    validateUser: boolean,
    repository: string,
    resources: readonly LockResourceDto[],
    now: number,
    leaseDurationMs: number,
  ): LockMutationResult {
    return this.ctx.storage.transactionSync(() => {
      this.deleteExpired(now, leaseDurationMs);
      const unique = deduplicate(resources);
      for (const resource of unique) {
        const existing = this.get(repository, resource);
        if (existing === undefined) return { status: "not_found" };
        if (validateUser && existing.owner_id !== owner)
          return { status: "not_owned" };
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
    });
  }

  public checkLocksStatus(
    repository: string,
    resources: readonly LockResourceDto[],
    now: number,
    leaseDurationMs: number,
  ): LockDataDto[] {
    this.deleteExpired(now, leaseDurationMs);
    return deduplicate(resources).flatMap((resource) => {
      const row = this.get(repository, resource);
      return row === undefined ? [] : [lockFromRow(row)];
    });
  }

  public queryLocks(
    query: LockQueryDto,
    now: number,
    leaseDurationMs: number,
  ): LockDataDto[] {
    this.deleteExpired(now, leaseDurationMs);
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
    const expiresBefore = now - leaseDurationMs;
    this.ctx.storage.sql.exec(
      "DELETE FROM locks WHERE locked_at <= ?",
      expiresBefore,
    );
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
