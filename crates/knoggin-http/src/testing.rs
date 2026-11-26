// SPDX-License-Identifier: AGPL-3.0-only
//! Test helpers: a store host over a `LocalBackend` in a folder the test owns, the fake
//! embedding provider on both threads, and request contexts for one user.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::path::Path;
use std::sync::Arc;

use knoggin_core::embed::EmbeddingProvider;
use knoggin_core::ops::{ModelState, ModelStatus};
use knoggin_core::testing::FakeProvider;
use knoggin_core::{Sensitive, StoreId, UserId};
use knoggin_crypto::KeyProvider;
use knoggin_state::{LocalFileDb, NewStore, NewUser, StateDb, Stores, Users};
use tokio::sync::{OnceCell, watch};
use uuid::Uuid;

use crate::ctx::{CredentialKind, CredentialRef, Permissions, RequestContext};
use crate::host::{
    EmbedWaits, Embedder, HostConfig, LocalBackend, Providers, StoreBackend, StoreHost,
};

/// The model state of test mode: ready at once, under the fake provider's identity.
pub fn fake_model_state(state: ModelStatus) -> ModelState {
    ModelState {
        state,
        identity: FakeProvider::new().identity().id.clone(),
        name: "Fake model".into(),
        bytes_done: None,
        bytes_total: None,
        reason: None,
    }
}

/// A host for tests, with its pieces.
pub struct TestHost {
    /// The host.
    pub host: Arc<StoreHost>,
    /// Its backend, over `<dir>/data`.
    pub backend: Arc<LocalBackend>,
    /// The store records.
    pub stores: Stores,
    /// The key provider (the fixed test key set).
    pub keys: Arc<dyn KeyProvider>,
    /// The one user.
    pub user: UserId,
    /// The user's personal store.
    pub store: StoreId,
    /// Sends model state changes.
    pub model: watch::Sender<ModelState>,
    /// The cell of the providers; filled when the model is ready.
    pub providers: Arc<OnceCell<Providers>>,
}

impl std::fmt::Debug for TestHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestHost")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

impl TestHost {
    /// A host over `dir` with `config`. With `ready`, the fake model is loaded at once;
    /// without, the model stays `loading` until [`TestHost::make_ready`].
    pub async fn new(dir: &Path, config: HostConfig, ready: bool, waits: EmbedWaits) -> Self {
        Self::with_backend(dir, config, ready, waits, |b| b).await
    }

    /// As [`TestHost::new`], with the host's backend wrapped by `wrap` (a test backend
    /// that counts or fails saves).
    pub async fn with_backend(
        dir: &Path,
        config: HostConfig,
        ready: bool,
        waits: EmbedWaits,
        wrap: impl FnOnce(Arc<LocalBackend>) -> Arc<dyn StoreBackend>,
    ) -> Self {
        let keys: Arc<dyn KeyProvider> = Arc::new(knoggin_crypto::testing::test_provider());
        let db: Arc<dyn StateDb> = Arc::new(LocalFileDb::in_memory(knoggin_state::system_clock()));
        let fields = Arc::new(knoggin_state::testing::test_field_keys());
        let stores = Stores::new(db.clone(), fields.clone());
        let users = Users::new(db, fields);
        let user = UserId::from_uuid(Uuid::now_v7());
        let store = StoreId::from_uuid(Uuid::now_v7());
        let (_, wrapped) = keys.generate(store).await.unwrap();
        users
            .create(
                NewUser {
                    id: user,
                    issuer: "local".into(),
                    subject: Sensitive::new("local".into()),
                    email: None,
                    email_verified: false,
                    name: None,
                },
                NewStore {
                    id: store,
                    name: Sensitive::new("Personal".into()),
                    wrapped,
                },
                0,
            )
            .await
            .unwrap();
        let backend = Arc::new(LocalBackend::new(
            &dir.join("data"),
            keys.clone(),
            stores.clone(),
            7,
        ));
        let (model, rx) = watch::channel(fake_model_state(ModelStatus::Loading));
        let providers = Arc::new(OnceCell::new());
        let embedder = Embedder::new(rx, providers.clone(), waits);
        let host = StoreHost::new(config, wrap(backend.clone()), embedder);
        let t = Self {
            host,
            backend,
            stores,
            keys,
            user,
            store,
            model,
            providers,
        };
        if ready {
            t.make_ready();
        }
        t
    }

    /// Loads two fake providers and reports the model ready.
    pub fn make_ready(&self) {
        let _ = self.providers.set(Providers {
            query: Arc::new(FakeProvider::new()),
            document: Arc::new(FakeProvider::new()),
        });
        self.model
            .send_replace(fake_model_state(ModelStatus::Ready));
    }

    /// A context for the personal store with `permissions`, as an API key.
    pub fn ctx(&self, permissions: Permissions) -> RequestContext {
        self.ctx_for(self.store, permissions)
    }

    /// A context for `store` with `permissions`, as an API key.
    pub fn ctx_for(&self, store: StoreId, permissions: Permissions) -> RequestContext {
        RequestContext {
            request_id: Uuid::now_v7(),
            user_id: self.user,
            store_id: store,
            permissions,
            credential: CredentialRef {
                kind: CredentialKind::ApiKey,
                id: Some(Uuid::now_v7()),
            },
            default_namespace: None,
            client: None,
        }
    }

    /// Makes another store of the user, with its own data key.
    pub async fn new_store(&self, name: &str) -> StoreId {
        let id = StoreId::from_uuid(Uuid::now_v7());
        let (_, wrapped) = self.keys.generate(id).await.unwrap();
        self.stores
            .create_app(
                self.user,
                NewStore {
                    id,
                    name: Sensitive::new(name.into()),
                    wrapped,
                },
                0,
            )
            .await
            .unwrap();
        id
    }
}

/// The local configuration under the fake model.
pub fn fake_config() -> HostConfig {
    HostConfig::local(FakeProvider::new().identity().clone())
}
