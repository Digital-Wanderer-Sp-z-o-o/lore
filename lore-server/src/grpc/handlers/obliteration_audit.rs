// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::types::{
    Address, ObliterationAuditCursor, ObliterationAuditEntry, ObliterationAuditQuery,
    ObliterationAuditStatus,
};
use lore_proto::rpc::{
    ObliterationAuditCursor as ObliterationAuditCursorProto,
    ObliterationAuditEntry as ObliterationAuditEntryProto,
    ObliterationAuditStatus as ObliterationAuditStatusProto, QueryObliterationAuditRequest,
    QueryObliterationAuditResponse,
};
use lore_storage::StoreError;
use tonic::metadata::MetadataMap;
use tonic::{Request, Response, Status};
use tracing::warn;

use crate::auth::jwt::{AuthorizationToken, JwtVerifier};
use crate::auth::jwt_interceptor::extract_bearer_token;
use crate::grpc::{extract_correlation_id, get_repository, get_user_id, is_owner_or_admin};
use crate::util::setup_execution;

async fn authenticate_request(
    metadata: &MetadataMap,
    jwt_verifier: &JwtVerifier,
) -> Result<AuthorizationToken, Status> {
    let token = extract_bearer_token(metadata)
        .ok_or_else(|| Status::unauthenticated("authorization header required"))?;
    jwt_verifier
        .verify_token(&token)
        .await
        .map_err(|error| Status::unauthenticated(format!("invalid token ({error:?})")))
}

pub async fn handler(
    mut request: Request<QueryObliterationAuditRequest>,
    immutable_store: Arc<dyn lore_storage::ImmutableStore>,
    jwt_verifier: &Arc<Option<JwtVerifier>>,
) -> Result<Response<QueryObliterationAuditResponse>, Status> {
    if let Some(verifier) = &**jwt_verifier {
        let authorization = authenticate_request(request.metadata(), verifier).await?;
        request.extensions_mut().insert(authorization);
    }

    let repository = get_repository(request.metadata())?;
    authorize_audit_read(jwt_verifier.is_some(), request.extensions(), repository)?;
    let user_id = get_user_id(request.extensions());
    let correlation_id = extract_correlation_id(&request).unwrap_or_default();
    let input = request.into_inner();
    let address = input
        .address
        .map(Address::from)
        .ok_or_else(|| Status::invalid_argument("audit address is required"))?;
    let query = audit_query(input.limit, input.cursor)?;
    let execution = setup_execution(module_path!(), correlation_id, user_id);

    LORE_CONTEXT
        .scope(execution, async move {
            let page = immutable_store
                .query_obliteration_audit(repository, address, &query)
                .await
                .map_err(map_store_error)?;
            Ok(Response::new(QueryObliterationAuditResponse {
                events: page.entries().iter().map(entry_proto).collect(),
                next_cursor: page.next_cursor().map(cursor_proto),
            }))
        })
        .await
}

fn authorize_audit_read(
    authentication_enabled: bool,
    extensions: &http::Extensions,
    repository: lore_base::types::RepositoryId,
) -> Result<(), Status> {
    if authentication_enabled && !is_owner_or_admin(extensions, repository) {
        Err(Status::permission_denied(
            "Only a repository administrator can read obliteration audit history",
        ))
    } else {
        Ok(())
    }
}

fn audit_query(
    limit: u32,
    cursor: Option<ObliterationAuditCursorProto>,
) -> Result<ObliterationAuditQuery, Status> {
    let cursor = cursor
        .map(|cursor| -> Result<ObliterationAuditCursor, Status> {
            let event_id = uuid::Uuid::from_slice(&cursor.event_id)
                .map_err(|_| Status::invalid_argument("audit cursor event ID must be a UUID"))?;
            Ok(ObliterationAuditCursor::new(
                event_id,
                timestamp_millis(cursor.recorded_at.as_ref(), "audit cursor")?,
            ))
        })
        .transpose()?;
    ObliterationAuditQuery::try_new(limit, cursor)
        .map_err(|error| Status::invalid_argument(error.to_string()))
}

