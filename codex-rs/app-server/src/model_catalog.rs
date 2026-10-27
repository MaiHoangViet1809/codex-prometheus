//! Loads the model catalog for the current effective provider configuration.

use std::path::PathBuf;
use std::sync::Arc;

use codex_config::ConfigLayerStack;
use codex_core::config::Config;
use codex_features::Feature;
use codex_login::AuthManager;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::openai_models::ModelPreset;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::sync::Mutex;

use crate::config_manager::ConfigManager;
use crate::config_manager::StartupConfig;

/// Refreshes catalog routing without changing the routes retained by existing threads.
pub(crate) struct ModelCatalog {
    config_manager: ConfigManager,
    auth_manager: Arc<AuthManager>,
    startup_cwd: Option<PathBuf>,
    state: Mutex<CatalogState>,
}

struct CatalogState {
    needs_reload: bool,
    config: Arc<Config>,
    models_manager: SharedModelsManager,
}

impl ModelCatalog {
    pub(crate) fn new(
        config_manager: ConfigManager,
        startup_cwd: Option<PathBuf>,
        config: Arc<Config>,
        models_manager: SharedModelsManager,
        auth_manager: Arc<AuthManager>,
    ) -> Self {
        Self {
            config_manager,
            auth_manager,
            startup_cwd,
            state: Mutex::new(CatalogState {
                needs_reload: false,
                config,
                models_manager,
            }),
        }
    }

    /// Loads the same startup context used to reload the catalog after account setup.
    pub(crate) async fn load_startup_config_layers(&self) -> std::io::Result<ConfigLayerStack> {
        let cwd = self
            .startup_cwd
            .as_deref()
            .map(AbsolutePathBuf::from_absolute_path)
            .transpose()?;
        self.config_manager.load_config_layers(cwd).await
    }

    /// Marks catalog configuration stale after an account transition.
    pub(crate) async fn invalidate(&self) {
        self.state.lock().await.needs_reload = true;
    }

