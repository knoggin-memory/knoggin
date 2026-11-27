// SPDX-License-Identifier: AGPL-3.0-only
//! The server roles' wiring (tab "Config, CLI, local install", Server commands): builds the
//! settings, the state database, the key provider, the bucket and the launcher from the
//! config, and runs `knoggin router`, `serve`, `lifecycle` and `launcher`. Every setting a
//! role needs is checked before it connects to anything, so a refusal is quick and says
//! which setting to set. The admin commands use the same builders (`cli`).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use knoggin_core::Timestamp;
use knoggin_crypto::source::{FileRule, read_key_file};
use knoggin_crypto::{KeyProvider, LocalKeyProvider, MasterKeySet};
use knoggin_http::edge;
use knoggin_server::kms::KmsKeyProvider;
use knoggin_server::launcher::{BuildInfo, ContainerEnv, DockerLauncher, EcsLauncher, Launcher};
use knoggin_server::link::InternalSecret;
use knoggin_server::router::{
    Backends, Listeners, RouterStart, forward::Timing, reserve, run_router,
};
use knoggin_server::settings::{
    Cidr, ContainerSettings, EcsSettings, InternalSettings, LauncherServiceSettings, RateSetting,
    RetentionSettings, RolloutSettings, RouterLimits, RouterSettings, SecretText, StorageSettings,
};
use knoggin_server::storage::{ObjectStorage, S3Storage};
use knoggin_state::{Clock, DynamoDb, PostgresDb, StateDb, system_clock};
use tokio::sync::watch;

use crate::config::{Backend, Config, Env, KeyProviderKind, LauncherKind, LogLevel, Role};

/// How long a server role keeps trying its database and the launcher at start.
const START_TRIES: Duration = Duration::from_secs(60);

/// Where the master key lives, checked before anything connects.
pub(crate) enum Keys {
    /// `keys.provider = "key_file"`: the key set file.
    File(PathBuf),
    /// `keys.provider = "kms"`: the KMS key ARN.
    Kms(String),
}

impl Keys {
    /// The admin status's name of the provider.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::File(_) => "key_file",
            Self::Kms(_) => "kms",
        }
    }
}

/// `keys.*`: the provider a server role uses.
pub(crate) fn keys_setting(config: &Config) -> anyhow::Result<Keys> {
    match config.keys.provider {
        None => bail!("keys.provider is required on a server: key_file (one host) or kms (AWS)"),
        Some(KeyProviderKind::Kms) => config
            .keys
            .kms_key_id
            .clone()
            .filter(|id| !id.is_empty())
            .map(Keys::Kms)
            .ok_or_else(|| anyhow!("keys.provider = \"kms\" needs keys.kms_key_id")),
        Some(KeyProviderKind::KeyFile) => config
            .keys
            .master_key_file
            .clone()
            .map(Keys::File)
            .ok_or_else(|| anyhow!("keys.provider = \"key_file\" needs keys.master_key_file")),
    }
}

/// The master key provider: the key set file (a server role reads a file that others
/// cannot write), or the KMS key through the AWS default chain.
pub(crate) async fn key_provider(keys: &Keys) -> anyhow::Result<Arc<dyn KeyProvider>> {
    match keys {
        Keys::File(path) => {
            let set = read_key_file(path, FileRule::NotWritableByOthers)
                .with_context(|| format!("keys.master_key_file {}", path.display()))?;
            Ok(Arc::new(LocalKeyProvider::new(set)))
        }
        Keys::Kms(id) => Ok(Arc::new(KmsKeyProvider::new(&aws_config().await, id))),
    }
}

/// The shared AWS config of DynamoDB, KMS and ECS: the default chain's region and
/// credentials (the task role on AWS). Knoggin has no endpoint setting for these three; a
/// test points a child process at its stand-in with `AWS_ENDPOINT_URL`.
async fn aws_config() -> knoggin_server::aws::SdkConfig {
    knoggin_server::aws::sdk_config(None, None).await
}

