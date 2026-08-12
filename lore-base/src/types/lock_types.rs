// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use uuid::Uuid;

use super::BranchId;
use super::Hash;
use crate::error::InvalidArguments;

#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
/// Descriptor of a resource that can be locked
pub struct LockResource {
    /// Branch ID
    pub branch: BranchId,

    /// Hash identifier for the resource
    pub hash: Hash,

    /// Human readable description of the resource (i.e. file path, property name, etc)
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
/// Represents the lock on a resource
pub struct LockData {
    /// Resource
    pub resource: LockResource,

    /// Identifier of the user holding the lock
    pub owner: String,

    /// Lock timestamp
    pub locked_at: u64,
}

pub const LOCK_RECOVERY_AUDIT_MAX_PAGE_SIZE: u32 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockRecoveryAuditCursor {
    event_id: Uuid,
    recorded_at: u64,
}

impl LockRecoveryAuditCursor {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockRecoveryAuditQuery {
    limit: u32,
    cursor: Option<LockRecoveryAuditCursor>,
}

impl LockRecoveryAuditQuery {
    pub fn try_new(
        limit: u32,
        cursor: Option<LockRecoveryAuditCursor>,
    ) -> Result<Self, InvalidArguments> {
        if !(1..=LOCK_RECOVERY_AUDIT_MAX_PAGE_SIZE).contains(&limit) {
            return Err(InvalidArguments {
                reason: format!(
                    "lock recovery audit page size must be between 1 and {LOCK_RECOVERY_AUDIT_MAX_PAGE_SIZE}"
                ),
            });
        }
        Ok(Self { limit, cursor })
    }

    pub fn limit(&self) -> u32 {
        self.limit
    }

    pub fn cursor(&self) -> Option<&LockRecoveryAuditCursor> {
        self.cursor.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockRecoveryAuditEntry {
    event_id: Uuid,
    actor_id: String,
    expected_owner_id: String,
    resources: Vec<LockResource>,
    recorded_at: u64,
}

impl LockRecoveryAuditEntry {
    pub fn try_new(
        event_id: Uuid,
        actor_id: String,
        expected_owner_id: String,
        resources: Vec<LockResource>,
        recorded_at: u64,
    ) -> Result<Self, InvalidArguments> {
        if actor_id.is_empty() || expected_owner_id.is_empty() {
            return Err(InvalidArguments {
                reason: "lock recovery audit actors cannot be empty".into(),
            });
        }
        if actor_id == expected_owner_id {
            return Err(InvalidArguments {
                reason: "lock recovery audit must describe a foreign-owner release".into(),
            });
        }
        if resources.is_empty() {
            return Err(InvalidArguments {
                reason: "lock recovery audit event must contain at least one resource".into(),
            });
        }
        Ok(Self {
            event_id,
            actor_id,
            expected_owner_id,
            resources,
            recorded_at,
        })
    }

    pub fn event_id(&self) -> Uuid {
        self.event_id
    }

    pub fn actor_id(&self) -> &str {
        &self.actor_id
    }

    pub fn expected_owner_id(&self) -> &str {
        &self.expected_owner_id
    }

    pub fn resources(&self) -> &[LockResource] {
        &self.resources
    }

    pub fn recorded_at(&self) -> u64 {
        self.recorded_at
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockRecoveryAuditPage {
    entries: Vec<LockRecoveryAuditEntry>,
    next_cursor: Option<LockRecoveryAuditCursor>,
}

impl LockRecoveryAuditPage {
    pub fn new(
        entries: Vec<LockRecoveryAuditEntry>,
        next_cursor: Option<LockRecoveryAuditCursor>,
    ) -> Self {
        Self {
            entries,
            next_cursor,
        }
    }

    pub fn entries(&self) -> &[LockRecoveryAuditEntry] {
        &self.entries
    }

    pub fn into_entries(self) -> Vec<LockRecoveryAuditEntry> {
        self.entries
    }

    pub fn next_cursor(&self) -> Option<&LockRecoveryAuditCursor> {
        self.next_cursor.as_ref()
    }
}

#[cfg(test)]
mod recovery_audit_tests {
    use super::*;

    #[test]
    fn query_rejects_zero_and_oversized_pages() {
        assert!(LockRecoveryAuditQuery::try_new(0, None).is_err());
        assert!(
            LockRecoveryAuditQuery::try_new(LOCK_RECOVERY_AUDIT_MAX_PAGE_SIZE + 1, None).is_err()
        );
    }

    #[test]
    fn audit_entry_requires_a_foreign_owner_and_resources() {
        let event_id = Uuid::new_v4();
        assert!(
            LockRecoveryAuditEntry::try_new(
                event_id,
                "artist".into(),
                "artist".into(),
                vec![LockResource::default()],
                1,
            )
            .is_err()
        );
        assert!(
            LockRecoveryAuditEntry::try_new(event_id, "admin".into(), "artist".into(), vec![], 1,)
                .is_err()
        );
    }
}