    pub(crate) async fn set_api_key_model_discovery_enabled(&self, enabled: bool) {
        self.state
            .lock()
            .await
            .models_manager
            .set_api_key_model_discovery_enabled(enabled);
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "catalog reloads are serialized with account invalidation; model fetching holds no lock"
    )]
    pub(crate) async fn current(&self) -> std::io::Result<(Arc<Config>, SharedModelsManager)> {
        let mut state = self.state.lock().await;
        if state.needs_reload {
            // Keep the large config-loading future off callers' stacks.
            let config = Box::pin(
                self.config_manager
                    .load_startup_config(self.startup_cwd.clone()),
            )
            .await?;
            let StartupConfig {
                config,
                used_defaults,
            } = config;
            if config.model_provider_id != state.config.model_provider_id
                || config.model_provider != state.config.model_provider
                || config.model_catalog != state.config.model_catalog
            {
                state.models_manager =
                    codex_core::build_models_manager(&config, Arc::clone(&self.auth_manager));
            }
            state.models_manager.set_api_key_model_discovery_enabled(
                config.features.enabled(Feature::ApiKeyModelDiscovery),
            );
            state.config = Arc::new(config);
            // Try again on the next access so a temporary fallback cannot pin the provider.
            state.needs_reload = used_defaults;
        }
        if let Some(path) = state.config.model_catalog_json.clone() {
            // Read the source rather than relying on mtime/length: publishers can
            // atomically replace a file with another of the same size and timestamp.
            // Keep unrelated configuration and existing thread snapshots unchanged.
            let refreshed =
                tokio::task::spawn_blocking(move || codex_core::config::load_catalog_json(&path))
                    .await
                    .map_err(std::io::Error::other)?;
            match refreshed {
                Ok(catalog) if state.config.model_catalog.as_ref() != Some(&catalog) => {
                    let mut config = (*state.config).clone();
                    config.model_catalog = Some(catalog);
                    let manager =
                        codex_core::build_models_manager(&config, Arc::clone(&self.auth_manager));
                    manager.set_api_key_model_discovery_enabled(
                        config.features.enabled(Feature::ApiKeyModelDiscovery),
                    );
                    state.config = Arc::new(config);
                    state.models_manager = manager;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(error_kind = ?error.kind(), "custom model catalog refresh failed; retaining last valid catalog");
                }
            }
        }
        Ok((Arc::clone(&state.config), Arc::clone(&state.models_manager)))
    }

    pub(crate) async fn list_models(
        &self,
        refresh_strategy: RefreshStrategy,
    ) -> std::io::Result<Vec<ModelPreset>> {
        let (config, models_manager) = self.current().await?;
        self.list_models_from_snapshot(config, models_manager, refresh_strategy)
            .await
    }

    pub(crate) async fn list_models_from_snapshot(
        &self,
        config: Arc<Config>,
        models_manager: SharedModelsManager,
        refresh_strategy: RefreshStrategy,
    ) -> std::io::Result<Vec<ModelPreset>> {
        // Keep the existing policy check even for a warm catalog. A policy
        // edit alone must not make the retained provider silently permissible.
        self.config_manager
            .check_thread_model_provider(&config)
            .await?;
        Ok(models_manager
            .list_models(refresh_strategy, config.http_client_factory())
            .await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::openai_models::ModelVisibility;
    use codex_protocol::openai_models::ModelsResponse;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn custom_catalog_refresh_is_atomic_and_keeps_last_valid_snapshot() -> anyhow::Result<()>
    {
        let home = tempfile::tempdir()?;
        let path = home.path().join("catalog.json");
        let mut first = codex_models_manager::bundled_models_response()?;
        first.models.truncate(1);
        first.models[0].slug = "first".into();
        first.models[0].visibility = ModelVisibility::List;
        std::fs::write(&path, serde_json::to_vec(&first)?)?;
        let original_metadata = std::fs::metadata(&path)?;
        std::fs::write(
            home.path().join("config.toml"),
            "model_catalog_json = 'catalog.json'",
        )?;
        let manager = ConfigManager::without_managed_config_for_tests(home.path().to_path_buf());
        let config = Arc::new(manager.load_non_project_config().await?);
        let auth = AuthManager::shared_from_config(&config, false).await?;
        let models = codex_core::build_models_manager(&config, Arc::clone(&auth));
        let catalog = Arc::new(ModelCatalog::new(manager, None, config, models, auth));
        let original = catalog.current().await?;
        assert!(Arc::ptr_eq(&original.1, &catalog.current().await?.1));

        // A custom file refresh must not reparse the user's unrelated config.
        std::fs::write(home.path().join("config.toml"), "model = [")?;
        for invalid in [b"not JSON".as_slice(), b"{\"models\":[]}"] {
            std::fs::write(&path, invalid)?;
            let retained = catalog.current().await?;
            assert!(Arc::ptr_eq(&original.0, &retained.0));
            assert!(Arc::ptr_eq(&original.1, &retained.1));
        }
        std::fs::remove_file(&path)?;
        assert!(Arc::ptr_eq(&original.1, &catalog.current().await?.1));

        let mut second = first.clone();
        second.models[0].slug = "other".into(); // same length; metadata is not a generation
        second.models[0].visibility = ModelVisibility::Hide;
        let replacement = home.path().join("replacement.json");
        std::fs::write(&replacement, serde_json::to_vec(&second)?)?;
        std::fs::File::open(&replacement)?
            .set_times(std::fs::FileTimes::new().set_modified(original_metadata.modified()?))?;
        assert_eq!(
            std::fs::metadata(&replacement)?.len(),
            original_metadata.len()
        );
        std::fs::rename(&replacement, &path)?;
        let (left, right) = tokio::join!(catalog.current(), catalog.current());
        let changed = left?;
        assert!(Arc::ptr_eq(&changed.1, &right?.1));
        assert!(!Arc::ptr_eq(&original.1, &changed.1));
        assert_eq!(changed.0.model_catalog.as_ref(), Some(&second));
        // Previously issued snapshots, including those retained by threads, stay immutable.
        assert_eq!(original.0.model_catalog.as_ref(), Some(&first));

        let mut added = second.clone();
        added.models.push(first.models[0].clone());
        std::fs::write(&path, serde_json::to_vec(&added)?)?;
        assert_eq!(
            catalog.current().await?.0.model_catalog.as_ref(),
            Some(&added)
        );
        let removed = ModelsResponse {
            models: vec![first.models[0].clone()],
        };
        std::fs::write(&path, serde_json::to_vec(&removed)?)?;
        assert_eq!(
            catalog.current().await?.0.model_catalog.as_ref(),
            Some(&removed)
        );
        Ok(())
    }
}
