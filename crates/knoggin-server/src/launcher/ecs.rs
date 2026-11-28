// SPDX-License-Identifier: AGPL-3.0-only
//! `EcsLauncher`: user containers as ECS tasks of the family `knoggin-user`. `launch` is one
//! `RunTask` with the capacity provider, `binpack` on memory (except on Fargate, which places
//! each task on its own), the user subnets and security groups without a public address,
//! `startedBy` `knoggin-pool`, the tag `knoggin:container-id` and a container override that
//! sets the five environment values and nothing else. No user identifier
//! goes into a tag, an override or `startedBy`: those show in the console and in CloudTrail.
//! `describe` and `list` read `DescribeTasks` (with tags) and mark the tasks whose host is
//! `DRAINING` (`DescribeContainerInstances`); a task the API does not know is left out, so the
//! router treats it as unknown. `stop` is `StopTask` with the reason.

use std::collections::{BTreeSet, HashMap};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use async_trait::async_trait;
use aws_config::SdkConfig;
use aws_sdk_ecs::Client;
use aws_sdk_ecs::config::timeout::TimeoutConfig;
use aws_sdk_ecs::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_ecs::types::{
    AssignPublicIp, AwsVpcConfiguration, CapacityProviderStrategyItem, ContainerOverride,
    DesiredStatus, KeyValuePair, NetworkConfiguration, PlacementStrategy, PlacementStrategyType,
    Tag, Task, TaskField, TaskOverride, TaskStopCode,
};
use knoggin_core::Timestamp;
use uuid::Uuid;

use super::{
    ContainerInfo, ContainerSpec, EndReason, Ended, LaunchError, LaunchHandle, Launcher, RunState,
};
use crate::settings::EcsSettings;

/// The tag that carries the Knoggin container ID.
pub const ID_TAG: &str = "knoggin:container-id";
/// `startedBy` of every user task.
pub const STARTED_BY: &str = "knoggin-pool";
/// The user task definition's one container.
pub const CONTAINER_NAME: &str = "store";
/// The port a user container listens on.
pub const PORT: u16 = 8443;
/// `DescribeTasks` and `DescribeContainerInstances` take at most 100 at a time.
const DESCRIBE_BATCH: usize = 100;
/// One try of a call.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
/// One call with its retries (the describe and list calls allow about 20 a second).
const OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

/// The launcher on AWS.
#[derive(Debug, Clone)]
pub struct EcsLauncher {
    client: Client,
    settings: EcsSettings,
}

impl EcsLauncher {
    /// A launcher for `launcher.ecs.*` with the shared config.
    pub fn new(sdk: &SdkConfig, settings: EcsSettings) -> Self {
        Self::from_conf(
            Self::with_timeouts(aws_sdk_ecs::config::Builder::from(sdk)).build(),
            settings,
        )
    }

    /// Adds the behaviour version and the time limits to a client config.
    pub fn with_timeouts(conf: aws_sdk_ecs::config::Builder) -> aws_sdk_ecs::config::Builder {
        conf.behavior_version(crate::aws::behavior_version())
            .timeout_config(
                TimeoutConfig::builder()
                    .connect_timeout(crate::link::CONNECT_TIMEOUT)
                    .read_timeout(ATTEMPT_TIMEOUT)
                    .operation_attempt_timeout(ATTEMPT_TIMEOUT)
                    .operation_timeout(OPERATION_TIMEOUT)
                    .build(),
            )
    }

    /// A launcher over a ready client config.
    pub fn from_conf(conf: aws_sdk_ecs::Config, settings: EcsSettings) -> Self {
        Self {
            client: Client::from_conf(conf),
            settings,
        }
    }

    /// The task definition family of `launcher.ecs.task_definition` (`knoggin-user`), from a
    /// family, a `family:revision` or an ARN.
    fn family(&self) -> &str {
        family(&self.settings.task_definition)
    }

