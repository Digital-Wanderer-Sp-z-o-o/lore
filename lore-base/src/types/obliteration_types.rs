// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT
use uuid::Uuid;

use super::Address;
use super::RepositoryId;
use crate::error::InvalidArguments;

pub const OBLITERATION_AUDIT_MAX_PAGE_SIZE: u32 = 100;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ObliterationAuditStatus {
    AssociationPending,
    AssociationRemoved,
    PayloadRetained,
    PayloadObliterated,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObliterationAuditCursor {
    event_id: Uuid,
    recorded_at: u64,
}

impl ObliterationAuditCursor {
    pub fn new(event_id: Uuid, recorded_at: u64) -> Self {
        Self {
            event_id,
            recorded_at,
        }
    }

    pub fn event_id(&self) -> Uuid {
        self.event_id
    }

    pub fn recorded_at(&self) -> u64 {
        self.recorded_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObliterationAuditQuery {
    limit: u32,
    cursor: Option<ObliterationAuditCursor>,
}

impl ObliterationAuditQuery {
    pub fn try_new(
        limit: u32,
        cursor: Option<ObliterationAuditCursor>,
    ) -> Result<Self, InvalidArguments> {
        if !(1..=OBLITERATION_AUDIT_MAX_PAGE_SIZE).contains(&limit) {
            return Err(InvalidArguments {
                reason: format!(
                    "obliteration audit page size must be between 1 and {OBLITERATION_AUDIT_MAX_PAGE_SIZE}"
                ),
            });
        }
        Ok(Self { limit, cursor })
    }

    pub fn limit(&self) -> u32 {
        self.limit
    }

    pub fn cursor(&self) -> Option<&ObliterationAuditCursor> {
        self.cursor.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObliterationAuditEntry {
    event_id: Uuid,
    actor_id: String,
    correlation_id: String,
    repository: RepositoryId,
    address: Address,
    status: ObliterationAuditStatus,
    remaining_associations: Option<u64>,
    recorded_at: u64,
    completed_at: Option<u64>,
}

pub struct ObliterationAuditEntryData {
    pub event_id: Uuid,
    pub actor_id: String,
    pub correlation_id: String,
    pub repository: RepositoryId,
    pub address: Address,
    pub status: ObliterationAuditStatus,
    pub remaining_associations: Option<u64>,
    pub recorded_at: u64,
    pub completed_at: Option<u64>,
}

impl ObliterationAuditEntry {
    pub fn try_new(data: ObliterationAuditEntryData) -> Result<Self, InvalidArguments> {
        if data.actor_id.is_empty() || data.correlation_id.is_empty() {
            return Err(InvalidArguments {
                reason: "obliteration audit actor and correlation ID cannot be empty".into(),
            });
        }
        validate_lifecycle(data.status, data.remaining_associations, data.completed_at)?;
        Ok(Self {
            event_id: data.event_id,
            actor_id: data.actor_id,
            correlation_id: data.correlation_id,
            repository: data.repository,
            address: data.address,
            status: data.status,
            remaining_associations: data.remaining_associations,
            recorded_at: data.recorded_at,
            completed_at: data.completed_at,
        })
    }

    pub fn event_id(&self) -> Uuid {
        self.event_id
    }

    pub fn actor_id(&self) -> &str {
        &self.actor_id
    }

    pub fn correlation_id(&self) -> &str {
        &self.correlation_id
    }

    pub fn repository(&self) -> RepositoryId {
        self.repository
    }

    pub fn address(&self) -> Address {
        self.address
    }

    pub fn status(&self) -> ObliterationAuditStatus {
        self.status
    }

    pub fn remaining_associations(&self) -> Option<u64> {
        self.remaining_associations
    }

    pub fn recorded_at(&self) -> u64 {
        self.recorded_at
    }

    pub fn completed_at(&self) -> Option<u64> {
        self.completed_at
    }
}

fn validate_lifecycle(
    status: ObliterationAuditStatus,
    remaining_associations: Option<u64>,
    completed_at: Option<u64>,
) -> Result<(), InvalidArguments> {
    let valid = match status {
        ObliterationAuditStatus::AssociationPending => {
            remaining_associations.is_none() && completed_at.is_none()
        }
        ObliterationAuditStatus::AssociationRemoved => {
            remaining_associations.is_some() && completed_at.is_none()
        }
        ObliterationAuditStatus::PayloadRetained => {
            remaining_associations.is_some_and(|count| count > 0) && completed_at.is_some()
        }
        ObliterationAuditStatus::PayloadObliterated => {
            remaining_associations == Some(0) && completed_at.is_some()
        }
    };
    if valid {
        Ok(())
    } else {
        Err(InvalidArguments {
            reason: "obliteration audit lifecycle fields do not match its status".into(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObliterationAuditPage {
    entries: Vec<ObliterationAuditEntry>,
    next_cursor: Option<ObliterationAuditCursor>,
}

impl ObliterationAuditPage {
    pub fn new(
        entries: Vec<ObliterationAuditEntry>,
        next_cursor: Option<ObliterationAuditCursor>,
    ) -> Self {
        Self {
            entries,
            next_cursor,
        }
    }

    pub fn entries(&self) -> &[ObliterationAuditEntry] {
        &self.entries
    }

    pub fn next_cursor(&self) -> Option<&ObliterationAuditCursor> {
        self.next_cursor.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_rejects_out_of_range_page_sizes() {
        assert!(ObliterationAuditQuery::try_new(0, None).is_err());
        assert!(
            ObliterationAuditQuery::try_new(OBLITERATION_AUDIT_MAX_PAGE_SIZE + 1, None).is_err()
        );
    }

    #[test]
    fn completed_statuses_require_consistent_outcomes() {
        let common = (
            Uuid::new_v4(),
            "owner".to_owned(),
            "correlation".to_owned(),
            RepositoryId::default(),
            Address::default(),
        );
        assert!(
            ObliterationAuditEntry::try_new(ObliterationAuditEntryData {
                event_id: common.0,
                actor_id: common.1.clone(),
                correlation_id: common.2.clone(),
                repository: common.3,
                address: common.4,
                status: ObliterationAuditStatus::PayloadRetained,
                remaining_associations: Some(0),
                recorded_at: 1,
                completed_at: Some(2),
            })
            .is_err()
        );
        assert!(
            ObliterationAuditEntry::try_new(ObliterationAuditEntryData {
                event_id: common.0,
                actor_id: common.1,
                correlation_id: common.2,
                repository: common.3,
                address: common.4,
                status: ObliterationAuditStatus::PayloadObliterated,
                remaining_associations: Some(0),
                recorded_at: 1,
                completed_at: Some(2),
            })
            .is_ok()
        );
    }
}
