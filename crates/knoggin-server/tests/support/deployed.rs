// SPDX-License-Identifier: AGPL-3.0-only
//! The isolation tests against a deployed AWS stack: `KNOGGIN_STACK_URL` names the stack and
//! `KNOGGIN_STACK_CREDENTIALS` a JSON file of test credentials. Without both, `stack_isolation.rs`
//! starts the one-host stack.
//!
//! The AWS run adds a network probe that ECS allows: a one-off task of the pinned
//! `busybox`, copied by digest into the deployment's ECR repository, run where the user
//! containers run (the user hosts with the EC2 launch type, or Fargate with the user execution
//! role to pull the image), in `awsvpc` mode, with the user security group and subnets and no
//! task role. Its exit code is the result, so it needs no log group: 0 when every check holds,
//! otherwise the number of the first check that failed.
//!
//! The AWS calls use the SDK's default chain with the `.cargo/config.toml` settings (no
//! shared files, no instance metadata): the keys come from `AWS_ACCESS_KEY_ID`,
//! `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`, the region from `AWS_REGION`.
//!
//! Included by `stack_isolation.rs` with `#[path = "support/deployed.rs"] mod deployed;`.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aws_sdk_ecs::types::{
    AssignPublicIp, AwsVpcConfiguration, Compatibility, ContainerDefinition, ContainerOverride,
    DesiredStatus, KernelCapabilities, LaunchType, LinuxParameters, NetworkConfiguration,
    NetworkMode, Task, TaskOverride,
};
use base64::Engine;
use serde::Deserialize;

/// The deployed stack's public address, `https://<name>`.
pub const URL_VAR: &str = "KNOGGIN_STACK_URL";
/// The JSON file of [`Credentials`].
pub const CREDENTIALS_VAR: &str = "KNOGGIN_STACK_CREDENTIALS";

/// Which stack the isolation tests run against.
#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    /// The one-host Compose stack, started by the test.
    Local,
    /// A deployed AWS stack.
    Deployed { url: String, credentials: PathBuf },
}

/// The target for the two variables' values. An empty value counts as unset. Only one
/// of the two set is a mistake, not a reason to fall back to the local stack.
pub fn target(url: Option<&str>, credentials: Option<&str>) -> Result<Target, String> {
    fn set(v: Option<&str>) -> Option<&str> {
        v.map(str::trim).filter(|v| !v.is_empty())
    }
    match (set(url), set(credentials)) {
        (None, None) => Ok(Target::Local),
        (Some(url), Some(credentials)) => {
            if !url.starts_with("https://") {
                return Err(format!(
                    "{URL_VAR} must be the stack's https address, not `{url}`"
                ));
            }
            Ok(Target::Deployed {
                url: url.trim_end_matches('/').to_owned(),
                credentials: PathBuf::from(credentials),
            })
        }
        (Some(_), None) => Err(format!(
            "{URL_VAR} is set but {CREDENTIALS_VAR} is not: a deployed stack needs test credentials"
        )),
        (None, Some(_)) => Err(format!(
            "{CREDENTIALS_VAR} is set but {URL_VAR} is not: say which stack to test"
        )),
    }
}

/// The target from the environment; panics on a half-set pair.
pub fn target_from_env() -> Target {
    let var = |name| std::env::var(name).ok();
    target(var(URL_VAR).as_deref(), var(CREDENTIALS_VAR).as_deref())
        .unwrap_or_else(|e| panic!("{e}"))
}

/// The test credentials file. A and B are two test identities that the provider lets
/// in; the run removes B at the end. `admin` is an identity of
/// `admin_subjects` or `admin_emails`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    /// The module's `name` (default `knoggin`): the cluster and the task families.
    pub name: String,
    pub a: Person,
    pub b: Person,
    pub admin: Admin,
    pub outside: Outside,
}

/// The value of a browser session's `__Host-session` cookie, and an OAuth access token
/// that the provider issued to an MCP client for this server, both of one identity.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Person {
    pub session: String,
    pub token: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admin {
    pub session: String,
}

/// A bucket in another account, in the deployment's region, and keys of that account
/// that may upload to it. The probe tries an upload there that the user network must
/// stop.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outside {
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    #[serde(default)]
    pub session_token: Option<String>,
}

impl Credentials {
    /// Reads the file, which must be readable by its owner alone.
    pub fn read(path: &Path) -> Self {
        let mode = std::fs::metadata(path)
            .unwrap_or_else(|e| panic!("{CREDENTIALS_VAR} {}: {e}", path.display()))
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "{} holds credentials: make it mode 0600",
            path.display()
        );
        let text = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{CREDENTIALS_VAR} {}: {e}", path.display()))
    }
}