    /// The facts of these tasks, with the draining mark of their hosts.
    async fn facts(&self, arns: &[String]) -> Result<Vec<ContainerInfo>, LaunchError> {
        let mut tasks = Vec::new();
        for chunk in arns.chunks(DESCRIBE_BATCH) {
            let answer = self
                .client
                .describe_tasks()
                .cluster(&self.settings.cluster)
                .set_tasks(Some(chunk.to_vec()))
                .include(TaskField::Tags)
                .send()
                .await
                .map_err(|e| ecs_err(&e))?;
            // A task in `failures` (MISSING) is unknown: it is left out.
            tasks.extend(answer.tasks.unwrap_or_default());
        }
        let hosts: Vec<String> = tasks
            .iter()
            .filter_map(|t| t.container_instance_arn().map(str::to_owned))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut draining = HashMap::new();
        for chunk in hosts.chunks(DESCRIBE_BATCH) {
            let answer = self
                .client
                .describe_container_instances()
                .cluster(&self.settings.cluster)
                .set_container_instances(Some(chunk.to_vec()))
                .send()
                .await
                .map_err(|e| ecs_err(&e))?;
            for host in answer.container_instances() {
                if let Some(arn) = host.container_instance_arn() {
                    draining.insert(arn.to_owned(), host.status() == Some("DRAINING"));
                }
            }
        }
        Ok(tasks
            .iter()
            .filter_map(|t| {
                let mut info = info(t)?;
                info.draining = info
                    .host
                    .as_ref()
                    .is_some_and(|h| draining.get(h).copied().unwrap_or(false));
                Some(info)
            })
            .collect())
    }

    async fn list_arns(&self, status: DesiredStatus) -> Result<Vec<String>, LaunchError> {
        let mut arns = Vec::new();
        let mut token = None;
        loop {
            let answer = self
                .client
                .list_tasks()
                .cluster(&self.settings.cluster)
                .family(self.family())
                .desired_status(status.clone())
                .max_results(100)
                .set_next_token(token.take())
                .send()
                .await
                .map_err(|e| ecs_err(&e))?;
            arns.extend(answer.task_arns().iter().cloned());
            match answer.next_token {
                Some(t) if !t.is_empty() => token = Some(t),
                _ => return Ok(arns),
            }
        }
    }
}

/// `knoggin-user` from `knoggin-user`, `knoggin-user:3` or
/// `arn:aws:ecs:<region>:<account>:task-definition/knoggin-user:3`.
fn family(task_definition: &str) -> &str {
    let name = task_definition
        .rsplit_once("task-definition/")
        .map_or(task_definition, |(_, n)| n);
    name.split_once(':').map_or(name, |(f, _)| f)
}

/// The `RunTask` call `launch` sends, built apart so that a unit test can read it.
fn run_task(
    client: &Client,
    settings: &EcsSettings,
    spec: &ContainerSpec,
) -> Result<aws_sdk_ecs::operation::run_task::builders::RunTaskFluentBuilder, LaunchError> {
    let refused = |_| LaunchError::Refused("a launch setting is missing");
    let task_definition = if spec.spec.is_empty() {
        &settings.task_definition
    } else {
        &spec.spec
    };
    let environment = spec
        .env
        .to_vars()
        .iter()
        .map(|(name, value)| {
            KeyValuePair::builder()
                .name(*name)
                .value(value.as_str())
                .build()
        })
        .collect();
    // Fargate takes no placement strategy. AWS reserves capacity provider names that start
    // with `FARGATE`, so these two can only mean Fargate.
    let binpack = (!matches!(
        settings.capacity_provider.as_str(),
        "FARGATE" | "FARGATE_SPOT"
    ))
    .then(|| {
        vec![
            PlacementStrategy::builder()
                .r#type(PlacementStrategyType::Binpack)
                .field("memory")
                .build(),
        ]
    });
    Ok(client
        .run_task()
        .cluster(&settings.cluster)
        .task_definition(task_definition)
        .count(1)
        .capacity_provider_strategy(
            CapacityProviderStrategyItem::builder()
                .capacity_provider(&settings.capacity_provider)
                .weight(1)
                .build()
                .map_err(refused)?,
        )
        .set_placement_strategy(binpack)
        .network_configuration(
            NetworkConfiguration::builder()
                .awsvpc_configuration(
                    AwsVpcConfiguration::builder()
                        .set_subnets(Some(settings.subnets.clone()))
                        .set_security_groups(Some(settings.security_groups.clone()))
                        .assign_public_ip(AssignPublicIp::Disabled)
                        .build()
                        .map_err(refused)?,
                )
                .build(),
        )
        .started_by(STARTED_BY)
        .overrides(
            TaskOverride::builder()
                .container_overrides(
                    ContainerOverride::builder()
                        .name(CONTAINER_NAME)
                        .set_environment(Some(environment))
                        .build(),
                )
                .build(),
        )
        .tags(
            Tag::builder()
                .key(ID_TAG)
                .value(spec.container_id.hyphenated().to_string())
                .build(),
        ))
}

