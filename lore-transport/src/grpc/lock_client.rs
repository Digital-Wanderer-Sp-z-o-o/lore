// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use lore_base::lore_debug;
use lore_base::types::Context;
use lore_base::types::LockData;
use lore_base::types::LockRecoveryAuditCursor;
use lore_base::types::LockRecoveryAuditEntry;
use lore_base::types::LockRecoveryAuditPage;
use lore_base::types::LockRecoveryAuditQuery;
use lore_base::types::LockResource;
use lore_base::types::RepositoryId;
use lore_proto::lock::AdminLockRequest;
use lore_proto::lock::LockRequest;
use lore_proto::lock::QueryRequest;
use lore_proto::lock::RecoveryAuditCursor;
use lore_proto::lock::RecoveryAuditRequest;
use lore_proto::lock::RecoveryAuditResponse;
use lore_proto::lock::StatusRequest;
use lore_proto::lock::UnlockRequest;
use lore_proto::lock::lock_service_client::LockServiceClient;
use tonic::Code;
use uuid::Uuid;

use super::AuthorizedService;
use super::AuthzInterceptor;
use super::Channel;
use super::GRPCAuthRef;
use super::RequestScopedCounter;
use super::grpc_retry;
use super::handle_error;
use crate::error::ProtocolError;

#[derive(Debug, Clone)]
pub struct LockService {
    client: LockServiceClient<AuthorizedService>,
    pub request_inflight: Arc<AtomicU64>,
}

impl LockService {
    pub fn new(channel: Channel, repository: RepositoryId, auth: GRPCAuthRef) -> Self {
        let client =
            LockServiceClient::with_interceptor(channel, AuthzInterceptor { repository, auth })
                .max_decoding_message_size(32 * 1024 * 1024); // 32MiB

        Self {
            client,
            request_inflight: Arc::new(AtomicU64::new(0)),
        }
    }

    pub async fn lock(
        &self,
        resources: &[LockResource],
        owner: Option<&str>,
    ) -> Result<Vec<LockData>, ProtocolError> {
        lore_debug!("Locking resources");

        let _ = RequestScopedCounter::new(self.request_inflight.clone());

        let mut retry = grpc_retry();
        let locks = loop {
            let resources = resources.iter().map(Into::into).collect();

            if let Some(owner) = owner {
                let request = AdminLockRequest {
                    resources,
                    owner: owner.to_string(),
                };

                let mut client = self.client.clone();
                match client.admin_lock(request).await {
                    Ok(response) => {
                        break response.into_inner().locks;
                    }
                    Err(status) => handle_error(&mut retry, status).await?,
                }
            } else {
                let request = LockRequest { resources };

                let mut client = self.client.clone();
                match client.lock(request).await {
                    Ok(response) => {
                        break response.into_inner().locks;
                    }
                    Err(status) => handle_error(&mut retry, status).await?,
                }
            }
        };

        Ok(locks.into_iter().map(Into::into).collect())
    }

    pub async fn query(
        &self,
        branch: Option<Context>,
        owner: Option<&str>,
        description: Option<&str>,
    ) -> Result<Vec<LockData>, ProtocolError> {
        lore_debug!("Querying resources");

        let _ = RequestScopedCounter::new(self.request_inflight.clone());

        let mut retry = grpc_retry();
        let locks = loop {
            let request = QueryRequest {
                branch: branch.map(Context::into),
                owner: owner.map(str::to_string),
                description: description.map(str::to_string),
            };

            let mut client = self.client.clone();
            match client.query(request).await {
                Ok(response) => {
                    break response.into_inner().result;
                }
                Err(status) => handle_error(&mut retry, status).await?,
            }
        };

        Ok(locks.into_iter().map(Into::into).collect())
    }

    pub async fn status(&self, resources: &[LockResource]) -> Result<Vec<LockData>, ProtocolError> {
        lore_debug!("Fetching resource lock status");

        let _ = RequestScopedCounter::new(self.request_inflight.clone());

        let mut retry = grpc_retry();
        let locks = loop {
            let request = StatusRequest {
                resources: resources.iter().map(Into::into).collect(),
            };

            let mut client = self.client.clone();

            match client.status(request).await {
                Ok(response) => {
                    break response.into_inner().locks;
                }
                Err(status) => handle_error(&mut retry, status).await?,
            }
        };

        Ok(locks.into_iter().map(Into::into).collect())
    }