/// What `state.*` names, checked before anything connects.
pub(crate) enum StateSetting {
    /// One host: the URL and the state key set.
    Postgres {
        url: knoggin_core::Sensitive<String>,
        doc_keys: MasterKeySet,
    },
    /// AWS: the table.
    Dynamodb { table: String },
}

impl StateSetting {
    /// The admin status's name of the backend.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Postgres { .. } => "postgres",
            Self::Dynamodb { .. } => "dynamodb",
        }
    }
}

/// `state.*` for a server role: Postgres with its URL and the state key file, or DynamoDB
/// with its table.
pub(crate) fn state_setting(config: &Config, role: Role) -> anyhow::Result<StateSetting> {
    match config.state.backend {
        None => {
            bail!("state.backend is required on a server: postgres (one host) or dynamodb (AWS)")
        }
        Some(Backend::Dynamodb) => {
            return config
                .state
                .dynamodb_table
                .clone()
                .filter(|t| !t.is_empty())
                .map(|table| StateSetting::Dynamodb { table })
                .ok_or_else(|| anyhow!("state.backend = \"dynamodb\" needs state.dynamodb_table"));
        }
        Some(Backend::LocalFile) => bail!(
            "state.backend = \"local_file\" is the local daemon's; a server uses postgres or dynamodb"
        ),
        Some(Backend::Postgres) => {}
    }
    let url = config.secret("state.postgres_url", role)?.ok_or_else(|| {
        anyhow!(
            "state.backend = \"postgres\" needs state.postgres_url (or state.postgres_url_file)"
        )
    })?;
    let path =
        config.state.postgres_doc_key_file.clone().ok_or_else(|| {
            anyhow!("state.backend = \"postgres\" needs state.postgres_doc_key_file")
        })?;
    let doc_keys = read_key_file(&path, FileRule::NotWritableByOthers)
        .with_context(|| format!("state.postgres_doc_key_file {}", path.display()))?;
    Ok(StateSetting::Postgres { url, doc_keys })
}

/// The state database, and on Postgres the backend itself for the lifecycle task's
/// housekeeping.
pub(crate) struct State {
    pub(crate) db: Arc<dyn StateDb>,
    pub(crate) postgres: Option<Arc<PostgresDb>>,
}

/// Connects, trying Postgres for up to 60 seconds; the router also migrates. DynamoDB
/// opens no connection here: the router's first read (the schema record) is its check.
pub(crate) async fn connect_state(
    s: &StateSetting,
    clock: Clock,
    migrate: bool,
) -> anyhow::Result<State> {
    let (url, doc_keys) = match s {
        StateSetting::Postgres { url, doc_keys } => (url, doc_keys),
        StateSetting::Dynamodb { table } => {
            let conf = aws_sdk_dynamodb::config::Builder::from(&aws_config().await);
            return Ok(State {
                db: Arc::new(DynamoDb::new(conf, table.clone(), clock)),
                postgres: None,
            });
        }
    };
    let deadline = tokio::time::Instant::now() + START_TRIES;
    let db = loop {
        match PostgresDb::connect(url.expose(), doc_keys, clock.clone()).await {
            Ok(db) => break db,
            Err(knoggin_state::StateError::Unavailable(_))
                if tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => return Err(anyhow!("the state database: {e}")),
        }
    };
    if migrate {
        db.migrate()
            .await
            .map_err(|e| anyhow!("migrating the state database: {e}"))?;
    }
    let db = Arc::new(db);
    Ok(State {
        db: db.clone(),
        postgres: Some(db),
    })
}

/// `storage.*`: the bucket must be named.
pub(crate) fn storage_settings(config: &Config) -> anyhow::Result<StorageSettings> {
    let s = &config.storage;
    Ok(StorageSettings {
        bucket: s
            .bucket
            .clone()
            .ok_or_else(|| anyhow!("storage.bucket is required on a server"))?,
        region: s.region.clone(),
        endpoint_url: s.endpoint_url.clone(),
        force_path_style: s.force_path_style,
        credentials_file: s.credentials_file.clone(),
        link_ttl_seconds: s.link_ttl_seconds,
    })
}