/// The facts of one task. `None` for a task without an ARN, which ECS never sends.
fn info(task: &Task) -> Option<ContainerInfo> {
    let handle = LaunchHandle(task.task_arn()?.to_owned());
    let container = task.containers().first();
    let state = match task.last_status() {
        Some("RUNNING") => RunState::Running,
        Some("STOPPED") => RunState::Ended,
        // Placed, pulling, or stopping: not running yet, or not ended yet.
        _ => RunState::Pending,
    };
    let container_id = task
        .tags()
        .iter()
        .find(|t| t.key() == Some(ID_TAG))
        .and_then(|t| t.value())
        .and_then(|v| Uuid::parse_str(v).ok());
    let ended = (state == RunState::Ended).then(|| {
        let memory = [task.stopped_reason(), container.and_then(|c| c.reason())]
            .into_iter()
            .flatten()
            .any(|r| r.to_ascii_lowercase().contains("outofmemory"));
        Ended {
            reason: if memory {
                EndReason::Memory
            } else if task.stop_code() == Some(&TaskStopCode::UserInitiated) {
                EndReason::Stopped
            } else {
                EndReason::Exited
            },
            exit_code: container.and_then(|c| c.exit_code()).map(i64::from),
        }
    });
    Some(ContainerInfo {
        handle,
        container_id,
        state,
        address: address(task),
        host: task.container_instance_arn().map(str::to_owned),
        draining: false,
        started_at: task
            .started_at()
            .and_then(|t| t.to_millis().ok())
            .map(Timestamp::from_millis),
        ended,
    })
}