fn entry_proto(entry: &ObliterationAuditEntry) -> ObliterationAuditEntryProto {
    ObliterationAuditEntryProto {
        event_id: entry.event_id().as_bytes().to_vec().into(),
        actor: entry.actor_id().to_owned(),
        correlation_id: entry.correlation_id().to_owned(),
        address: Some(entry.address().into()),
        status: status_proto(entry.status()) as i32,
        remaining_associations: entry.remaining_associations(),
        recorded_at: Some(timestamp_proto(entry.recorded_at())),
        completed_at: entry.completed_at().map(timestamp_proto),
    }
}

fn status_proto(status: ObliterationAuditStatus) -> ObliterationAuditStatusProto {
    match status {
        ObliterationAuditStatus::AssociationPending => {
            ObliterationAuditStatusProto::AssociationPending
        }
        ObliterationAuditStatus::AssociationRemoved => {
            ObliterationAuditStatusProto::AssociationRemoved
        }
        ObliterationAuditStatus::PayloadRetained => ObliterationAuditStatusProto::PayloadRetained,
        ObliterationAuditStatus::PayloadObliterated => {
            ObliterationAuditStatusProto::PayloadObliterated
        }
    }
}

fn cursor_proto(cursor: &ObliterationAuditCursor) -> ObliterationAuditCursorProto {
    ObliterationAuditCursorProto {
        event_id: cursor.event_id().as_bytes().to_vec().into(),
        recorded_at: Some(timestamp_proto(cursor.recorded_at())),
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
) -> Result<u64, Status> {
    let timestamp = timestamp
        .ok_or_else(|| Status::invalid_argument(format!("{field} timestamp is required")))?;
    if timestamp.seconds < 0 || !(0..1_000_000_000).contains(&timestamp.nanos) {
        return Err(Status::invalid_argument(format!(
            "{field} timestamp is invalid"
        )));
    }
    u64::try_from(timestamp.seconds)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000))
        .and_then(|millis| millis.checked_add((timestamp.nanos as u64) / 1_000_000))
        .ok_or_else(|| Status::invalid_argument(format!("{field} timestamp is out of range")))
}

fn map_store_error(error: StoreError) -> Status {
    warn!(?error, "Failed to query obliteration audit");
    if error.is_not_supported() {
        Status::unimplemented(error.to_string())
    } else if error.is_slow_down() || error.is_disconnected() {
        Status::unavailable(error.to_string())
    } else {
        Status::internal(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use lore_base::types::RepositoryId;
    use rand::random;
    use tonic::Code;

    use super::*;
    use crate::auth::jwt::ResourcePermission;

    fn extensions_with_permissions(
        repository: RepositoryId,
        permissions: &[&str],
    ) -> http::Extensions {
        let mut extensions = http::Extensions::new();
        extensions.insert(AuthorizationToken {
            resources: Some(vec![ResourcePermission {
                resource_id: format!("urc-{repository}"),
                permission: permissions.iter().map(ToString::to_string).collect(),
            }]),
            ..Default::default()
        });
        extensions
    }

    #[test]
    fn ordinary_repository_member_cannot_read_audit() {
        let repository = random::<RepositoryId>();
        let extensions = extensions_with_permissions(repository, &["read", "write"]);
        let error = authorize_audit_read(true, &extensions, repository).unwrap_err();
        assert_eq!(error.code(), Code::PermissionDenied);
    }

    #[test]
    fn repository_admin_can_read_audit() {
        let repository = random::<RepositoryId>();
        let extensions = extensions_with_permissions(repository, &["admin"]);
        assert!(authorize_audit_read(true, &extensions, repository).is_ok());
    }

    #[test]
    fn audit_cursor_requires_a_valid_timestamp_and_uuid() {
        let invalid_uuid = ObliterationAuditCursorProto {
            event_id: vec![1, 2, 3].into(),
            recorded_at: Some(timestamp_proto(1)),
        };
        assert_eq!(
            audit_query(1, Some(invalid_uuid)).unwrap_err().code(),
            Code::InvalidArgument
        );
        assert_eq!(
            audit_query(0, None).unwrap_err().code(),
            Code::InvalidArgument
        );
    }
}