pub(crate) async fn storage(settings: &StorageSettings) -> Arc<dyn ObjectStorage> {
    Arc::new(S3Storage::connect(settings).await)
}

fn retention(config: &Config) -> RetentionSettings {
    RetentionSettings {
        days: config.retention.days,
        soft_delete_days: config.retention.soft_delete_days,
    }
}

fn cidr(name: &str, text: &str) -> anyhow::Result<Cidr> {
    text.parse()
        .map_err(|_| anyhow!("{name}: {text:?} is not an address range like 10.0.0.0/8"))
}

fn log_level(level: LogLevel) -> knoggin_server::launcher::LogLevel {
    use knoggin_server::launcher::LogLevel as L;
    match level {
        LogLevel::Error => L::Error,
        LogLevel::Warn => L::Warn,
        LogLevel::Info => L::Info,
        LogLevel::Debug => L::Debug,
        LogLevel::Trace => L::Trace,
    }
}

fn containers(config: &Config) -> anyhow::Result<ContainerSettings> {
    let c = &config.containers;
    Ok(ContainerSettings {
        user_net_cidr: cidr("containers.user_net_cidr", &c.user_net_cidr)?,
        memory_mib: c.memory_mib,
        pool_target: c.pool_target,
        max: c.max,
        idle_seconds: c.idle_seconds,
        max_lifetime_hours: c.max_lifetime_hours,
        claim_wait_ms: c.claim_wait_ms,
    })
}

/// The router's settings from the config, with its secrets read.
pub(crate) fn router_settings(config: &Config) -> anyhow::Result<RouterSettings> {
    let role = Role::Router;
    let need = |key: &str| -> anyhow::Result<knoggin_core::Sensitive<String>> {
        config
            .secret(key, role)?
            .ok_or_else(|| anyhow!("{key} is required (set {key}_file)"))
    };
    let secret = InternalSecret::from_base64(need("internal.secret")?.expose().trim())
        .map_err(|_| anyhow!("internal.secret must be 32 bytes in standard base64"))?;
    let internal = InternalSettings {
        bind: parse_addr("internal.bind", &config.internal.bind)?,
        router_url: config.internal.router_url.clone(),
        secret,
        cert: need("internal.cert")?.expose().clone(),
        key: SecretText::new(need("internal.key")?.expose()),
    };
    let l = &config.limits;
    Ok(RouterSettings {
        trusted_proxies: config
            .server
            .trusted_proxies
            .iter()
            .map(|t| cidr("server.trusted_proxies", t))
            .collect::<anyhow::Result<_>>()?,
        image_digest: config.server.image_digest.clone(),
        limits: RouterLimits {
            per_address: RateSetting {
                rps: l.per_address_rps,
                burst: l.per_address_burst,
            },
            per_user: RateSetting {
                rps: l.per_user_rps,
                burst: l.per_user_burst,
            },
            upload_bytes_per_hour: l.upload_bytes_per_hour,
        },
        storage: storage_settings(config)?,
        retention: retention(config),
        containers: containers(config)?,
        internal,
        rollout: RolloutSettings {
            allow_downgrade: config.rollout.allow_downgrade,
        },
        log_level: log_level(config.log.level),
    })
}

fn parse_addr(name: &str, text: &str) -> anyhow::Result<SocketAddr> {
    text.parse()
        .map_err(|_| anyhow!("{name}: {text:?} is not an address and port"))
}

/// `launcher.*` as the router reads it.
enum LauncherSetting {
    /// One host: the launcher service's client.
    Docker(DockerLauncher),
    /// AWS: ECS.
    Ecs(EcsSettings),
}