/// An S3 POST upload of one small object, signed with Signature Version 4 by the
/// outside account's keys (the browser-upload form, because the probe's `wget` can send
/// a POST but not a PUT).
#[derive(Debug)]
pub struct Upload {
    pub url: String,
    pub content_type: String,
    pub body: Vec<u8>,
}

const BOUNDARY: &str = "knogginprobeboundary";

impl Upload {
    pub fn new(outside: &Outside, region: &str, key: &str, now: time::OffsetDateTime) -> Upload {
        let date = now
            .format(time::macros::format_description!(
                "[year][month][day]T[hour][minute][second]Z"
            ))
            .unwrap();
        let day = &date[..8];
        let expires = (now + Duration::from_secs(15 * 60))
            .format(time::macros::format_description!(
                "[year]-[month]-[day]T[hour]:[minute]:[second].000Z"
            ))
            .unwrap();
        let credential = format!("{}/{day}/{region}/s3/aws4_request", outside.access_key_id);
        let mut fields: Vec<(&str, String)> = vec![
            ("key", key.to_owned()),
            ("x-amz-algorithm", "AWS4-HMAC-SHA256".to_owned()),
            ("x-amz-credential", credential),
            ("x-amz-date", date.clone()),
        ];
        if let Some(token) = &outside.session_token {
            fields.push(("x-amz-security-token", token.clone()));
        }
        let mut conditions = vec![serde_json::json!({ "bucket": outside.bucket })];
        conditions.extend(fields.iter().map(|(k, v)| serde_json::json!({ (*k): v })));
        let policy = serde_json::json!({ "expiration": expires, "conditions": conditions });
        let policy = base64::engine::general_purpose::STANDARD.encode(policy.to_string());
        let key = signing_key(&outside.secret_access_key, day, region, "s3");
        let signature = hex(&hmac(&key, policy.as_bytes()));
        fields.push(("policy", policy));
        fields.push(("x-amz-signature", signature));
        let mut body = String::new();
        for (name, value) in &fields {
            body.push_str(&format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            ));
        }
        body.push_str(&format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"probe\"\r\nContent-Type: text/plain\r\n\r\nknoggin isolation probe\r\n--{BOUNDARY}--\r\n"
        ));
        Upload {
            url: format!("https://{}.s3.{region}.amazonaws.com/", outside.bucket),
            content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
            body: body.into_bytes(),
        }
    }
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let key = aws_lc_rs::hmac::Key::new(aws_lc_rs::hmac::HMAC_SHA256, key);
    aws_lc_rs::hmac::sign(&key, data).as_ref().to_vec()
}

