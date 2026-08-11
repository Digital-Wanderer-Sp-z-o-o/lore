// SPDX-FileCopyrightText: 2026 Digital Wanderer Sp. z o.o.
// SPDX-License-Identifier: MIT

//! Native Cloudflare Durable Objects and R2 storage plugin.

use std::sync::Arc;
use std::time::Duration;

use lore_base::error::PluginConfigError;
use lore_base::error::PluginInitError;
use lore_cloudflare::CloudflareClient;
use lore_cloudflare::CloudflareImmutableStore;
use lore_cloudflare::CloudflareLockStore;
use lore_cloudflare::CloudflareMutableStore;
use lore_revision::lock::LockStore;
use lore_storage::ImmutableStore;
use lore_storage::MutableStore;
use serde::Deserialize;

use crate::plugins::ImmutableStorePluginFactory;
use crate::plugins::LockStorePluginFactory;
use crate::plugins::MutableStorePluginFactory;
use crate::plugins::PluginError;
use crate::plugins::PluginRegistry;

const PLUGIN_NAME: &str = "cloudflare";
const DEFAULT_SECRET_ENV: &str = "LORE_CLOUDFLARE_SHARED_SECRET";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudflarePluginConfig {
    pub endpoint_url: String,
    #[serde(default = "default_secret_env")]
    pub auth_shared_secret_env: String,
    #[serde(default = "default_timeout_millis")]
    pub timeout_millis: u64,
}

fn default_secret_env() -> String {
    DEFAULT_SECRET_ENV.to_string()
}
fn default_timeout_millis() -> u64 {
    15_000
}

impl CloudflarePluginConfig {
    fn validate(&self) -> Result<(), PluginError> {
        if !self.endpoint_url.starts_with("https://")
            && !self.endpoint_url.starts_with("http://127.0.0.1")
        {
            return Err(config_error(
                "endpoint_url must use HTTPS (localhost is allowed for tests)",
            ));
        }
        if self.auth_shared_secret_env.trim().is_empty() {
            return Err(config_error("auth_shared_secret_env cannot be empty"));
        }
        if self.timeout_millis == 0 {
            return Err(config_error("timeout_millis must be greater than zero"));
        }
        Ok(())
    }

    fn client(&self) -> Result<CloudflareClient, PluginError> {
        self.validate()?;
        let secret = std::env::var(&self.auth_shared_secret_env).map_err(|_env_error| {
            PluginError::from(PluginInitError {
                plugin_name: PLUGIN_NAME.to_string(),
                message: format!(
                    "required environment variable {} is not set",
                    self.auth_shared_secret_env
                ),
            })
        })?;
        if secret.len() < 32 {
            return Err(PluginInitError {
                plugin_name: PLUGIN_NAME.to_string(),
                message: format!(
                    "{} must contain at least 32 characters",
                    self.auth_shared_secret_env
                ),
            }
            .into());
        }
        CloudflareClient::new(
            self.endpoint_url.clone(),
            secret,
            Duration::from_millis(self.timeout_millis),
        )
        .map_err(|error| {
            PluginInitError {
                plugin_name: PLUGIN_NAME.to_string(),
                message: format!("failed to initialize Cloudflare HTTP client: {error}"),
            }
            .into()
        })
    }
}

fn parse(config: &toml::Value) -> Result<CloudflarePluginConfig, PluginError> {
    let parsed: CloudflarePluginConfig = config.clone().try_into().map_err(|error| {
        PluginError::from(PluginConfigError {
            plugin_name: PLUGIN_NAME.to_string(),
            message: format!("failed to deserialize Cloudflare config: {error}"),
        })
    })?;
    parsed.validate()?;
    Ok(parsed)
}

fn config_error(message: impl Into<String>) -> PluginError {
    PluginConfigError {
        plugin_name: PLUGIN_NAME.to_string(),
        message: message.into(),
    }
    .into()
}

pub struct CloudflareImmutableStorePluginFactory;