/// `launcher.*`: on one host the launcher service's client, on AWS the ECS settings.
fn launcher_setting(config: &Config) -> anyhow::Result<LauncherSetting> {
    match config.launcher.kind {
        None => bail!("launcher.kind is required on a server: docker (one host) or ecs (AWS)"),
        Some(LauncherKind::Ecs) => return ecs_settings(config).map(LauncherSetting::Ecs),
        Some(LauncherKind::Docker) => {}
    }
    let token = config
        .secret("launcher.docker.token", Role::Router)?
        .ok_or_else(|| {
            anyhow!("launcher.kind = \"docker\" needs launcher.docker.token (or its _file)")
        })?;
    DockerLauncher::new(&config.launcher.docker.url, SecretText::new(token.expose()))
        .map(LauncherSetting::Docker)
        .map_err(|e| anyhow!("launcher.docker.url: {e}"))
}

/// `launcher.ecs.*` (tab "Deployment"): every value is needed, and containers reach the
/// router at `internal.router_url`.
fn ecs_settings(config: &Config) -> anyhow::Result<EcsSettings> {
    let e = &config.launcher.ecs;
    let need = |key: &str, value: &Option<String>| {
        value
            .clone()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow!("launcher.kind = \"ecs\" needs launcher.ecs.{key}"))
    };
    if e.subnets.is_empty() || e.security_groups.is_empty() {
        bail!(
            "launcher.kind = \"ecs\" needs launcher.ecs.subnets and launcher.ecs.security_groups"
        );
    }
    if config
        .internal
        .router_url
        .as_deref()
        .is_none_or(str::is_empty)
    {
        bail!("launcher.kind = \"ecs\" needs internal.router_url, the address containers use");
    }
    Ok(EcsSettings {
        cluster: need("cluster", &e.cluster)?,
        task_definition: need("task_definition", &e.task_definition)?,
        subnets: e.subnets.clone(),
        security_groups: e.security_groups.clone(),
        capacity_provider: need("capacity_provider", &e.capacity_provider)?,
    })
}

/// A task that ends when this is dropped, so that a role's helpers end with it.
pub(crate) struct Owned(tokio::task::JoinHandle<()>);

impl Drop for Owned {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A watch that turns true on SIGTERM or SIGINT, and the task that watches for them,
/// which ends when the returned [`Owned`] is dropped.
pub(crate) fn stop_signal() -> anyhow::Result<(watch::Receiver<bool>, Owned)> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).context("installing the stop signal handler")?;
    let mut int = signal(SignalKind::interrupt()).context("installing the stop signal handler")?;
    let (tx, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        let _ = tx.send(true);
    });
    Ok((rx, Owned(task)))
}

async fn stopped(mut rx: watch::Receiver<bool>) {
    let _ = rx.wait_for(|s| *s).await;
}

