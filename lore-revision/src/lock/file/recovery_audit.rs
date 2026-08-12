// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use lore_base::types::LockRecoveryAuditCursor;
use lore_base::types::LockRecoveryAuditQuery;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::interface::LoreError;
use crate::interface::LoreString;
use crate::repository::RepositoryContext;

#[derive(Clone, Debug)]
pub struct RecoveryAuditOptions {
    pub limit: u32,
    pub cursor: Option<LockRecoveryAuditCursor>,
}

#[error_set]
pub enum RecoveryAuditError {
    Disconnected,
    InvalidArguments,
    SlowDown,
    NotAuthorized,
    NotAuthenticated,
    Maintenance,
    NotFound,
    NoRemote,
    NotSupported,
    InvalidPath,
    Oversized,
}

impl EventError for RecoveryAuditError {
    fn translated(&self) -> LoreError {
        LoreError::Internal
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

#[repr(C)]
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLockRecoveryAuditBeginEventData {
    pub count: u64,
    pub next_cursor_event_id: LoreString,
    pub next_cursor_recorded_at: u64,
    pub has_next_cursor: u8,
}

#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLockRecoveryAuditEntryEventData {
    pub event_id: LoreString,
    pub actor_id: LoreString,
    pub expected_owner_id: LoreString,
    pub recorded_at: u64,
    pub resource_count: u64,
}

#[repr(C)]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreLockRecoveryAuditResourceEventData {
    pub event_id: LoreString,
    pub branch: crate::lore::BranchId,
    pub path: LoreString,
}

pub async fn query(
    repository: Arc<RepositoryContext>,
    options: RecoveryAuditOptions,
) -> Result<(), RecoveryAuditError> {
    let remote = repository
        .remote()
        .await
        .forward::<RecoveryAuditError>("Unable to query lock recovery audit while offline")?;
    let query = LockRecoveryAuditQuery::try_new(options.limit, options.cursor)
        .map_err(RecoveryAuditError::from)?;
    let page = remote
        .lock(repository.id)
        .await
        .forward_with::<RecoveryAuditError, _>(|| {
            format!("Failed to connect to remote {}", remote.remote_url())
        })?
        .query_recovery_audit(&query)
        .await
        .forward::<RecoveryAuditError>("Failed to query lock recovery audit")?;

    let next_cursor = page.next_cursor();
    event::LoreEvent::LockRecoveryAuditBegin(LoreLockRecoveryAuditBeginEventData {
        count: page.entries().len() as u64,
        next_cursor_event_id: next_cursor
            .map(|cursor| LoreString::from(cursor.event_id().to_string()))
            .unwrap_or_default(),
        next_cursor_recorded_at: next_cursor.map_or(0, LockRecoveryAuditCursor::recorded_at),
        has_next_cursor: u8::from(next_cursor.is_some()),
    })
    .send();

    for entry in page.entries() {
        let event_id = entry.event_id().to_string();
        event::LoreEvent::LockRecoveryAuditEntry(LoreLockRecoveryAuditEntryEventData {
            event_id: LoreString::from(&event_id),
            actor_id: LoreString::from(entry.actor_id()),
            expected_owner_id: LoreString::from(entry.expected_owner_id()),
            recorded_at: entry.recorded_at(),
            resource_count: entry.resources().len() as u64,
        })
        .send();
        for resource in entry.resources() {
            event::LoreEvent::LockRecoveryAuditResource(LoreLockRecoveryAuditResourceEventData {
                event_id: LoreString::from(&event_id),
                branch: resource.branch,
                path: LoreString::from(&resource.description),
            })
            .send();
        }
    }
    Ok(())
}
