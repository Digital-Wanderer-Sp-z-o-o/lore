// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use lore_base::lore_debug;
use lore_base::types::Address;
use lore_base::types::RepositoryId;
use lore_base::types::{
    ObliterationAuditCursor, ObliterationAuditEntry, ObliterationAuditEntryData,
    ObliterationAuditPage, ObliterationAuditQuery, ObliterationAuditStatus,
};
use lore_proto::AdminServiceClient;
use lore_proto::ObliterateRequest;
#[cfg(test)]
use lore_proto::rpc::ObliterationAuditEntry as ObliterationAuditEntryProto;
use lore_proto::rpc::{
    ObliterationAuditCursor as ObliterationAuditCursorProto,
    ObliterationAuditStatus as ObliterationAuditStatusProto, QueryObliterationAuditRequest,
    QueryObliterationAuditResponse,
};

use super::AuthorizedService;
use super::AuthzInterceptor;
use super::Channel;
use super::GRPCAuthRef;
use super::RequestScopedCounter;
use super::grpc_retry;
use super::handle_error;
use crate::error::ProtocolError;

#[derive(Clone)]
pub struct AdminService {
    client: AdminServiceClient<AuthorizedService>,
    repository: RepositoryId,
    pub request_inflight: Arc<AtomicU64>,
}

impl AdminService {
    pub fn new(channel: Channel, repository: RepositoryId, auth: GRPCAuthRef) -> Self {
        let client =
            AdminServiceClient::with_interceptor(channel, AuthzInterceptor { repository, auth });

        Self {
            client,
            repository,
            request_inflight: Arc::new(AtomicU64::new(0)),
        }
    }

    pub async fn obliterate(&self, address: Address) -> Result<(), ProtocolError> {
        lore_debug!("Initiating remote obliterate for address {address}");

        let mut retry = grpc_retry();
        let _response = loop {
            let _ = RequestScopedCounter::new(self.request_inflight.clone());

            let request = ObliterateRequest {
                address: Some(address.into()),
            };

            let mut client = self.client.clone();

            match client.obliterate(request).await {
                Ok(response) => {
                    break response.into_inner();
                }
                Err(status) => {
                    handle_error(&mut retry, status).await?;
                }
            }
        };

        Ok(())
    }

    pub async fn query_obliteration_audit(
        &self,
        address: Address,
        query: &ObliterationAuditQuery,
    ) -> Result<ObliterationAuditPage, ProtocolError> {
        let _ = RequestScopedCounter::new(self.request_inflight.clone());
        let mut retry = grpc_retry();
        let response = loop {
            let request = QueryObliterationAuditRequest {
                address: Some(address.into()),
                limit: query.limit(),
                cursor: query.cursor().map(cursor_proto),
            };
            let mut client = self.client.clone();
            match client.query_obliteration_audit(request).await {
                Ok(response) => break response.into_inner(),
                Err(status) => handle_error(&mut retry, status).await?,
            }
        };
        audit_page(self.repository, address, query.limit(), response)
    }
}

fn cursor_proto(cursor: &ObliterationAuditCursor) -> ObliterationAuditCursorProto {
    ObliterationAuditCursorProto {
        event_id: cursor.event_id().as_bytes().to_vec().into(),
        recorded_at: Some(timestamp_proto(cursor.recorded_at())),
    }
}

