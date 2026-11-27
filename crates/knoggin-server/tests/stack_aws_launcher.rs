// SPDX-License-Identifier: AGPL-3.0-only
//! `EcsLauncher` against ECS on a moto container: a launch is one task of the user task
//! definition with the five environment values, the ID tag and `startedBy`; `describe` and
//! `list` report it with its address, host and ID; a draining host marks its tasks; a task
//! the API does not know is left out; a stop ends it with its reason, and the listing still
//! shows a stopped task. No AWS account: dummy credentials and moto's address are set here,
//! on the clients, and nowhere else.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use aws_config::Region;
use aws_sdk_ecs::config::Credentials;
use aws_sdk_ecs::types::{
    Compatibility, ContainerDefinition, ContainerInstanceStatus, NetworkMode, PortMapping,
    TaskField,
};
use knoggin_server::launcher::{
    ContainerEnv, ContainerSpec, EcsLauncher, EndReason, LaunchError, LaunchHandle, Launcher,
    LogLevel, RunState,
};
use knoggin_server::link::{CertPin, InternalSecret};
use knoggin_server::settings::EcsSettings;
use testcontainers::GenericImage;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use uuid::Uuid;

const CLUSTER: &str = "knoggin";

/// `<repository>@sha256:<digest>` from `tests/images.toml`.
fn pinned_image(name: &str) -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/images.toml");
    let file: toml::Table = std::fs::read_to_string(path).unwrap().parse().unwrap();
    file["images"][name].as_str().unwrap().to_owned()
}

/// A client config for moto alone: its address, a region and dummy credentials.
fn moto_config(endpoint: &str) -> aws_sdk_ecs::Config {
    EcsLauncher::with_timeouts(
        aws_sdk_ecs::config::Builder::new()
            .region(Region::new("us-east-1"))
            .credentials_provider(Credentials::new("testing", "testing", None, None, "moto"))
            .endpoint_url(endpoint),
    )
    .build()
}

/// One EC2 Query API call to moto (the instances that ECS hosts register; there is no EC2
/// SDK in the workspace). Returns the text of the first `<tag>` in the answer.
async fn ec2(http: &reqwest::Client, endpoint: &str, params: &[(&str, &str)], tag: &str) -> String {
    let mut form = vec![("Version", "2016-11-15")];
    form.extend_from_slice(params);
    let body = http
        .post(endpoint)
        // Moto picks the service and region from the credential scope; nothing is signed.
        .header(
            "Authorization",
            "AWS4-HMAC-SHA256 Credential=testing/20261126/us-east-1/ec2/aws4_request, \
             SignedHeaders=host, Signature=0",
        )
        .form(&form)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let open = format!("<{tag}>");
    let start = body.find(&open).unwrap_or_else(|| panic!("{body}")) + open.len();
    body[start..start + body[start..].find('<').unwrap()].to_owned()
}

