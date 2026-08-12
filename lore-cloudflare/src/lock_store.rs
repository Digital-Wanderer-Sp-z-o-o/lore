// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

use async_trait::async_trait;
use lore_base::error::InvalidArguments;
use lore_base::error::LockNotFound;
use lore_base::error::LockNotOwned;
use lore_base::error::SlowDown;
use lore_base::types::BranchId;
use lore_base::types::Hash;
use lore_base::types::LockData;
use lore_base::types::LockRecoveryAuditCursor;
use lore_base::types::LockRecoveryAuditEntry;
use lore_base::types::LockRecoveryAuditPage;
use lore_base::types::LockRecoveryAuditQuery;
use lore_base::types::LockResource;
use lore_base::types::RepositoryId;
use lore_revision::lock::LockError;
use lore_revision::lock::LockQuery;
use lore_revision::lock::LockStore;
use reqwest::StatusCode;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::CloudflareClient;
use crate::CloudflareClientError;

pub struct CloudflareLockStore {
    client: CloudflareClient,
}

impl CloudflareLockStore {
    pub fn new(client: CloudflareClient) -> Self {
        Self { client }
    }

    async fn release_resources(
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
        self.release_resources(actor_id, expected_owner_id, repository, resources)
            .await
    }

    async fn recover_resources(
        &self,
        actor_id: &str,
        expected_owner_id: &str,
        repository: RepositoryId,
        resources: &[LockResource],
    ) -> Result<Vec<LockResource>, LockError> {
        self.release_resources(actor_id, expected_owner_id, repository, resources)
            .await
    }

    async fn query_recovery_audit(
        &self,
        repository: RepositoryId,
        query: &LockRecoveryAuditQuery,
    ) -> Result<LockRecoveryAuditPage, LockError> {
        let response: RecoveryAuditPageDto = self
            .client
            .post(
                "/v1/locks/recovery-audit",
                &RecoveryAuditRequest {
                    repository,
                    limit: query.limit(),
                    cursor: query.cursor().map(RecoveryAuditCursorDto::from),
                },
            )
            .await
            .map_err(lock_error)?;
        recovery_audit_page(repository, query, response)
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryAuditRequest {
    repository: RepositoryId,
    limit: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<RecoveryAuditCursorDto>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryAuditCursorDto {
    recorded_at: u64,
    event_id: String,
}

impl From<&LockRecoveryAuditCursor> for RecoveryAuditCursorDto {
    fn from(value: &LockRecoveryAuditCursor) -> Self {
        Self {
            recorded_at: value.recorded_at(),
            event_id: value.event_id().to_string(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryAuditEntryDto {
    event_id: String,
    actor: String,
    expected_owner: String,
    repository: RepositoryId,
    resources: Vec<LockResourceDto>,
    recorded_at: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryAuditPageDto {
    events: Vec<RecoveryAuditEntryDto>,
    next_cursor: Option<RecoveryAuditCursorDto>,
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

fn recovery_audit_page(
    repository: RepositoryId,
    query: &LockRecoveryAuditQuery,
    response: RecoveryAuditPageDto,
) -> Result<LockRecoveryAuditPage, LockError> {
    if response.events.len() > query.limit() as usize {
        return Err(invalid_audit_response(
            "audit response exceeded the requested page size",
        ));
    }
    let mut entries = Vec::with_capacity(response.events.len());
    for event in response.events {
        if event.repository != repository {
            return Err(invalid_audit_response(
                "audit response contained a different repository",
            ));
        }
        let event_id = Uuid::parse_str(&event.event_id)
            .map_err(|_| invalid_audit_response("audit event ID was not a UUID"))?;
        entries.push(
            LockRecoveryAuditEntry::try_new(
                event_id,
                event.actor,
                event.expected_owner,
                event.resources.into_iter().map(Into::into).collect(),
                event.recorded_at,
            )
            .map_err(|error| invalid_audit_response(&error.to_string()))?,
        );
    }
    if !entries.windows(2).all(|pair| {
        (pair[0].recorded_at(), pair[0].event_id()) > (pair[1].recorded_at(), pair[1].event_id())
    }) {
        return Err(invalid_audit_response(
            "audit events were not in stable newest-first order",
        ));
    }
    let next_cursor = response
        .next_cursor
        .map(|cursor| {
            Uuid::parse_str(&cursor.event_id)
                .map(|event_id| LockRecoveryAuditCursor::new(event_id, cursor.recorded_at))
                .map_err(|_| invalid_audit_response("audit cursor event ID was not a UUID"))
        })
        .transpose()?;
    if let Some(cursor) = &next_cursor {
        let Some(last) = entries.last() else {
            return Err(invalid_audit_response(
                "audit response included a cursor without events",
            ));
        };
        if cursor.event_id() != last.event_id() || cursor.recorded_at() != last.recorded_at() {
            return Err(invalid_audit_response(
                "audit cursor did not identify the final page event",
            ));
        }
    }
    Ok(LockRecoveryAuditPage::new(entries, next_cursor))
}

fn invalid_audit_response(message: &str) -> LockError {
    LockError::internal(format!(
        "invalid Cloudflare lock recovery audit response: {message}"
    ))
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