/// `knoggin router`.
pub(crate) async fn router(config: &Config, env: &Env) -> anyhow::Result<u8> {
    // Every setting first, then the connections.
    let launcher = launcher_setting(config)?;
    if matches!(launcher, LauncherSetting::Docker(_)) && config.containers.max == 0 {
        bail!(
            "with launcher.kind = \"docker\" containers.max must be set: one host has a fixed amount of memory"
        );
    }
    let keys = keys_setting(config)?;
    let state = state_setting(config, Role::Router)?;
    let settings = router_settings(config)?;
    let public_url = config
        .server
        .public_url
        .clone()
        .ok_or_else(|| anyhow!("server.public_url is required for the router"))?;
    let mut edge =
        crate::daemon::edge_config(config, public_url.clone(), env.test_mode, Role::Router)?;
    edge.role = edge::Role::Server;
    // `run_router` sets the rates per address and per user, and the idle time, from
    // `settings`.

    let clock = system_clock();
    let backends = Backends {
        state_db: state.name(),
        key_provider: keys.name(),
        launcher: match launcher {
            LauncherSetting::Docker(_) => "docker",
            LauncherSetting::Ecs(_) => "ecs",
        },
    };
    let keys = key_provider(&keys).await?;
    let db: Arc<dyn StateDb> = connect_state(&state, clock.clone(), true).await?.db;
    let bucket = storage(&settings.storage).await;
    // The sockets are bound now and listen only at the last start-up step.
    let public =
        reserve(config.bind()).with_context(|| format!("binding server.bind {}", config.bind()))?;
    let internal = reserve(settings.internal.bind)
        .with_context(|| format!("binding internal.bind {}", settings.internal.bind))?;
    let (metrics_listener, metrics, _upkeep) = match config.metrics_bind() {
        Some(addr) => {
            let handle = metrics_exporter_prometheus::PrometheusBuilder::new()
                .install_recorder()
                .context("installing the metrics recorder")?;
            let l = reserve(addr).with_context(|| format!("binding metrics.bind {addr}"))?;
            let h = handle.clone();
            let upkeep = Owned(tokio::spawn(async move {
                let mut every = tokio::time::interval(Duration::from_secs(5));
                loop {
                    every.tick().await;
                    h.run_upkeep();
                }
            }));
            (Some(l), Some(edge::metrics_router(handle)), Some(upkeep))
        }
        None => (None, None, None),
    };
    let (stop, _watching) = stop_signal()?;
    // On one host the launcher service's client and its `attach`; on AWS ECS, whose spec is
    // the router's own task definition.
    let (docker, launcher, one_host, spec): (_, Arc<dyn Launcher>, _, _) = match launcher {
        LauncherSetting::Docker(d) => {
            let d = Arc::new(d);
            (
                Some(d.clone()),
                d,
                Some("https://router:8443".to_owned()),
                String::new(),
            )
        }
        LauncherSetting::Ecs(e) => {
            let spec = e.task_definition.clone();
            (
                None,
                Arc::new(EcsLauncher::new(&aws_config().await, e)),
                None,
                spec,
            )
        }
    };
    // Start-up steps 7 and 8: the bucket self-test, then on one host the launcher's `attach`.
    let before_rollout = {
        let (bucket, clock) = (bucket.clone(), clock.clone());
        Box::pin(async move {
            let http = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| "no HTTP client".to_owned())?;
            knoggin_server::admin::bucket_selftest(
                &*bucket,
                &http,
                Timestamp::from_millis(clock()),
            )
            .await
            .map_err(|e| e.to_string())?;
            let Some(launcher) = docker else {
                return Ok(());
            };
            let deadline = tokio::time::Instant::now() + START_TRIES;
            loop {
                match launcher.attach().await {
                    Ok(()) => return Ok(()),
                    Err(knoggin_server::LaunchError::Unavailable)
                        if tokio::time::Instant::now() < deadline =>
                    {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    Err(e) => return Err(format!("the launcher's attach: {e}")),
                }
            }
        }) as futures_util::future::BoxFuture<'static, Result<(), String>>
    };
    let code = run_router(
        RouterStart {
            edge,
            settings,
            one_host,
            build: BuildInfo::new(env!("CARGO_PKG_VERSION")),
            spec,
            db,
            keys,
            storage: bucket,
            launcher,
            clock,
            listeners: Listeners {
                public,
                internal,
                metrics: metrics_listener,
            },
            metrics,
            before_rollout: Some(before_rollout),
            backends,
            timing: Timing::default(),
        },
        stopped(stop),
    )
    .await;
    Ok(exit_code(code))
}

fn exit_code(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(1)
}

