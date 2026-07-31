// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

export type StoreMatch = 0 | 1 | 2 | 3;

export interface AddressDto {
  readonly hash: string;
  readonly context: string;
}

export interface FragmentDto {
  readonly flags: number;
  readonly sizePayload: number;
  readonly sizeContent: number;
}

export interface QueryResultDto {
  readonly matchMade: StoreMatch;
  readonly fragment?: FragmentDto;
}

export interface LockResourceDto {
  readonly branch: string;
  readonly hash: string;
  readonly description: string;
}

export interface LockDataDto {
  readonly resource: LockResourceDto;
  readonly owner: string;
  readonly lockedAt: number;
}

export type LockQueryDto =
  | { readonly kind: "hash"; readonly hash: string }
  | { readonly kind: "hashRepository"; readonly hash: string; readonly repository: string }
  | { readonly kind: "hashRepositoryBranch"; readonly hash: string; readonly repository: string; readonly branch: string }
  | { readonly kind: "owner"; readonly owner: string }
  | { readonly kind: "ownerRepository"; readonly owner: string; readonly repository: string }
  | { readonly kind: "ownerRepositoryBranch"; readonly owner: string; readonly repository: string; readonly branch: string }
  | { readonly kind: "repository"; readonly repository: string }
  | { readonly kind: "repositoryBranch"; readonly repository: string; readonly branch: string }
  | { readonly kind: "repositoryBranchDescription"; readonly repository: string; readonly branch: string; readonly description: string };

export interface ApiErrorBody {
  readonly error: "invalid_request" | "not_found" | "conflict" | "slow_down" | "internal";
  readonly message: string;
}