/// `containers[0].networkInterfaces[0].privateIpv4Address`, port 8443. The task's network
/// interface attachment names the same address; it is read when the container does not.
fn address(task: &Task) -> Option<SocketAddrV4> {
    let from_container = task
        .containers()
        .first()
        .and_then(|c| c.network_interfaces().first())
        .and_then(|n| n.private_ipv4_address());
    let from_attachment = || {
        task.attachments()
            .iter()
            .filter(|a| a.r#type() == Some("ElasticNetworkInterface"))
            .flat_map(|a| a.details())
            .find(|d| d.name() == Some("privateIPv4Address"))
            .and_then(|d| d.value())
    };
    let ip: Ipv4Addr = from_container.or_else(from_attachment)?.parse().ok()?;
    Some(SocketAddrV4::new(ip, PORT))
}

/// A server error, a throttle that outlasted the retries, or no answer: the launcher cannot
/// be reached. Anything else is a refusal, with ECS's code only.
fn ecs_err<E: ProvideErrorMetadata, R>(e: &SdkError<E, R>) -> LaunchError {
    match e {
        SdkError::ServiceError(s) => match s.err().code() {
            Some("ServerException" | "ThrottlingException" | "ServiceUnavailableException") => {
                LaunchError::Unavailable
            }
            _ => LaunchError::Refused("ECS refused the call"),
        },
        _ => LaunchError::Unavailable,
    }
}

#[async_trait]
impl Launcher for EcsLauncher {
    async fn launch(&self, spec: &ContainerSpec) -> Result<LaunchHandle, LaunchError> {
        let answer = run_task(&self.client, &self.settings, spec)?
            .send()
            .await
            .map_err(|e| ecs_err(&e))?;
        if !answer.failures().is_empty() {
            return Err(LaunchError::Refused("ECS could not place the task"));
        }
        answer
            .tasks()
            .first()
            .and_then(Task::task_arn)
            .map(|arn| LaunchHandle(arn.to_owned()))
            .ok_or(LaunchError::Refused("ECS started no task"))
    }

    async fn describe(&self, handles: &[LaunchHandle]) -> Result<Vec<ContainerInfo>, LaunchError> {
        if handles.is_empty() {
            return Ok(Vec::new());
        }
        let arns: Vec<String> = handles.iter().map(|h| h.0.clone()).collect();
        self.facts(&arns).await
    }

    async fn list(&self) -> Result<Vec<ContainerInfo>, LaunchError> {
        // A task told to stop has the desired status STOPPED while it still parks, and the
        // default listing leaves it out: both listings are needed.
        let mut arns = self.list_arns(DesiredStatus::Running).await?;
        arns.extend(self.list_arns(DesiredStatus::Stopped).await?);
        let arns: Vec<String> = arns
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if arns.is_empty() {
            return Ok(Vec::new());
        }
        self.facts(&arns).await
    }

    async fn stop(&self, handle: &LaunchHandle, reason: &str) -> Result<(), LaunchError> {
        self.client
            .stop_task()
            .cluster(&self.settings.cluster)
            .task(&handle.0)
            .reason(reason)
            .send()
            .await
            .map(drop)
            .map_err(|e| match ecs_err(&e) {
                // The task named is not in this cluster.
                LaunchError::Refused(_)
                    if e.as_service_error().and_then(|s| s.code())
                        == Some("InvalidParameterException") =>
                {
                    LaunchError::NotFound
                }
                other => other,
            })
    }
}

#[cfg(test)]
mod tests {
    use aws_config::Region;
    use aws_sdk_ecs::config::Credentials;
    use aws_sdk_ecs::primitives::DateTime;
    use aws_sdk_ecs::types::{Attachment, Container, NetworkInterface};
    use serde_json::{Value, json};
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::launcher::{ContainerEnv, LogLevel};
    use crate::link::{CertPin, InternalSecret};

    fn settings() -> EcsSettings {
        EcsSettings {
            cluster: "knoggin".into(),
            task_definition: "arn:aws:ecs:eu-west-1:111122223333:task-definition/knoggin-user:3"
                .into(),
            subnets: vec!["subnet-a".into(), "subnet-b".into()],
            security_groups: vec!["sg-user".into()],
            capacity_provider: "knoggin-hosts".into(),
        }
    }

    async fn stub() -> (MockServer, EcsLauncher) {
        stub_with(settings()).await
    }

    async fn stub_with(settings: EcsSettings) -> (MockServer, EcsLauncher) {
        let server = MockServer::start().await;
        let conf = EcsLauncher::with_timeouts(
            aws_sdk_ecs::config::Builder::new()
                .region(Region::new("eu-west-1"))
                .credentials_provider(Credentials::new("AKIDTEST", "secret", None, None, "t"))
                .endpoint_url(server.uri())
                .retry_config(
                    aws_sdk_ecs::config::retry::RetryConfig::standard()
                        .with_max_attempts(2)
                        .with_initial_backoff(Duration::from_millis(1)),
                ),
        )
        .build();
        (server, EcsLauncher::from_conf(conf, settings))
    }

    fn answer(status: u16, body: Value) -> ResponseTemplate {
        ResponseTemplate::new(status)
            .insert_header("content-type", "application/x-amz-json-1.1")
            .set_body_string(body.to_string())
    }

    fn target(name: &str) -> String {
        format!("AmazonEC2ContainerServiceV20141113.{name}")
    }

    async fn calls(server: &MockServer) -> Vec<(String, Value)> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r.headers["x-amz-target"]
                        .to_str()
                        .unwrap()
                        .trim_start_matches("AmazonEC2ContainerServiceV20141113.")
                        .to_owned(),
                    serde_json::from_slice(&r.body).unwrap(),
                )
            })
            .collect()
    }

    fn spec(revision: &str) -> ContainerSpec {
        let id = Uuid::now_v7();
        ContainerSpec {
            container_id: id,
            spec: revision.into(),
            env: ContainerEnv {
                container_id: id,
                launch_secret: InternalSecret::from_bytes([3; 32]).launch_secret(id),
                router_url: "https://router.knoggin.internal:8443".into(),
                router_cert_sha256: CertPin::of(b"cert"),
                log_level: LogLevel::Info,
            },
        }
    }

    #[test]
    fn the_family_of_a_task_definition() {
        assert_eq!(family("knoggin-user"), "knoggin-user");
        assert_eq!(family("knoggin-user:12"), "knoggin-user");
        assert_eq!(
            family("arn:aws:ecs:eu-west-1:111122223333:task-definition/knoggin-user:3"),
            "knoggin-user"
        );
    }

    /// `RunTask` carries exactly this: the revision of the spec, one task, the
    /// capacity provider, binpack on memory, the network without a public address,
    /// `startedBy`, the ID tag, and an override of the five environment values only.
    #[tokio::test]
    async fn run_task_carries_the_launch_fields_and_nothing_of_the_user() {
        let (server, ecs) = stub().await;
        let arn = "arn:aws:ecs:eu-west-1:111122223333:task/knoggin/abc";
        Mock::given(header("x-amz-target", target("RunTask").as_str()))
            .respond_with(answer(
                200,
                json!({ "tasks": [{ "taskArn": arn }], "failures": [] }),
            ))
            .mount(&server)
            .await;
        let s = spec("knoggin-user:7");
        assert_eq!(ecs.launch(&s).await.unwrap(), LaunchHandle(arn.into()));
        let mut sent = calls(&server).await;
        let body = &mut sent[0].1;
        // The SDK fills the idempotency token, so its own retries never start a second task.
        let token = body.as_object_mut().unwrap().remove("clientToken");
        assert!(token.is_some_and(|t| t.as_str().is_some_and(|t| !t.is_empty())));
        let env: Vec<Value> = s
            .env
            .to_vars()
            .iter()
            .map(|(k, v)| json!({ "name": k, "value": v.as_str() }))
            .collect();
        assert_eq!(
            *body,
            json!({
                "cluster": "knoggin",
                "taskDefinition": "knoggin-user:7",
                "count": 1,
                "capacityProviderStrategy": [{ "capacityProvider": "knoggin-hosts", "weight": 1, "base": 0 }],
                "placementStrategy": [{ "type": "binpack", "field": "memory" }],
                "networkConfiguration": { "awsvpcConfiguration": {
                    "subnets": ["subnet-a", "subnet-b"],
                    "securityGroups": ["sg-user"],
                    "assignPublicIp": "DISABLED",
                } },
                "startedBy": "knoggin-pool",
                "overrides": { "containerOverrides": [{ "name": "store", "environment": env }] },
                "tags": [{ "key": "knoggin:container-id", "value": s.container_id.to_string() }],
            })
        );
        assert_eq!(env.len(), 5);

        // An empty spec falls back to the router's own task definition.
        server.reset().await;
        Mock::given(method("POST"))
            .respond_with(answer(200, json!({ "tasks": [{ "taskArn": arn }] })))
            .mount(&server)
            .await;
        ecs.launch(&spec("")).await.unwrap();
        assert_eq!(
            calls(&server).await[0].1["taskDefinition"],
            settings().task_definition
        );
    }

    /// On Fargate `RunTask` carries the same capacity provider strategy and no placement strategy.
    #[tokio::test]
    async fn run_task_on_fargate_has_no_placement_strategy() {
        for provider in ["FARGATE", "FARGATE_SPOT"] {
            let (server, ecs) = stub_with(EcsSettings {
                capacity_provider: provider.into(),
                ..settings()
            })
            .await;
            Mock::given(method("POST"))
                .respond_with(answer(
                    200,
                    json!({ "tasks": [{ "taskArn": "arn:aws:ecs:eu-west-1:111122223333:task/knoggin/abc" }] }),
                ))
                .mount(&server)
                .await;
            ecs.launch(&spec("knoggin-user:7")).await.unwrap();
            let body = &calls(&server).await[0].1;
            assert_eq!(body.get("placementStrategy"), None, "{provider}");
            assert_eq!(
                body["capacityProviderStrategy"],
                json!([{ "capacityProvider": provider, "weight": 1, "base": 0 }])
            );
        }
    }

    #[tokio::test]
    async fn launch_errors() {
        let (server, ecs) = stub().await;
        let cases = [
            (
                answer(
                    200,
                    json!({ "tasks": [], "failures": [{ "reason": "RESOURCE:MEMORY" }] }),
                ),
                LaunchError::Refused("ECS could not place the task"),
            ),
            (
                answer(200, json!({ "tasks": [] })),
                LaunchError::Refused("ECS started no task"),
            ),
            (
                answer(400, json!({ "__type": "InvalidParameterException" })),
                LaunchError::Refused("ECS refused the call"),
            ),
            (
                answer(400, json!({ "__type": "AccessDeniedException" })),
                LaunchError::Refused("ECS refused the call"),
            ),
            (
                answer(500, json!({ "__type": "ServerException" })),
                LaunchError::Unavailable,
            ),
            (
                answer(400, json!({ "__type": "ThrottlingException" })),
                LaunchError::Unavailable,
            ),
        ];
        for (response, expected) in cases {
            server.reset().await;
            Mock::given(method("POST"))
                .respond_with(response)
                .mount(&server)
                .await;
            assert_eq!(ecs.launch(&spec("x:1")).await.unwrap_err(), expected);
        }
    }

    fn task(arn: &str, status: &str) -> aws_sdk_ecs::types::builders::TaskBuilder {
        Task::builder().task_arn(arn).last_status(status)
    }

    #[test]
    fn task_facts() {
        let id = Uuid::now_v7();
        let running = task("t1", "RUNNING")
            .tags(Tag::builder().key("other").value("x").build())
            .tags(Tag::builder().key(ID_TAG).value(id.to_string()).build())
            .container_instance_arn("ci-1")
            .started_at(DateTime::from_millis(1_800_000_000_123))
            .containers(
                Container::builder()
                    .network_interfaces(
                        NetworkInterface::builder()
                            .private_ipv4_address("10.0.4.17")
                            .build(),
                    )
                    .build(),
            )
            .build();
        let i = info(&running).unwrap();
        assert_eq!(i.handle, LaunchHandle("t1".into()));
        assert_eq!(i.container_id, Some(id));
        assert_eq!(i.state, RunState::Running);
        assert_eq!(i.address, Some("10.0.4.17:8443".parse().unwrap()));
        assert_eq!(i.host.as_deref(), Some("ci-1"));
        assert_eq!(
            i.started_at,
            Some(Timestamp::from_millis(1_800_000_000_123))
        );
        assert!(i.ended.is_none() && !i.draining);

        // Before the container reports its interface, the ENI attachment names the address.
        let attached = task("t2", "PENDING")
            .attachments(
                Attachment::builder()
                    .r#type("ElasticNetworkInterface")
                    .details(KeyValuePair::builder().name("subnetId").value("s").build())
                    .details(
                        KeyValuePair::builder()
                            .name("privateIPv4Address")
                            .value("10.0.5.9")
                            .build(),
                    )
                    .build(),
            )
            .build();
        let i = info(&attached).unwrap();
        assert_eq!(i.state, RunState::Pending);
        assert_eq!(i.address, Some("10.0.5.9:8443".parse().unwrap()));
        assert_eq!(i.container_id, None);

        // Only STOPPED is an end; STOPPING still runs its park sequence.
        for status in [
            "PROVISIONING",
            "PENDING",
            "ACTIVATING",
            "DEACTIVATING",
            "STOPPING",
            "DEPROVISIONING",
        ] {
            assert_eq!(
                info(&task("t", status).build()).unwrap().state,
                RunState::Pending
            );
        }
        assert!(info(&Task::builder().last_status("RUNNING").build()).is_none());

        let ended = |reason: Option<&str>, container_reason: Option<&str>, code| {
            let mut c = Container::builder().exit_code(137);
            if let Some(r) = container_reason {
                c = c.reason(r);
            }
            let mut t = task("t", "STOPPED").containers(c.build());
            if let Some(r) = reason {
                t = t.stopped_reason(r);
            }
            if let Some(code) = code {
                t = t.stop_code(code);
            }
            info(&t.build()).unwrap().ended.unwrap()
        };
        assert_eq!(
            ended(
                None,
                Some("OutOfMemoryError: Container killed due to memory usage"),
                None
            ),
            Ended {
                reason: EndReason::Memory,
                exit_code: Some(137)
            }
        );
        assert_eq!(
            ended(
                Some("task outofmemory"),
                None,
                Some(TaskStopCode::UserInitiated)
            )
            .reason,
            EndReason::Memory
        );
        assert_eq!(
            ended(Some("drain"), None, Some(TaskStopCode::UserInitiated)).reason,
            EndReason::Stopped
        );
        assert_eq!(
            ended(
                Some("Essential container in task exited"),
                None,
                Some(TaskStopCode::EssentialContainerExited)
            )
            .reason,
            EndReason::Exited
        );
        let bad = task("t", "RUNNING")
            .containers(
                Container::builder()
                    .network_interfaces(
                        NetworkInterface::builder()
                            .private_ipv4_address("fe80::1")
                            .build(),
                    )
                    .build(),
            )
            .build();
        assert_eq!(info(&bad).unwrap().address, None);
    }

    /// `list` asks for both desired statuses of the family, pages, describes every task with
    /// its tags in batches of 100, and marks the tasks on draining hosts; a task the API
    /// reports missing is left out.
    #[tokio::test]
    async fn list_and_describe() {
        let (server, ecs) = stub().await;
        let arns: Vec<String> = (0..150).map(|i| format!("arn:task/{i:03}")).collect();
        Mock::given(header("x-amz-target", target("ListTasks").as_str()))
            .and(wiremock::matchers::body_partial_json(
                json!({ "desiredStatus": "RUNNING" }),
            ))
            .and(wiremock::matchers::body_partial_json(
                json!({ "nextToken": "p2" }),
            ))
            .respond_with(answer(200, json!({ "taskArns": arns[100..140] })))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(header("x-amz-target", target("ListTasks").as_str()))
            .and(wiremock::matchers::body_partial_json(
                json!({ "desiredStatus": "RUNNING" }),
            ))
            .respond_with(answer(
                200,
                json!({ "taskArns": arns[..100], "nextToken": "p2" }),
            ))
            .with_priority(2)
            .mount(&server)
            .await;
        Mock::given(header("x-amz-target", target("ListTasks").as_str()))
            .and(wiremock::matchers::body_partial_json(
                json!({ "desiredStatus": "STOPPED" }),
            ))
            .respond_with(answer(
                200,
                json!({ "taskArns": [arns[0].clone(), arns[149].clone()] }),
            ))
            .mount(&server)
            .await;
        let described =
            |arns: &[String]| -> Vec<Value> {
                arns.iter()
                .filter(|a| !a.ends_with("007"))
                .map(|a| {
                    let host = if a.ends_with('1') { "ci-drain" } else { "ci-ok" };
                    json!({ "taskArn": a, "lastStatus": "RUNNING", "containerInstanceArn": host })
                })
                .collect()
            };
        Mock::given(header("x-amz-target", target("DescribeTasks").as_str()))
            .respond_with(move |r: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&r.body).unwrap();
                let asked: Vec<String> = serde_json::from_value(body["tasks"].clone()).unwrap();
                answer(
                    200,
                    json!({ "tasks": described(&asked), "failures": [{ "reason": "MISSING" }] }),
                )
            })
            .mount(&server)
            .await;
        Mock::given(header(
            "x-amz-target",
            target("DescribeContainerInstances").as_str(),
        ))
        .respond_with(answer(
            200,
            json!({ "containerInstances": [
                { "containerInstanceArn": "ci-drain", "status": "DRAINING" },
                { "containerInstanceArn": "ci-ok", "status": "ACTIVE" },
            ] }),
        ))
        .mount(&server)
        .await;

        let listed = ecs.list().await.unwrap();
        assert_eq!(
            listed.len(),
            140,
            "141 distinct running and stopped, one missing"
        );
        assert!(
            listed
                .iter()
                .all(|i| i.draining == i.handle.0.ends_with('1'))
        );
        let sent = calls(&server).await;
        let lists: Vec<&Value> = sent
            .iter()
            .filter(|(t, _)| t == "ListTasks")
            .map(|(_, b)| b)
            .collect();
        assert_eq!(lists.len(), 3);
        for b in &lists {
            assert_eq!(b["cluster"], "knoggin");
            assert_eq!(b["family"], "knoggin-user");
        }
        let describes: Vec<&Value> = sent
            .iter()
            .filter(|(t, _)| t == "DescribeTasks")
            .map(|(_, b)| b)
            .collect();
        assert_eq!(describes.len(), 2);
        assert_eq!(describes[0]["tasks"].as_array().unwrap().len(), 100);
        assert_eq!(describes[0]["include"], json!(["TAGS"]));
        let hosts: Vec<&Value> = sent
            .iter()
            .filter(|(t, _)| t == "DescribeContainerInstances")
            .map(|(_, b)| b)
            .collect();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0]["containerInstances"], json!(["ci-drain", "ci-ok"]));

        server.reset().await;
        assert!(ecs.describe(&[]).await.unwrap().is_empty());
        assert!(calls(&server).await.is_empty());
    }

    #[tokio::test]
    async fn stop_names_the_task_and_the_reason() {
        let (server, ecs) = stub().await;
        Mock::given(method("POST"))
            .respond_with(answer(200, json!({ "task": {} })))
            .mount(&server)
            .await;
        ecs.stop(&LaunchHandle("arn:task/1".into()), "drain")
            .await
            .unwrap();
        assert_eq!(
            calls(&server).await,
            [(
                "StopTask".to_owned(),
                json!({ "cluster": "knoggin", "task": "arn:task/1", "reason": "drain" })
            )]
        );
        for (response, expected) in [
            (
                answer(400, json!({ "__type": "InvalidParameterException" })),
                LaunchError::NotFound,
            ),
            (
                answer(400, json!({ "__type": "ClusterNotFoundException" })),
                LaunchError::Refused("ECS refused the call"),
            ),
            (
                answer(500, json!({ "__type": "ServerException" })),
                LaunchError::Unavailable,
            ),
        ] {
            server.reset().await;
            Mock::given(method("POST"))
                .respond_with(response)
                .mount(&server)
                .await;
            assert_eq!(
                ecs.stop(&LaunchHandle("t".into()), "drain")
                    .await
                    .unwrap_err(),
                expected
            );
        }
    }
}