    pub async fn unlock(
        &self,
        resources: &[LockResource],
        expected_owner: Option<&str>,
    ) -> Result<Vec<LockResource>, ProtocolError> {
        lore_debug!("Releasing resources");

        let _ = RequestScopedCounter::new(self.request_inflight.clone());

        let mut retry = grpc_retry();
        let resources = loop {
            let request = UnlockRequest {
                resources: resources.iter().map(Into::into).collect(),
                expected_owner: expected_owner.map(str::to_owned),
            };

            let mut client = self.client.clone();

            match client.unlock(request).await {
                Ok(response) => {
                    break response.into_inner().resources;
                }
                Err(status) => {
                    if status.code() == Code::NotFound {
                        return Ok(vec![]);
                    }
                    handle_error(&mut retry, status).await?;
                }
            }
        };

        Ok(resources.into_iter().map(Into::into).collect())
    }

    pub async fn query_recovery_audit(
        &self,
        query: &LockRecoveryAuditQuery,
    ) -> Result<LockRecoveryAuditPage, ProtocolError> {
        lore_debug!("Querying administrative lock recovery audit");

        let _ = RequestScopedCounter::new(self.request_inflight.clone());
        let mut retry = grpc_retry();
        let response = loop {
            let request = RecoveryAuditRequest {
                limit: query.limit(),
                cursor: query.cursor().map(recovery_audit_cursor_proto),
            };
            let mut client = self.client.clone();
            match client.query_recovery_audit(request).await {
                Ok(response) => break response.into_inner(),
                Err(status) => handle_error(&mut retry, status).await?,
            }
        };
        recovery_audit_page(response)
    }
}

fn recovery_audit_cursor_proto(cursor: &LockRecoveryAuditCursor) -> RecoveryAuditCursor {
    RecoveryAuditCursor {
        event_id: cursor.event_id().as_bytes().to_vec().into(),
        recorded_at: Some(timestamp_proto(cursor.recorded_at())),
    }
}

fn recovery_audit_page(
    response: RecoveryAuditResponse,
) -> Result<LockRecoveryAuditPage, ProtocolError> {
    let entries = response
        .events
        .into_iter()
        .map(|event| {
            let event_id = Uuid::from_slice(&event.event_id)
                .map_err(|_| invalid_audit_response("event ID was not a UUID"))?;
            LockRecoveryAuditEntry::try_new(
                event_id,
                event.actor,
                event.expected_owner,
                event.resources.into_iter().map(Into::into).collect(),
                timestamp_millis(event.recorded_at.as_ref(), "event")?,
            )
            .map_err(|error| invalid_audit_response(&error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let next_cursor = response
        .next_cursor
        .map(|cursor| -> Result<LockRecoveryAuditCursor, ProtocolError> {
            let event_id = Uuid::from_slice(&cursor.event_id)
                .map_err(|_| invalid_audit_response("cursor event ID was not a UUID"))?;
            Ok(LockRecoveryAuditCursor::new(
                event_id,
                timestamp_millis(cursor.recorded_at.as_ref(), "cursor")?,
            ))
        })
        .transpose()?;
    Ok(LockRecoveryAuditPage::new(entries, next_cursor))
}

fn timestamp_proto(milliseconds: u64) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: (milliseconds / 1_000) as i64,
        nanos: ((milliseconds % 1_000) * 1_000_000) as i32,
    }
}

fn timestamp_millis(
    timestamp: Option<&prost_types::Timestamp>,
    field: &str,
) -> Result<u64, ProtocolError> {
    let timestamp = timestamp
        .ok_or_else(|| invalid_audit_response(&format!("{field} timestamp was missing")))?;
    if timestamp.seconds < 0 || !(0..1_000_000_000).contains(&timestamp.nanos) {
        return Err(invalid_audit_response(&format!(
            "{field} timestamp was invalid"
        )));
    }
    let seconds = u64::try_from(timestamp.seconds)
        .map_err(|_| invalid_audit_response(&format!("{field} timestamp was invalid")))?;
    seconds
        .checked_mul(1_000)
        .and_then(|value| value.checked_add(timestamp.nanos as u64 / 1_000_000))
        .ok_or_else(|| invalid_audit_response(&format!("{field} timestamp was out of range")))
}

fn invalid_audit_response(message: &str) -> ProtocolError {
    ProtocolError::internal(format!("invalid lock recovery audit response: {message}"))
}
