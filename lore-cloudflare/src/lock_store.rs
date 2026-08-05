// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

use async_trait::async_trait;
use lore_base::error::{InvalidArguments, LockNotFound, LockNotOwned, SlowDown};
use lore_base::types::{BranchId, Hash, LockData, LockResource, RepositoryId};
use lore_revision::lock::{LockError, LockQuery, LockStore};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::{CloudflareClient, CloudflareClientError};

pub struct CloudflareLockStore {
    client: CloudflareClient,
}

impl CloudflareLockStore {
    pub fn new(client: CloudflareClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl LockStore for CloudflareLockStore {
    async fn lock_resources(
        &self,
        owner_id: &str,
        repository: RepositoryId,
        resources: &[LockResource],
    ) -> Result<Vec<LockData>, LockError> {
        let response: LockMutationResponse = self
            .client
            .post(
                "/v1/locks/acquire",
                &AcquireRequest {
                    owner: owner_id,
                    repository,
                    resources: resources.iter().map(LockResourceDto::from).collect(),
                },
            )
            .await
            .map_err(lock_error)?;
        Ok(response
            .locks
            .unwrap_or_default()
            .into_iter()
            .map(LockData::from)
            .collect())
    }

    async fn query_locks(&self, query: LockQuery) -> Result<Vec<LockData>, LockError> {
        ensure_shardable_query(&query)?;
        let response: LocksResponse = self
            .client
            .post(
                "/v1/locks/query",
                &QueryRequest {
                    query: LockQueryDto::from(query),
                },
            )
            .await
            .map_err(lock_error)?;
        Ok(response.locks.into_iter().map(LockData::from).collect())
    }

    async fn check_locks_status(
        &self,
        repository: RepositoryId,
        resources: &[LockResource],
    ) -> Result<Vec<LockData>, LockError> {
        let response: LocksResponse = self
            .client
            .post(
                "/v1/locks/status",
                &StatusRequest {
                    repository,
                    resources: resources.iter().map(LockResourceDto::from).collect(),
                },
            )
            .await
            .map_err(lock_error)?;
        Ok(response.locks.into_iter().map(LockData::from).collect())
    }

    async fn unlock_resources(
        &self,
        actor_id: &str,
        expected_owner_id: &str,
        repository: RepositoryId,
        resources: &[LockResource],
    ) -> Result<Vec<LockResource>, LockError> {
        let response: LockMutationResponse = self
            .client
            .post(
                "/v1/locks/release",
                &ReleaseRequest {
                    actor: actor_id,
                    expected_owner: expected_owner_id,
                    repository,
                    resources: resources.iter().map(LockResourceDto::from).collect(),
                },
            )
            .await
            .map_err(lock_error)?;
        Ok(response
            .resources
            .unwrap_or_default()
            .into_iter()
            .map(LockResource::from)
            .collect())
    }
}

fn ensure_shardable_query(query: &LockQuery) -> Result<(), LockError> {
    if matches!(query, LockQuery::Hash(_) | LockQuery::Owner(_)) {
        Err(InvalidArguments {
            reason:
                "global lock queries are not supported by the repository-sharded Cloudflare backend"
                    .into(),
        }
        .into())
    } else {
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct LockResourceDto {
    branch: BranchId,
    hash: Hash,
    description: String,
}

impl From<&LockResource> for LockResourceDto {
    fn from(value: &LockResource) -> Self {
        Self {
            branch: value.branch,
            hash: value.hash,
            description: value.description.clone(),
        }
    }
}

impl From<LockResourceDto> for LockResource {
    fn from(value: LockResourceDto) -> Self {
        Self {
            branch: value.branch,
            hash: value.hash,
            description: value.description,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LockDataDto {
    resource: LockResourceDto,
    owner: String,
    locked_at: u64,
}

impl From<LockDataDto> for LockData {
    fn from(value: LockDataDto) -> Self {
        Self {
            resource: value.resource.into(),
            owner: value.owner,
            locked_at: value.locked_at,
        }
    }
}

#[derive(Serialize)]
struct AcquireRequest<'a> {
    owner: &'a str,
    repository: RepositoryId,
    resources: Vec<LockResourceDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseRequest<'a> {
    actor: &'a str,
    expected_owner: &'a str,
    repository: RepositoryId,
    resources: Vec<LockResourceDto>,
}

#[derive(Serialize)]
struct StatusRequest {
    repository: RepositoryId,
    resources: Vec<LockResourceDto>,
}

#[derive(Serialize)]
struct QueryRequest {
    query: LockQueryDto,
}

#[derive(Deserialize)]
struct LocksResponse {
    locks: Vec<LockDataDto>,
}

#[derive(Deserialize)]
struct LockMutationResponse {
    #[allow(dead_code)]
    status: String,
    locks: Option<Vec<LockDataDto>>,
    resources: Option<Vec<LockResourceDto>>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum LockQueryDto {
    Hash {
        hash: Hash,
    },
    HashRepository {
        hash: Hash,
        repository: RepositoryId,
    },
    HashRepositoryBranch {
        hash: Hash,
        repository: RepositoryId,
        branch: BranchId,
    },
    Owner {
        owner: String,
    },
    OwnerRepository {
        owner: String,
        repository: RepositoryId,
    },
    OwnerRepositoryBranch {
        owner: String,
        repository: RepositoryId,
        branch: BranchId,
    },
    Repository {
        repository: RepositoryId,
    },
    RepositoryBranch {
        repository: RepositoryId,
        branch: BranchId,
    },
    RepositoryBranchDescription {
        repository: RepositoryId,
        branch: BranchId,
        description: String,
    },
}

impl From<LockQuery> for LockQueryDto {
    fn from(value: LockQuery) -> Self {
        match value {
            LockQuery::Hash(hash) => Self::Hash { hash },
            LockQuery::HashRepository(hash, repository) => {
                Self::HashRepository { hash, repository }
            }
            LockQuery::HashRepositoryBranch(hash, repository, branch) => {
                Self::HashRepositoryBranch {
                    hash,
                    repository,
                    branch,
                }
            }
            LockQuery::Owner(owner) => Self::Owner { owner },
            LockQuery::OwnerRepository(owner, repository) => {
                Self::OwnerRepository { owner, repository }
            }
            LockQuery::OwnerRepositoryBranch(owner, repository, branch) => {
                Self::OwnerRepositoryBranch {
                    owner,
                    repository,
                    branch,
                }
            }
            LockQuery::Repository(repository) => Self::Repository { repository },
            LockQuery::RepositoryBranch(repository, branch) => {
                Self::RepositoryBranch { repository, branch }
            }
            LockQuery::RepositoryBranchDescription(repository, branch, description) => {
                Self::RepositoryBranchDescription {
                    repository,
                    branch,
                    description,
                }
            }
        }
    }
}

fn lock_error(error: CloudflareClientError) -> LockError {
    match &error {
        CloudflareClientError::Response { status, message } if *status == StatusCode::NOT_FOUND => {
            LockNotFound.into()
        }
        CloudflareClientError::Response { status, message }
            if *status == StatusCode::CONFLICT && message.contains("owned") =>
        {
            LockNotOwned.into()
        }
        CloudflareClientError::Response { status, .. }
            if status.is_server_error() || *status == StatusCode::TOO_MANY_REQUESTS =>
        {
            SlowDown.into()
        }
        CloudflareClientError::Transport(_) => SlowDown.into(),
        _ => LockError::internal(format!("Cloudflare lock store operation failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_shards_reject_global_lock_queries_before_http() {
        assert!(ensure_shardable_query(&LockQuery::Hash(Hash::default())).is_err());
        assert!(ensure_shardable_query(&LockQuery::Owner("artist".to_owned())).is_err());
    }
}
