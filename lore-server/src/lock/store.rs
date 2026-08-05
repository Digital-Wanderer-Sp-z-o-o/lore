// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::collections::hash_map::Entry;

use async_trait::async_trait;
use lore_base::error::InvalidArguments;
use lore_base::error::LockNotFound;
use lore_base::error::LockNotOwned;
use lore_base::types::Hash;
use lore_base::types::LockData;
use lore_base::types::LockResource;
use lore_revision::lock::LockError;
use lore_revision::lock::LockQuery;
use lore_revision::lock::LockStore;
use lore_revision::lore::BranchId;
use lore_revision::lore::RepositoryId;
use lore_revision::util;
use parking_lot::Mutex;

#[derive(Eq, Hash, PartialEq)]
pub struct LockKey {
    repository: RepositoryId,
    branch: BranchId,
    hash: Hash,
}

#[derive(Default)]
pub struct LocalLockStore {
    storage: Mutex<HashMap<LockKey, LockData>>,
}

#[async_trait]
impl LockStore for LocalLockStore {
    async fn lock_resources(
        &self,
        owner_id: &str,
        repository: RepositoryId,
        resources: &[LockResource],
    ) -> Result<Vec<LockData>, LockError> {
        let resources = deduplicate_resources(resources);
        let mut storage = self.storage.lock();
        for resource in &resources {
            let key = lock_key(repository, resource);
            if storage.get(&key).is_some_and(|lock| lock.owner != owner_id) {
                return Err(LockError::internal("resource already locked"));
            }
        }

        let mut locks = Vec::<LockData>::with_capacity(resources.len());
        let timestamp = util::time::timestamp();
        for resource in resources {
            let lock = LockData {
                resource: resource.clone(),
                owner: owner_id.to_string(),
                locked_at: timestamp,
            };
            if let Entry::Vacant(entry) = storage.entry(lock_key(repository, &resource)) {
                entry.insert(lock.clone());
                locks.push(lock);
            }
        }

        Ok(locks)
    }

    async fn query_locks(&self, query: LockQuery) -> Result<Vec<LockData>, LockError> {
        let storage = self.storage.lock();
        let mut locks = Vec::new();

        match query {
            LockQuery::Repository(repository) => {
                for (key, lock) in storage.iter() {
                    if key.repository == repository {
                        locks.push(lock.clone());
                    }
                }
            }
            LockQuery::RepositoryBranch(repository, branch) => {
                for (key, value) in storage.iter() {
                    if key.repository == repository && key.branch == branch {
                        locks.push(value.clone());
                    }
                }
            }
            LockQuery::RepositoryBranchDescription(repository, branch, description) => {
                for (key, value) in storage.iter() {
                    if key.repository == repository
                        && key.branch == branch
                        && value.resource.description == description
                    {
                        locks.push(value.clone());
                    }
                }
            }
            LockQuery::OwnerRepository(owner, repository) => {
                for (key, value) in storage.iter() {
                    if key.repository == repository && value.owner == owner {
                        locks.push(value.clone());
                    }
                }
            }
            LockQuery::OwnerRepositoryBranch(owner, repository, branch) => {
                for (key, value) in storage.iter() {
                    if key.repository == repository && key.branch == branch && value.owner == owner
                    {
                        locks.push(value.clone());
                    }
                }
            }
            LockQuery::HashRepositoryBranch(resource, repository, branch) => {
                let key = LockKey {
                    hash: resource,
                    repository,
                    branch,
                };

                if let Some(lock) = storage.get(&key) {
                    locks.push(lock.clone());
                }
            }
            _ => {
                return Err(InvalidArguments {
                    reason: "unsupported lock query".into(),
                }
                .into());
            }
        }

        Ok(locks)
    }

    async fn check_locks_status(
        &self,
        repository: RepositoryId,
        resources: &[LockResource],
    ) -> Result<Vec<LockData>, LockError> {
        let storage = self.storage.lock();
        let mut locked = vec![];

        for resource in resources {
            let key = LockKey {
                repository,
                branch: resource.branch,
                hash: resource.hash,
            };

            if let Some(lock) = storage.get(&key) {
                locked.push(lock.clone());
            }
        }

        Ok(locked)
    }

    async fn unlock_resources(
        &self,
        _actor_id: &str,
        expected_owner_id: &str,
        repository: RepositoryId,
        resources: &[LockResource],
    ) -> Result<Vec<LockResource>, LockError> {
        let resources = deduplicate_resources(resources);
        let mut storage = self.storage.lock();
        for resource in &resources {
            let Some(lock) = storage.get(&lock_key(repository, resource)) else {
                return Err(LockNotFound.into());
            };
            if lock.owner != expected_owner_id {
                return Err(LockNotOwned.into());
            }
        }
        for resource in &resources {
            storage.remove(&lock_key(repository, resource));
        }

        Ok(resources)
    }
}

fn lock_key(repository: RepositoryId, resource: &LockResource) -> LockKey {
    LockKey {
        repository,
        branch: resource.branch,
        hash: resource.hash,
    }
}

fn deduplicate_resources(resources: &[LockResource]) -> Vec<LockResource> {
    let mut resources = resources.to_vec();
    resources.sort();
    resources.dedup();
    resources
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn owner_mismatch_does_not_partially_release_a_batch() {
        let store = LocalLockStore::default();
        let repository = RepositoryId::default();
        let first = LockResource {
            branch: BranchId::default(),
            hash: rand::random(),
            description: "first.blend".to_owned(),
        };
        let second = LockResource {
            branch: BranchId::default(),
            hash: rand::random(),
            description: "second.blend".to_owned(),
        };
        store
            .lock_resources("alice", repository, std::slice::from_ref(&first))
            .await
            .unwrap();
        store
            .lock_resources("bob", repository, std::slice::from_ref(&second))
            .await
            .unwrap();

        let error = store
            .unlock_resources(
                "admin",
                "alice",
                repository,
                &[first.clone(), second.clone()],
            )
            .await
            .unwrap_err();
        assert!(matches!(error, LockError::LockNotOwned(_)));

        let locks = store
            .check_locks_status(repository, &[first, second])
            .await
            .unwrap();
        assert_eq!(locks.len(), 2);
    }
}