/// `knoggin serve`, inside a user container: five environment values and the model.
pub(crate) async fn serve(config: &Config, env: &Env) -> anyhow::Result<u8> {
    use knoggin_server::container::{ServeConfig, ServeModel, ServeTimings, serve};
    let container_env = ContainerEnv::from_lookup(|k| env.vars.get(k).map(|v| v.expose().clone()))
        .map_err(|e| anyhow!("{e}: knoggin serve runs in a container a launcher started"))?;
    // The sender must outlive the role (a dropped sender makes the host's wait spin).
    let (model, _keep) = if env.test_mode {
        (ServeModel::fake(), None)
    } else {
        let library = crate::model::library_path(env.ort_dylib.as_deref())?;
        let m = crate::model::start(config.model.dir.clone(), config.model.download, library);
        (
            ServeModel::new(m.identity.clone(), m.state.clone(), m.providers.clone()),
            Some(m),
        )
    };
    let (stop, _watching) = stop_signal()?;
    let end = serve(
        SocketAddr::from(([0, 0, 0, 0], 8443)),
        ServeConfig {
            env: container_env,
            build: BuildInfo::new(env!("CARGO_PKG_VERSION")),
            test_mode: env.test_mode,
            local_address: None,
            model,
            timings: ServeTimings::default(),
        },
        stopped(stop),
    )
    .await;
    tracing::info!(kind = "end", reason = ?end.reason, "the container role ended");
    Ok(exit_code(end.exit_code))
}

/// `--every <n>s|m|h`.
pub(crate) fn parse_every(text: &str) -> Result<Duration, String> {
    let bad = || format!("{text:?} is a whole number followed by s, m or h, such as 10m");
    let (number, unit) = text.split_at(text.len().saturating_sub(1));
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let n: u64 = number.parse().map_err(|_| bad())?;
    let seconds = match unit {
        "s" => n,
        "m" => n.checked_mul(60).ok_or_else(bad)?,
        "h" => n.checked_mul(3600).ok_or_else(bad)?,
        _ => return Err(bad()),
    };
    if seconds == 0 {
        return Err(bad());
    }
    Ok(Duration::from_secs(seconds))
}

/// `knoggin lifecycle [--every <duration>]`.
pub(crate) async fn lifecycle(config: &Config, every: Option<Duration>) -> anyhow::Result<u8> {
    let state = state_setting(config, Role::Lifecycle)?;
    let storage_settings = storage_settings(config)?;
    let clock = system_clock();
    let state = connect_state(&state, clock.clone(), false).await?;
    let (stop, _watching) = stop_signal()?;
    let code = knoggin_server::lifecycle::run(
        knoggin_server::lifecycle::Lifecycle {
            db: state.db,
            postgres: state.postgres,
            storage: storage(&storage_settings).await,
            retention: retention(config),
        },
        every,
        clock,
        stop,
    )
    .await;
    Ok(exit_code(code))
}

/// `knoggin launcher`: the one-host launcher service.
pub(crate) async fn launcher(config: &Config, env: &Env) -> anyhow::Result<u8> {
    let token = config
        .secret("launcher.docker.token", Role::Launcher)?
        .ok_or_else(|| {
            anyhow!("launcher.docker.token is required (set launcher.docker.token_file)")
        })?;
    let settings = LauncherServiceSettings {
        bind: parse_addr("launcher.bind", &config.launcher.bind)?,
        token: SecretText::new(token.expose()),
        user_net_cidr: cidr("containers.user_net_cidr", &config.containers.user_net_cidr)?,
        memory_mib: config.containers.memory_mib,
    };
    // Docker sets a container's hostname to its ID.
    let own = std::fs::read_to_string("/etc/hostname")
        .context("reading /etc/hostname: the launcher runs in a container")?;
    let docker = bollard::Docker::connect_with_unix_defaults()
        .map_err(|e| anyhow!("the Docker socket: {e}"))?;
    let (stop, _watching) = stop_signal()?;
    let code =
        knoggin_server::launcher_service::run(settings, env.test_mode, docker, own.trim(), stop)
            .await;
    Ok(exit_code(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_takes_a_whole_number_and_a_unit() {
        assert_eq!(parse_every("10m"), Ok(Duration::from_secs(600)));
        assert_eq!(parse_every("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_every("2h"), Ok(Duration::from_secs(7200)));
        for bad in ["", "m", "10", "10x", "1.5m", "-1m", "0s", "10 m"] {
            assert!(parse_every(bad).is_err(), "{bad}");
        }
    }
}