fn spec(revision: &str) -> ContainerSpec {
    let id = Uuid::now_v7();
    ContainerSpec {
        container_id: id,
        spec: revision.into(),
        env: ContainerEnv {
            container_id: id,
            launch_secret: InternalSecret::from_bytes([9; 32]).launch_secret(id),
            router_url: "https://router.knoggin.internal:8443".into(),
            router_cert_sha256: CertPin::of(b"internal cert"),
            log_level: LogLevel::Info,
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs docker"]
async fn ecs_launcher_on_moto() {
    let image = pinned_image("moto");
    let (name, digest) = image.rsplit_once(':').unwrap();
    let container = GenericImage::new(name, digest)
        .with_exposed_port(5000.tcp())
        .with_wait_for(WaitFor::message_on_stderr("Running on"))
        .start()
        .await
        .unwrap();
    let port = container.get_host_port_ipv4(5000).await.unwrap();
    let endpoint = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    // Moto moves an ECS task one state on at each read by default; keep a task RUNNING
    // until it is stopped, as ECS does.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let set = http
            .post(format!("{endpoint}/moto-api/state-manager/set-transition"))
            .json(&serde_json::json!({
                "model_name": "ecs::task",
                "transition": { "progression": "manual", "times": 1_000_000 },
            }))
            .send()
            .await;
        match set {
            Ok(r) if r.status().is_success() => break,
            other if Instant::now() < deadline => {
                eprintln!("waiting for moto: {other:?}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            other => panic!("moto did not start: {other:?}"),
        }
    }

    // The cluster, two hosts, and the user task definition.
    let raw = aws_sdk_ecs::Client::from_conf(moto_config(&endpoint));
    raw.create_cluster()
        .cluster_name(CLUSTER)
        .send()
        .await
        .unwrap();
    let mut hosts = Vec::new();
    for _ in 0..2 {
        let instance = ec2(
            &http,
            &endpoint,
            &[
                ("Action", "RunInstances"),
                ("ImageId", "ami-12c6146b"),
                ("InstanceType", "m8g.xlarge"),
                ("MinCount", "1"),
                ("MaxCount", "1"),
            ],
            "instanceId",
        )
        .await;
        let doc = serde_json::json!({ "instanceId": instance, "region": "us-east-1" });
        let made = raw
            .register_container_instance()
            .cluster(CLUSTER)
            .instance_identity_document(doc.to_string())
            .send()
            .await
            .unwrap();
        hosts.push(
            made.container_instance
                .unwrap()
                .container_instance_arn
                .unwrap(),
        );
    }
    let registered = raw
        .register_task_definition()
        .family("knoggin-user")
        // The real definition is `awsvpc`; moto 5.2.5 fails every `awsvpc` task with a server
        // error (its network interface has no `private_dns_name`), so here it is `bridge`,
        // which moto runs without a network interface. The address is unit-tested.
        .network_mode(NetworkMode::Bridge)
        .requires_compatibilities(Compatibility::Ec2)
        .container_definitions(
            ContainerDefinition::builder()
                .name("store")
                .image("registry.example/knoggin@sha256:0000")
                .command("serve")
                .essential(true)
                .cpu(128)
                .memory(1024)
                .user("65532:65532")
                .privileged(false)
                .readonly_root_filesystem(true)
                .port_mappings(PortMapping::builder().container_port(8443).build())
                .stop_timeout(120)
                .build(),
        )
        .send()
        .await
        .unwrap();
    let revision = registered
        .task_definition
        .unwrap()
        .task_definition_arn
        .unwrap();

    let settings = EcsSettings {
        cluster: CLUSTER.into(),
        task_definition: revision.clone(),
        subnets: vec!["subnet-0a".into(), "subnet-0b".into()],
        security_groups: vec!["sg-0user".into()],
        capacity_provider: "knoggin-hosts".into(),
    };
    let ecs = EcsLauncher::from_conf(moto_config(&endpoint), settings.clone());

    // Launch: one task, the ID tag, `startedBy`, and exactly the five environment values.
    let first = spec(&revision);
    let handle = ecs.launch(&first).await.unwrap();
    let second = spec("");
    let other = ecs.launch(&second).await.unwrap();
    assert_ne!(handle, other);
    let shown = raw
        .describe_tasks()
        .cluster(CLUSTER)
        .tasks(&handle.0)
        .include(TaskField::Tags)
        .send()
        .await
        .unwrap();
    let task = &shown.tasks()[0];
    assert_eq!(task.started_by(), Some("knoggin-pool"));
    assert_eq!(task.task_definition_arn(), Some(revision.as_str()));
    let tags: Vec<(&str, &str)> = task
        .tags()
        .iter()
        .map(|t| (t.key().unwrap(), t.value().unwrap()))
        .collect();
    let id_text = first.container_id.to_string();
    assert_eq!(tags, [("knoggin:container-id", id_text.as_str())]);
    let overrides = task.overrides().unwrap().container_overrides();
    assert_eq!(overrides.len(), 1);
    assert_eq!(overrides[0].name(), Some("store"));
    let names: BTreeSet<&str> = overrides[0]
        .environment()
        .iter()
        .map(|kv| kv.name().unwrap())
        .collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "KNOGGIN_CONTAINER_ID",
            "KNOGGIN_LAUNCH_SECRET",
            "KNOGGIN_LOG__LEVEL",
            "KNOGGIN_ROUTER_CERT_SHA256",
            "KNOGGIN_ROUTER_URL",
        ])
    );

    // Describe: running, with its ID and its host. (No address: see the task definition.)
    let facts = ecs.describe(std::slice::from_ref(&handle)).await.unwrap();
    assert_eq!(facts.len(), 1);
    let f = &facts[0];
    assert_eq!(f.handle, handle);
    assert_eq!(f.container_id, Some(first.container_id));
    assert_eq!(f.state, RunState::Running);
    assert_eq!(f.address, None);
    let host = f.host.clone().unwrap();
    assert!(hosts.contains(&host));
    assert!(!f.draining);
    assert!(f.ended.is_none());

    // A task the API does not know is left out: the router treats it as unknown.
    let unknown = LaunchHandle(format!(
        "arn:aws:ecs:us-east-1:123456789012:task/{CLUSTER}/{}",
        Uuid::new_v4().simple()
    ));
    let facts = ecs
        .describe(&[unknown.clone(), handle.clone()])
        .await
        .unwrap();
    assert_eq!(
        facts.iter().map(|i| &i.handle).collect::<Vec<_>>(),
        [&handle]
    );

    // List: both tasks, each with its ID.
    let listed = ecs.list().await.unwrap();
    let ids: BTreeSet<Option<Uuid>> = listed.iter().map(|i| i.container_id).collect();
    assert_eq!(
        ids,
        BTreeSet::from([Some(first.container_id), Some(second.container_id)])
    );

    // A draining host marks its tasks, in describe and in list.
    raw.update_container_instances_state()
        .cluster(CLUSTER)
        .container_instances(&host)
        .status(ContainerInstanceStatus::Draining)
        .send()
        .await
        .unwrap();
    assert!(ecs.describe(std::slice::from_ref(&handle)).await.unwrap()[0].draining);
    for i in ecs.list().await.unwrap() {
        assert_eq!(
            i.draining,
            i.host.as_deref() == Some(host.as_str()),
            "{i:?}"
        );
    }

    // Stop with a reason: the task ends, and the listing of stopped tasks still shows it.
    ecs.stop(&handle, "drain").await.unwrap();
    let f = &ecs.describe(std::slice::from_ref(&handle)).await.unwrap()[0];
    assert_eq!(f.state, RunState::Ended);
    let ended = f.ended.unwrap();
    assert_ne!(ended.reason, EndReason::Memory);
    let listed = ecs.list().await.unwrap();
    let stopped = listed.iter().find(|i| i.handle == handle).unwrap();
    assert_eq!(stopped.state, RunState::Ended);
    assert_eq!(stopped.container_id, Some(first.container_id));
    let shown = raw
        .describe_tasks()
        .cluster(CLUSTER)
        .tasks(&handle.0)
        .send()
        .await
        .unwrap();
    assert_eq!(shown.tasks()[0].stopped_reason(), Some("drain"));

    // A stopped reason that names memory, in any letter case, is an end for memory. (Moto
    // copies the stop's reason into `stoppedReason`; on ECS the agent writes it.)
    ecs.stop(
        &other,
        "OutOfMemoryError: Container killed due to memory usage",
    )
    .await
    .unwrap();
    let f = &ecs.describe(std::slice::from_ref(&other)).await.unwrap()[0];
    assert_eq!(f.ended.unwrap().reason, EndReason::Memory);

    // A launch of a task definition that does not exist is refused.
    assert!(matches!(
        ecs.launch(&spec("knoggin-user:99")).await,
        Err(LaunchError::Refused(_))
    ));
    // Stopping a task the API does not know fails. Real ECS answers
    // InvalidParameterException (`NotFound`, unit-tested); moto fails with a server error.
    assert!(ecs.stop(&unknown, "stray").await.is_err());

    container.rm().await.unwrap();
}