/// The Signature Version 4 signing key of a day, region and service.
pub fn signing_key(secret: &str, day: &str, region: &str, service: &str) -> Vec<u8> {
    let k = hmac(format!("AWS4{secret}").as_bytes(), day.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, service.as_bytes());
    hmac(&k, b"aws4_request")
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The private address of an `awsvpc` task.
fn address(task: &Task) -> Option<String> {
    task.attachments()
        .iter()
        .flat_map(|a| a.details())
        .find(|d| d.name() == Some("privateIPv4Address"))
        .and_then(|d| d.value())
        .map(str::to_owned)
}

/// The running tasks of one family, described.
async fn running(ecs: &aws_sdk_ecs::Client, cluster: &str, family: &str) -> Vec<Task> {
    let arns = ecs
        .list_tasks()
        .cluster(cluster)
        .family(family)
        .desired_status(DesiredStatus::Running)
        .send()
        .await
        .unwrap()
        .task_arns()
        .to_vec();
    if arns.is_empty() {
        return Vec::new();
    }
    ecs.describe_tasks()
        .cluster(cluster)
        .set_tasks(Some(arns))
        .send()
        .await
        .unwrap()
        .tasks()
        .iter()
        .filter(|t| t.last_status() == Some("RUNNING"))
        .cloned()
        .collect()
}

/// The shell script of the probe: each check is a condition that holds when the rule
/// does; the exit code is the number of the first one that does not.
pub fn script(checks: &[(String, String)], upload: &Upload) -> String {
    let form = base64::engine::general_purpose::STANDARD.encode(&upload.body);
    let mut script = format!("echo {form} | base64 -d > /tmp/form || exit 100\n");
    for (n, (_, condition)) in checks.iter().enumerate() {
        script.push_str(&format!(
            "if ! ( {condition} ); then fail=${{fail:-{}}}; fi\n",
            n + 1
        ));
    }
    script.push_str("exit ${fail:-0}\n");
    script
}

/// The network probes on AWS, from a one-off probe task in the user network. The keep-alive of the
/// test keeps the user containers of A and B running meanwhile.
pub async fn network_probe(c: &Credentials) {
    let config = knoggin_server::aws::sdk_config(None, None).await;
    let region = config
        .region()
        .expect("AWS_REGION names the deployment's region")
        .to_string();
    let ecs = aws_sdk_ecs::Client::new(&config);

    // The user network, the cluster and the image repository, as the router sees them.
    let router_def = ecs
        .describe_task_definition()
        .task_definition(format!("{}-router", c.name))
        .send()
        .await
        .unwrap();
    let env: BTreeMap<String, String> = router_def
        .task_definition()
        .unwrap()
        .container_definitions()
        .iter()
        .flat_map(|d| d.environment())
        .filter_map(|kv| Some((kv.name()?.to_owned(), kv.value()?.to_owned())))
        .collect();
    let setting = |name: &str| {
        env.get(name)
            .unwrap_or_else(|| panic!("the router's task definition has no {name}"))
            .clone()
    };
    let cluster = setting("KNOGGIN_LAUNCHER__ECS__CLUSTER");
    let subnets: Vec<String> = setting("KNOGGIN_LAUNCHER__ECS__SUBNETS")
        .split(',')
        .map(str::to_owned)
        .collect();
    let groups: Vec<String> = setting("KNOGGIN_LAUNCHER__ECS__SECURITY_GROUPS")
        .split(',')
        .map(str::to_owned)
        .collect();
    let user_def = ecs
        .describe_task_definition()
        .task_definition(setting("KNOGGIN_LAUNCHER__ECS__TASK_DEFINITION"))
        .send()
        .await
        .unwrap();
    let user_def = user_def.task_definition().unwrap();
    let fargate = user_def
        .requires_compatibilities()
        .contains(&Compatibility::Fargate);
    let user_image = user_def.container_definitions()[0]
        .image()
        .unwrap()
        .to_owned();
    let repository = user_image.split('@').next().unwrap();
    let busybox = super::stack::pinned_image("busybox");
    let digest = busybox.split('@').nth(1).unwrap();
    let probe_image = format!("{repository}@{digest}");

    let router = running(&ecs, &cluster, &format!("{}-router", c.name)).await;
    let router = router
        .iter()
        .find_map(address)
        .expect("a running router task");
    let user = running(&ecs, &cluster, &format!("{}-user", c.name)).await;
    let user = user
        .iter()
        .find_map(address)
        .expect("a running user container");

    // The same upload from outside the VPC must work, or a refusal inside proves nothing.
    let now = time::OffsetDateTime::now_utc();
    let key = format!("knoggin-probe/{}", uuid::Uuid::now_v7());
    let control = Upload::new(&c.outside, &region, &format!("{key}-outside"), now);
    let r = reqwest::Client::new()
        .post(&control.url)
        .header("content-type", &control.content_type)
        .body(control.body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    assert_eq!(
        status,
        204,
        "an upload to the outside bucket from outside the VPC failed; check `outside` in {CREDENTIALS_VAR}: {}",
        r.text().await.unwrap_or_default()
    );
    let upload = Upload::new(&c.outside, &region, &format!("{key}-inside"), now);

    let bytes = |port: &str| {
        format!(
            "[ \"$(printf 'GET /healthz HTTP/1.0\\r\\n\\r\\n' | nc -w 3 {router} {port} 2>/dev/null | wc -c)\" -eq 0 ]"
        )
    };
    let checks: Vec<(String, String)> = vec![
        (
            "another user container".into(),
            format!("! nc -z -w 2 {user} 8443"),
        ),
        ("the router's public port".into(), bytes("8080")),
        ("the router's metrics port".into(), bytes("9091")),
        (
            "the router's internal port answers".into(),
            format!("nc -z -w 2 {router} 8443"),
        ),
        (
            "the metadata address".into(),
            "! nc -z -w 2 169.254.169.254 80".into(),
        ),
        (
            "an outside address".into(),
            "! nc -z -w 2 1.1.1.1 443".into(),
        ),
        (
            "a lookup of an outside name".into(),
            "! timeout 5 nslookup example.com >/dev/null 2>&1".into(),
        ),
        // S3 answers, and refuses: the endpoint policy lets through only the deployment's
        // bucket. No answer at all would mean the check did not reach S3.
        (
            "an upload to a bucket in another account".into(),
            format!(
                "timeout 30 wget -S -O /dev/null --header 'Content-Type: {}' --post-file /tmp/form {} 2>&1 | grep -q 'HTTP/1.[01] 403'",
                upload.content_type, upload.url
            ),
        ),
    ];
    let script = script(&checks, &upload);

    let family = format!("{}-isolation-probe", c.name);
    let mut register = ecs
        .register_task_definition()
        .family(&family)
        .network_mode(NetworkMode::Awsvpc)
        .set_requires_compatibilities(Some(user_def.requires_compatibilities().to_vec()))
        .set_runtime_platform(user_def.runtime_platform().cloned());
    if fargate {
        register = register
            .cpu("256")
            .memory("512")
            .set_execution_role_arn(user_def.execution_role_arn().map(str::to_owned));
    }
    let registered = register
        .container_definitions(
            ContainerDefinition::builder()
                .name("probe")
                .image(&probe_image)
                .essential(true)
                .memory(64)
                .user("65532:65532")
                .linux_parameters(
                    LinuxParameters::builder()
                        .capabilities(KernelCapabilities::builder().drop("ALL").build())
                        .build(),
                )
                .build(),
        )
        .send()
        .await
        .unwrap();
    let definition = registered
        .task_definition()
        .unwrap()
        .task_definition_arn()
        .unwrap()
        .to_owned();
    // The script carries the signed form, so it goes in an override, not the definition.
    let run = ecs
        .run_task()
        .cluster(&cluster)
        .task_definition(&definition)
        .launch_type(if fargate {
            LaunchType::Fargate
        } else {
            LaunchType::Ec2
        })
        .count(1)
        .started_by("knoggin-isolation-test")
        .network_configuration(
            NetworkConfiguration::builder()
                .awsvpc_configuration(
                    AwsVpcConfiguration::builder()
                        .set_subnets(Some(subnets))
                        .set_security_groups(Some(groups))
                        .assign_public_ip(AssignPublicIp::Disabled)
                        .build()
                        .unwrap(),
                )
                .build(),
        )
        .overrides(
            TaskOverride::builder()
                .container_overrides(
                    ContainerOverride::builder()
                        .name("probe")
                        .command("sh")
                        .command("-c")
                        .command(&script)
                        .build(),
                )
                .build(),
        )
        .send()
        .await;
    let outcome = match run {
        Ok(run) if run.failures().is_empty() => {
            let arn = run.tasks()[0].task_arn().unwrap().to_owned();
            wait_stopped(&ecs, &cluster, &arn).await
        }
        Ok(run) => Err(format!(
            "the probe task did not start: {:?}",
            run.failures()
        )),
        Err(e) => Err(format!("the probe task did not start: {e:?}")),
    };
    let _ = ecs
        .deregister_task_definition()
        .task_definition(&definition)
        .send()
        .await;
    let code = outcome.unwrap_or_else(|e| panic!("{e}"));
    assert!(code != 100, "the probe could not write its upload form");
    let failed = usize::try_from(code)
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|i| checks.get(i));
    assert_eq!(
        code,
        0,
        "the probe in the user network: {} does not hold (router {router}, user container {user})",
        failed.map_or("an unknown check", |(label, _)| label.as_str())
    );
    println!(
        "probe in the user network: all {} checks hold",
        checks.len()
    );
}

/// Waits for the probe task to stop: its container's exit code.
async fn wait_stopped(ecs: &aws_sdk_ecs::Client, cluster: &str, arn: &str) -> Result<i32, String> {
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let tasks = ecs
            .describe_tasks()
            .cluster(cluster)
            .tasks(arn)
            .send()
            .await
            .map_err(|e| format!("{e:?}"))?;
        if let Some(task) = tasks.tasks().first()
            && task.last_status() == Some("STOPPED")
        {
            let container = task.containers().first();
            return container.and_then(|c| c.exit_code()).ok_or_else(|| {
                format!(
                    "the probe task stopped without an exit code: {} {}",
                    task.stopped_reason().unwrap_or_default(),
                    container.and_then(|c| c.reason()).unwrap_or_default()
                )
            });
        }
        if Instant::now() > deadline {
            return Err(format!(
                "the probe task {arn} did not stop within 10 minutes"
            ));
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