impl ImmutableStorePluginFactory for CloudflareImmutableStorePluginFactory {
    fn name(&self) -> &'static str {
        PLUGIN_NAME
    }
    fn validate_config(&self, config: &toml::Value) -> Result<(), PluginError> {
        parse(config).map(|_| ())
    }
    fn create(&self, config: &toml::Value) -> Result<Arc<dyn ImmutableStore>, PluginError> {
        let client = parse(config)?.client()?;
        Ok(Arc::new(CloudflareImmutableStore::new(client)))
    }
}

pub struct CloudflareMutableStorePluginFactory;

impl MutableStorePluginFactory for CloudflareMutableStorePluginFactory {
    fn name(&self) -> &'static str {
        PLUGIN_NAME
    }
    fn validate_config(&self, config: &toml::Value) -> Result<(), PluginError> {
        parse(config).map(|_| ())
    }
    fn create(
        &self,
        config: &toml::Value,
        _immutable_store: Arc<dyn ImmutableStore>,
    ) -> Result<Arc<dyn MutableStore>, PluginError> {
        let client = parse(config)?.client()?;
        Ok(Arc::new(CloudflareMutableStore::new(client)))
    }
}

pub struct CloudflareLockStorePluginFactory;

impl LockStorePluginFactory for CloudflareLockStorePluginFactory {
    fn name(&self) -> &'static str {
        PLUGIN_NAME
    }
    fn validate_config(&self, config: &toml::Value) -> Result<(), PluginError> {
        parse(config).map(|_| ())
    }
    fn create(&self, config: &toml::Value) -> Result<Arc<dyn LockStore>, PluginError> {
        let client = parse(config)?.client()?;
        Ok(Arc::new(CloudflareLockStore::new(client)))
    }
}

pub fn register(registry: &mut PluginRegistry) {
    registry.register_immutable_store_plugin(Box::new(CloudflareImmutableStorePluginFactory));
    registry.register_mutable_store_plugin(Box::new(CloudflareMutableStorePluginFactory));
    registry.register_lock_store_plugin(Box::new(CloudflareLockStorePluginFactory));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_https_configuration() {
        let config: toml::Value = toml::from_str(
            r#"
            endpoint_url = "https://lore.example.workers.dev"
            auth_shared_secret_env = "LORE_SECRET"
            timeout_millis = 5000
        "#,
        )
        .unwrap();
        assert!(
            CloudflareImmutableStorePluginFactory
                .validate_config(&config)
                .is_ok()
        );
    }

    #[test]
    fn rejects_plain_http_remote_endpoint() {
        let config: toml::Value = toml::from_str(r#"endpoint_url = "http://example.com""#).unwrap();
        assert!(
            CloudflareMutableStorePluginFactory
                .validate_config(&config)
                .is_err()
        );
    }

    #[test]
    fn hetzner_profile_uses_valid_native_cloudflare_config() {
        let profile: toml::Value =
            toml::from_str(include_str!("../../../contrib/hetzner/config.toml")).unwrap();
        assert_valid_cloudflare_profile(&profile);
    }

    #[test]
    fn production_profile_uses_archigma_auth_and_promoted_cloudflare_data() {
        let profile: toml::Value = toml::from_str(include_str!(
            "../../../contrib/hetzner/config.production.toml"
        ))
        .unwrap();

        assert_eq!(
            profile["server"]["auth"]["jwt_issuer"].as_str(),
            Some("https://archigma.com")
        );
        assert_eq!(
            profile["server"]["auth"]["jwk"]["endpoint"].as_str(),
            Some("https://archigma.com/api/v1/lore/auth/jwks.json")
        );
        assert_eq!(
            profile["environment"]["endpoint"]["auth_url"].as_str(),
            Some("ucs-auth://archigma.com")
        );
        assert_valid_cloudflare_profile(&profile);
    }

    fn assert_valid_cloudflare_profile(profile: &toml::Value) {
        assert_eq!(
            profile["immutable_store"]["composite"]["durable"]["mode"].as_str(),
            Some(PLUGIN_NAME)
        );
        assert_eq!(profile["mutable_store"]["mode"].as_str(), Some(PLUGIN_NAME));
        assert_eq!(profile["lock_store"]["mode"].as_str(), Some(PLUGIN_NAME));
        CloudflareImmutableStorePluginFactory
            .validate_config(&profile["plugins"][PLUGIN_NAME])
            .unwrap();
    }
}