fn audit_page(
    repository: RepositoryId,
    requested_address: Address,
    requested_limit: u32,
    response: QueryObliterationAuditResponse,
) -> Result<ObliterationAuditPage, ProtocolError> {
    if response.events.len() > requested_limit as usize {
        return Err(invalid_audit_response(
            "response exceeded the requested page size",
        ));
    }
    let entries = response
        .events
        .into_iter()
        .map(|event| {
            let event_id = uuid::Uuid::from_slice(&event.event_id)
                .map_err(|_| invalid_audit_response("event ID was not a UUID"))?;
            let address = event
                .address
                .map(Address::from)
                .ok_or_else(|| invalid_audit_response("event address was missing"))?;
            if address != requested_address {
                return Err(invalid_audit_response(
                    "event address differed from the requested address",
                ));
            }
            ObliterationAuditEntry::try_new(ObliterationAuditEntryData {
                event_id,
                actor_id: event.actor,
                correlation_id: event.correlation_id,
                repository,
                address,
                status: status_from_proto(event.status)?,
                remaining_associations: event.remaining_associations,
                recorded_at: timestamp_millis(event.recorded_at.as_ref(), "event")?,
                completed_at: event
                    .completed_at
                    .as_ref()
                    .map(|value| timestamp_millis(Some(value), "completed event"))
                    .transpose()?,
            })
            .map_err(|error| invalid_audit_response(&error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !entries.windows(2).all(|pair| {
        (pair[0].recorded_at(), pair[0].event_id()) > (pair[1].recorded_at(), pair[1].event_id())
    }) {
        return Err(invalid_audit_response(
            "events were not in stable newest-first order",
        ));
    }
    let next_cursor = response
        .next_cursor
        .map(|cursor| -> Result<ObliterationAuditCursor, ProtocolError> {
            let event_id = uuid::Uuid::from_slice(&cursor.event_id)
                .map_err(|_| invalid_audit_response("cursor event ID was not a UUID"))?;
            Ok(ObliterationAuditCursor::new(
                event_id,
                timestamp_millis(cursor.recorded_at.as_ref(), "cursor")?,
            ))
        })
        .transpose()?;
    if let Some(cursor) = next_cursor.as_ref() {
        let Some(last) = entries.last() else {
            return Err(invalid_audit_response(
                "response included a cursor without events",
            ));
        };
        if cursor.event_id() != last.event_id() || cursor.recorded_at() != last.recorded_at() {
            return Err(invalid_audit_response(
                "cursor did not identify the final page event",
            ));
        }
    }
    Ok(ObliterationAuditPage::new(entries, next_cursor))
}

fn status_from_proto(value: i32) -> Result<ObliterationAuditStatus, ProtocolError> {
    match ObliterationAuditStatusProto::try_from(value) {
        Ok(ObliterationAuditStatusProto::AssociationPending) => {
            Ok(ObliterationAuditStatus::AssociationPending)
        }
        Ok(ObliterationAuditStatusProto::AssociationRemoved) => {
            Ok(ObliterationAuditStatus::AssociationRemoved)
        }
        Ok(ObliterationAuditStatusProto::PayloadRetained) => {
            Ok(ObliterationAuditStatus::PayloadRetained)
        }
        Ok(ObliterationAuditStatusProto::PayloadObliterated) => {
            Ok(ObliterationAuditStatus::PayloadObliterated)
        }
        Ok(ObliterationAuditStatusProto::Unspecified) | Err(_) => {
            Err(invalid_audit_response("event status was invalid"))
        }
    }
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
    u64::try_from(timestamp.seconds)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000))
        .and_then(|millis| millis.checked_add((timestamp.nanos as u64) / 1_000_000))
        .ok_or_else(|| invalid_audit_response(&format!("{field} timestamp was out of range")))
}

fn invalid_audit_response(message: &str) -> ProtocolError {
    ProtocolError::internal(format!("invalid obliteration audit response: {message}"))
}

#[cfg(test)]
mod obliteration_audit_tests {
    use rand::random;

    use super::*;

    fn completed_event(address: Address, recorded_at: u64) -> ObliterationAuditEntryProto {
        ObliterationAuditEntryProto {
            event_id: uuid::Uuid::new_v4().as_bytes().to_vec().into(),
            actor: "owner".into(),
            correlation_id: "correlation".into(),
            address: Some(address.into()),
            status: ObliterationAuditStatusProto::PayloadObliterated as i32,
            remaining_associations: Some(0),
            recorded_at: Some(timestamp_proto(recorded_at)),
            completed_at: Some(timestamp_proto(recorded_at + 1)),
        }
    }

    #[test]
    fn accepts_a_stable_page_with_a_matching_cursor() {
        let repository = random::<RepositoryId>();
        let address = random::<Address>();
        let older = completed_event(address, 10);
        let cursor = ObliterationAuditCursorProto {
            event_id: older.event_id.clone(),
            recorded_at: older.recorded_at,
        };
        let page = audit_page(
            repository,
            address,
            2,
            QueryObliterationAuditResponse {
                events: vec![completed_event(address, 20), older],
                next_cursor: Some(cursor),
            },
        )
        .expect("valid audit response should pass");
        assert_eq!(page.entries().len(), 2);
        assert!(page.next_cursor().is_some());
    }

    #[test]
    fn rejects_oversized_or_unstable_pages() {
        let repository = random::<RepositoryId>();
        let address = random::<Address>();
        assert!(
            audit_page(
                repository,
                address,
                1,
                QueryObliterationAuditResponse {
                    events: vec![completed_event(address, 20), completed_event(address, 10)],
                    next_cursor: None,
                },
            )
            .is_err()
        );
        assert!(
            audit_page(
                repository,
                address,
                2,
                QueryObliterationAuditResponse {
                    events: vec![completed_event(address, 10), completed_event(address, 20)],
                    next_cursor: None,
                },
            )
            .is_err()
        );
    }
}
