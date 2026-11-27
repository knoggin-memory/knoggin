// SPDX-License-Identifier: AGPL-3.0-only
//! The isolation rules on the real one-host stack (tab "Security", the stack column of
//! I1 to I3, I8 to I10, I12 to I14, I16 and I17), the Compose file checks and the
//! Caddyfile log test (tab "Deployment", Checking the Compose stack). Two users, A and B,
//! each with a personal store and apps, behind the router of one stack.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

#[path = "support/stack.rs"]
mod stack;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use knoggin_server::router::forward::STORE_ROUTES;
use reqwest::Method;
use serde_json::{Value, json};
use stack::{Answer, Cred, ROOT, Stack, docker, docker_ok, eventually, pinned_image, text};

fn suffix() -> String {
    format!(
        "{:x}{}",
        std::process::id(),
        &uuid::Uuid::now_v7().simple().to_string()[24..]
    )
}

/// A container's addresses on its networks, by network name.
fn addresses(id: &str) -> BTreeMap<String, String> {
    let v: Value = serde_json::from_str(&docker_ok(&["inspect", id])).unwrap();
    v[0]["NetworkSettings"]["Networks"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, n)| (k.clone(), n["IPAddress"].as_str().unwrap().to_owned()))
        .collect()
}

fn not_success(a: &Answer, what: &str) {
    assert!(
        !(200..300).contains(&a.status),
        "{what}: {} {}",
        a.status,
        a.body
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs docker"]
async fn isolation_on_the_one_host_stack() {
    let s = Stack::up().await;
    let sfx = suffix();
    compose_file_checks(&s);

    let a = s.sign_in(&format!("alice{sfx}")).await;
    let b = s.sign_in(&format!("bob{sfx}")).await;
    let admin = s.sign_in("admin").await;
    let (ax_name, ay_name, bx_name) = (
        format!("appx{sfx}"),
        format!("appy{sfx}"),
        format!("bapp{sfx}"),
    );
    let ax = s.app(&a, &ax_name).await;
    let ay = s.app(&a, &ay_name).await;
    let _bx = s.app(&b, &bx_name).await;
    let (key_names, keys) = {
        let names = [
            format!("keyx{sfx}"),
            format!("keyy{sfx}"),
            format!("keyb{sfx}"),
        ];
        let kx = s.key(&a, &ax, &names[0]).await;
        let ky = s.key(&a, &ay, &names[1]).await;
        let kb = s.key(&b, &b.personal, &names[2]).await;
        (names, [kx, ky, kb])
    };
    let [(ax_key_id, ax_key), (_ay_key_id, ay_key), (b_key_id, b_key)] = keys.clone();
    let texts = [
        format!("a-personal-{sfx}"),
        format!("a-appx-{sfx}"),
        format!("b-personal-{sfx}"),
    ];
    assert_eq!(s.save(&a.token, None, &texts[0]).await.status, 201);
    assert_eq!(s.save(&ax_key, None, &texts[1]).await.status, 201);
    assert_eq!(s.save(&b.token, None, &texts[2]).await.status, 201);
    let alive = s.keep_alive(vec![a.token.clone(), b.token.clone(), ax_key.clone()]);

    // I1, I2, I3, I13: every store route, with a credential of A (or the admin) and a
    // store that is not theirs to name, is refused and shows nothing of B's.
    let foreign = [
        ("A's session, B's store", &a.session, b.personal.as_str()),
        ("A's token, B's store", &a.token, b.personal.as_str()),
        ("A's app key, B's store", &ax_key, b.personal.as_str()),
        (
            "A's app key, A's personal store",
            &ax_key,
            a.personal.as_str(),
        ),
        ("A's app key, A's other app", &ax_key, ay.as_str()),
        (
            "the admin's session, B's store",
            &admin.session,
            b.personal.as_str(),
        ),
    ];
    let id = uuid::Uuid::now_v7().to_string();
    for pattern in STORE_ROUTES {
        let path = pattern.replace("{id}", &id).replace("{name}", "default");
        for method in [Method::GET, Method::POST, Method::DELETE] {
            for (what, cred, store) in &foreign {
                let body = (method == Method::POST).then(
                    || json!({ "summary": "x", "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
                );
                let r = s
                    .call(
                        method.clone(),
                        &path,
                        cred,
                        &[("knoggin-store", store)],
                        body,
                    )
                    .await;
                not_success(&r, &format!("{what}: {method} {path}"));
                assert!(
                    !r.body.contains(&texts[2]),
                    "{what}: {method} {path} shows B's memory"
                );
                // I2: a session naming another user's store gets not_found (a body
                // check of the route may refuse first; that is a refusal too).
                if matches!(cred, Cred::Session(_)) && method == Method::GET {
                    assert_eq!(r.code(), "not_found", "{what}: {method} {path}: {}", r.body);
                }
            }
        }
    }
    // Each credential still sees only its own store.
    assert_eq!(s.summaries(&a.token, None).await.1, vec![texts[0].clone()]);
    assert_eq!(s.summaries(&ax_key, None).await.1, vec![texts[1].clone()]);
    assert_eq!(s.summaries(&ay_key, None).await.1, Vec::<String>::new());
    assert_eq!(s.summaries(&b_key, None).await.1, vec![texts[2].clone()]);

    // Control routes that take an ID, with B's IDs, and mixed IDs: not_found, for A and
    // for the admin alike.
    let new_key = json!({ "name": "x" });
    for (who, cred) in [("A", &a.session), ("the admin", &admin.session)] {
        for (method, path, body) in [
            (Method::GET, format!("/v1/stores/{}", b.personal), None),
            (
                Method::PATCH,
                format!("/v1/stores/{}", b.personal),
                Some(json!({ "name": "y" })),
            ),
            (Method::DELETE, format!("/v1/stores/{}", b.personal), None),
            (
                Method::POST,
                format!("/v1/stores/{}/undelete", b.personal),
                None,
            ),
            (
                Method::GET,
                format!("/v1/stores/{}/status", b.personal),
                None,
            ),
            (Method::GET, format!("/v1/stores/{}/keys", b.personal), None),
            (
                Method::POST,
                format!("/v1/stores/{}/keys", b.personal),
                Some(new_key.clone()),
            ),
            (
                Method::POST,
                format!("/v1/stores/{}/keys/{b_key_id}/rotate", b.personal),
                None,
            ),
            (
                Method::DELETE,
                format!("/v1/stores/{}/keys/{b_key_id}", b.personal),
                None,
            ),
            (
                Method::POST,
                format!("/v1/stores/{}/keys/{b_key_id}/rotate", a.personal),
                None,
            ),
            (
                Method::DELETE,
                format!("/v1/stores/{}/keys/{b_key_id}", a.personal),
                None,
            ),
            (
                Method::DELETE,
                format!("/v1/stores/{ax}/keys/{b_key_id}"),
                None,
            ),
        ] {
            let r = s.call(method.clone(), &path, cred, &[], body).await;
            assert_eq!(
                r.code(),
                "not_found",
                "{who}: {method} {path}: {} {}",
                r.status,
                r.body
            );
        }
    }
    assert_eq!(
        s.summaries(&b_key, None).await.1,
        vec![texts[2].clone()],
        "B's key still works"
    );

    // Internal paths sent to the public listener are never forwarded.
    for (method, path) in [
        (Method::GET, "/internal/v1/status"),
        (Method::GET, "/internal/v1/hello?nonce=00"),
        (Method::POST, "/internal/v1/park"),
        (Method::POST, "/v1/%2e%2e/internal/v1/park"),
        (Method::GET, "/v1/../internal/v1/status"),
        (
            Method::GET,
            "/internal/v1/blob/stores/x/snap/0000000001-0000000001.kgs",
        ),
    ] {
        let r = s.call(method.clone(), path, &a.token, &[], None).await;
        // A GET that is no API path gets the web UI's page; nothing reaches a container.
        let page = r.status == 200
            && r.headers
                .get("content-type")
                .is_some_and(|v| v.to_str().unwrap().starts_with("text/html"));
        if !page {
            not_success(&r, &format!("{method} {path}"));
        }
        assert_ne!(
            r.status, 503,
            "{method} {path} reached the store layer: {}",
            r.body
        );
        assert!(
            !r.body.contains("\"state\""),
            "{method} {path} answered a status: {}",
            r.body
        );
    }

    // I16: an access token reaches memories and the session route, nothing else.
    for (method, path) in [
        (Method::GET, "/v1/stores".to_owned()),
        (Method::POST, "/v1/stores".to_owned()),
        (Method::GET, format!("/v1/stores/{}/keys", a.personal)),
        (Method::GET, "/v1/users".to_owned()),
        (Method::GET, "/v1/admin/status".to_owned()),
    ] {
        let r = s
            .call(
                method.clone(),
                &path,
                &a.token,
                &[],
                Some(json!({ "name": "z" })),
            )
            .await;
        assert_eq!(r.code(), "permission_denied", "token: {method} {path}");
    }
    assert_eq!(
        s.call(Method::GET, "/v1/session", &a.token, &[], None)
            .await
            .status,
        200
    );

    // I14: the client of app X's last write, which X's record holds once the park saves.
    let client = format!("client{sfx}");
    let mcp_text = format!("a-appx-mcp-{sfx}");
    let r = s
        .call(
            Method::POST,
            "/mcp",
            &ax_key,
            &[
                ("mcp-method", "tools/call"),
                ("mcp-protocol-version", "2026-07-28"),
                ("mcp-name", "store_memory"),
            ],
            Some(
                json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                "name": "store_memory", "arguments": { "summary": mcp_text }, "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientInfo": { "name": client, "version": "1" },
                    "io.modelcontextprotocol/clientCapabilities": {} } } }),
            ),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.json()["result"]["isError"].is_null(), "{}", r.body);
    // I9: from inside A's container, only the router's internal port answers.
    let ca = s.container_of(&a).expect("A's container");
    let cb = s.container_of(&b).expect("B's container");
    assert_ne!(ca.docker_id, cb.docker_id, "one container per user");
    network_probes(&s, &ca, &cb);
    container_description(&s, &ca);
    service_probes(&s);
    second_stack_leaves_this_one_alone(&s, &[&ca, &cb]).await;
    let mut user_logs = String::new();
    for c in s.user_containers() {
        user_logs.push_str(&text(&docker(&["logs", &c.docker_id])));
    }
    drop(alive);

    // I10: once idle, A's container parks; it no longer runs and its network is gone.
    assert!(
        eventually(Duration::from_secs(90), || async {
            !s.user_containers()
                .iter()
                .any(|c| c.docker_id == ca.docker_id)
                && !s.user_networks().contains(&ca.network)
        })
        .await,
        "A's container or network is still there after the park"
    );

    // I12: a revoked key is refused on its next request.
    let r = s
        .call(
            Method::DELETE,
            &format!("/v1/stores/{ax}/keys/{ax_key_id}"),
            &a.session,
            &[],
            None,
        )
        .await;
    assert_eq!(r.status, 204, "{}", r.body);
    not_success(
        &s.call(Method::GET, "/v1/memories", &ax_key, &[], None)
            .await,
        "revoked key",
    );

    // I14: nothing at rest is plaintext, and no log holds a marker or a secret.
    let mut markers: Vec<String> = texts.to_vec();
    markers.extend([
        a.sub.clone(),
        b.sub.clone(),
        ax_name,
        ay_name,
        bx_name,
        client,
        mcp_text,
    ]);
    markers.extend(key_names);
    let mut secrets = s.secrets.clone();
    secrets.extend(keys.iter().map(|(_, k)| match k {
        Cred::Bearer(t) => t.clone(),
        _ => unreachable!(),
    }));
    at_rest_checks(&s, &markers, &secrets);
    let mut logs = user_logs;
    for service in ["router", "launcher", "lifecycle"] {
        logs.push_str(&s.logs(service));
    }
    for m in markers.iter().chain(&secrets) {
        assert!(!logs.contains(m.as_str()), "a log holds `{m}`");
    }
    assert!(
        !logs.contains("X-Amz-Signature"),
        "a log holds a signed link"
    );

    // I17: once an admin removes B, B's key, session and token are refused at once.
    let r = s
        .call(
            Method::DELETE,
            &format!("/v1/users/{}", b.id),
            &admin.session,
            &[],
            None,
        )
        .await;
    assert!((200..300).contains(&r.status), "{} {}", r.status, r.body);
    for (what, cred) in [
        ("key", &b_key),
        ("session", &b.session),
        ("token", &b.token),
    ] {
        not_success(
            &s.call(Method::GET, "/v1/memories", cred, &[], None).await,
            what,
        );
    }
}

/// Reads the merged Compose file and checks what tab "Deployment" fixes: capabilities,
/// networks and each service's secret files; and the Caddyfile's two log settings.
fn compose_file_checks(s: &Stack) {
    let v: Value =
        serde_json::from_str(&s.compose_ok(&["--profile", "tls", "config", "--format", "json"]))
            .unwrap();
    let services = v["services"].as_object().unwrap();
    let names = |v: &Value| -> BTreeSet<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .map(|x| {
                        x.as_str()
                            .map_or_else(|| x["source"].as_str().unwrap().to_owned(), str::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let expected: BTreeMap<&str, &[&str]> = BTreeMap::from([
        (
            "router",
            &[
                "postgres_url",
                "master_key",
                "state_key",
                "s3_credentials",
                "internal_secret",
                "internal_cert.pem",
                "internal_key.pem",
                "launcher_token",
                "web_client_secret",
            ][..],
        ),
        ("launcher", &["launcher_token"][..]),
        (
            "lifecycle",
            &[
                "postgres_url_lifecycle",
                "s3_credentials_lifecycle",
                "state_key",
            ][..],
        ),
        (
            "db",
            &[
                "postgres_superuser_password",
                "postgres_url",
                "postgres_url_lifecycle",
                "postgres_url_backup",
            ][..],
        ),
        ("backup", &["postgres_url_backup"][..]),
        (
            "s3-init",
            &["s3_credentials", "s3_credentials_lifecycle"][..],
        ),
        ("proxy", &[][..]),
        ("s3", &[][..]),
        ("test-issuer", &[][..]),
    ]);
    assert_eq!(
        services.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        expected.keys().copied().collect(),
        "the services"
    );
    for (name, svc) in services {
        assert!(
            names(&svc["cap_drop"]).contains("ALL"),
            "{name} drops every capability"
        );
        let added = names(&svc["cap_add"]);
        assert!(
            !added.contains("NET_RAW") && !added.contains("CAP_NET_RAW"),
            "{name} has NET_RAW"
        );
        assert_eq!(
            names(&svc["secrets"]),
            expected[name.as_str()]
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            "{name}'s secret files"
        );
    }
    let networks = |n: &str| -> BTreeSet<String> {
        services[n]["networks"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    };
    assert!(
        networks("db").is_disjoint(&networks("s3")),
        "db and s3 share a network"
    );

    let caddy = std::fs::read_to_string(format!("{ROOT}/deploy/docker/Caddyfile")).unwrap();
    assert!(caddy.contains("exclude http.log.error"));
    let site = caddy
        .split("{$KNOGGIN_DOMAIN} {")
        .nth(1)
        .expect("the site block");
    assert!(
        !site.lines().any(|l| l.trim_start().starts_with("log")),
        "the site block has a log directive"
    );
}

/// Rule I9 from inside A's network namespace.
fn network_probes(s: &Stack, a: &stack::UserContainer, b: &stack::UserContainer) {
    let router = s.service_id("router");
    let router_here = addresses(&router)[&a.network].clone();
    let ip = |service: &str| {
        addresses(&s.service_id(service))
            .into_values()
            .next()
            .unwrap()
    };
    let gateway = {
        let net: Value =
            serde_json::from_str(&docker_ok(&["network", "inspect", &a.network])).unwrap();
        assert_eq!(net[0]["Internal"], true, "the user network is internal");
        let subnet = net[0]["IPAM"]["Config"][0]["Subnet"]
            .as_str()
            .unwrap()
            .to_owned();
        let base = subnet.split('/').next().unwrap();
        let (head, last) = base.rsplit_once('.').unwrap();
        format!("{head}.{}", last.parse::<u8>().unwrap() + 1)
    };
    let edge_host = format!("{}.0.1", s.prefix);
    let port = s.port.to_string();
    let closed: Vec<(String, String, String)> = vec![
        (
            "the other user container".into(),
            b.ip.clone(),
            "8443".into(),
        ),
        ("db".into(), ip("db"), "5432".into()),
        ("launcher".into(), ip("launcher"), "7070".into()),
        ("s3".into(), ip("s3"), "8333".into()),
        ("metadata".into(), "169.254.169.254".into(), "80".into()),
        ("outside".into(), "1.1.1.1".into(), "443".into()),
        ("gateway".into(), gateway, "80".into()),
        ("host published port".into(), edge_host.clone(), port),
        ("host 22".into(), edge_host.clone(), "22".into()),
        ("host 2375".into(), edge_host.clone(), "2375".into()),
        ("host 2376".into(), edge_host, "2376".into()),
    ];
    let mut script = String::new();
    for (label, host, port) in &closed {
        script.push_str(&format!(
            "if nc -z -w 2 {host} {port} 2>/dev/null; then echo '{label}: open'; else echo '{label}: closed'; fi\n"
        ));
    }
    script.push_str(&format!(
        "if nc -z -w 2 {router_here} 8443; then echo 'internal: open'; else echo 'internal: closed'; fi\n"
    ));
    for p in ["8080", "9091"] {
        script.push_str(&format!(
            "n=$(printf 'GET /healthz HTTP/1.0\\r\\n\\r\\n' | nc -w 3 {router_here} {p} 2>/dev/null | wc -c); echo \"router {p}: $n bytes\"\n"
        ));
    }
    script.push_str("if timeout 5 nslookup example.com >/dev/null 2>&1; then echo 'lookup: ok'; else echo 'lookup: failed'; fi\n");
    let out = s.probe(&format!("container:{}", a.docker_id), &script);
    println!("probe from A's container:\n{out}");
    for (label, _, _) in &closed {
        assert!(
            out.contains(&format!("{label}: closed")),
            "{label} is reachable:\n{out}"
        );
    }
    assert!(
        out.contains("internal: open"),
        "the router's internal port:\n{out}"
    );
    assert!(
        out.contains("router 8080: 0 bytes"),
        "the public port answered:\n{out}"
    );
    assert!(
        out.contains("router 9091: 0 bytes"),
        "the metrics port answered:\n{out}"
    );
    assert!(
        out.contains("lookup: failed"),
        "an outside name resolved:\n{out}"
    );

    // From the host, the networks that give it no address cannot be reached (Linux only:
    // elsewhere Docker runs in a VM whose own routing decides).
    if cfg!(target_os = "linux") {
        for (service, port) in [("db", 5432), ("s3", 8333), ("launcher", 7070)] {
            let addr: std::net::SocketAddr = format!("{}:{port}", ip(service)).parse().unwrap();
            assert!(
                std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_err(),
                "the host reaches {service}"
            );
        }
    } else {
        println!("skipped: the host-side checks of I9 run on Linux only");
    }
}

/// Rule I9: what the launcher's fixed description sets for safety (steps 2, 3 and 5),
/// and no Compose service with NET_RAW.
fn container_description(s: &Stack, c: &stack::UserContainer) {
    let v: Value = serde_json::from_str(&docker_ok(&["inspect", &c.docker_id])).unwrap();
    let v = &v[0];
    let h = &v["HostConfig"];
    assert_eq!(h["ReadonlyRootfs"], true);
    assert!(
        v["Mounts"].as_array().is_none_or(Vec::is_empty),
        "no mounts"
    );
    assert_eq!(v["Config"]["User"], "65532:65532");
    assert!(h["CapDrop"].as_array().unwrap().iter().any(|c| c == "ALL"));
    assert!(
        h["CapAdd"].as_array().is_none_or(Vec::is_empty),
        "no added capability"
    );
    assert!(
        h["SecurityOpt"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o.as_str().unwrap().starts_with("no-new-privileges")),
        "no-new-privileges"
    );
    assert_eq!(h["Privileged"], false);
    assert!(h["Memory"].as_i64().unwrap() > 0);
    assert_eq!(
        h["MemorySwap"], h["Memory"],
        "swap equal to the memory limit"
    );
    assert_eq!(h["PidsLimit"], 256);
    assert!(
        h["Ulimits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["Name"] == "core" && u["Soft"] == 0 && u["Hard"] == 0),
        "core limit 0"
    );
    assert!(
        h["PortBindings"]
            .as_object()
            .is_none_or(serde_json::Map::is_empty),
        "no published port"
    );
    assert_eq!(h["RestartPolicy"]["Name"], "no");
    assert_eq!(
        v["NetworkSettings"]["Networks"].as_object().unwrap().len(),
        1,
        "one network"
    );
    for id in s.compose_ok(&["ps", "-aq"]).split_whitespace() {
        let v: Value = serde_json::from_str(&docker_ok(&["inspect", id])).unwrap();
        let added = v[0]["HostConfig"]["CapAdd"].to_string();
        assert!(!added.contains("NET_RAW"), "{id} has NET_RAW: {added}");
    }
}

/// Rule I9: from the network namespace of db, s3 and launcher, and from a place on the
/// edge network in place of the proxy, the router's internal port cannot be opened (it
/// closes the connection without a byte).
fn service_probes(s: &Stack) {
    let script = "n=$(printf 'x\\r\\n' | nc -w 3 router 8443 2>/dev/null | wc -c); echo \"internal: $n bytes\"";
    for service in ["db", "s3", "launcher"] {
        let out = s.probe(&format!("container:{}", s.service_id(service)), script);
        assert!(
            out.contains("internal: 0 bytes"),
            "{service} reached the internal port: {out}"
        );
    }
    let out = s.probe(&s.network("edge"), script);
    assert!(
        out.contains("internal: 0 bytes"),
        "the edge network reached the internal port: {out}"
    );
}

/// Rule I9: a second stack on the same Docker host, under another project name and
/// prefix, leaves this stack's containers and networks alone.
async fn second_stack_leaves_this_one_alone(s: &Stack, mine: &[&stack::UserContainer]) {
    let other = Stack::create(&[&s.prefix]);
    other.compose_ok(&["up", "-d", "launcher"]);
    // Longer than the launcher's tidy-up interval and its one-minute rule for networks.
    tokio::time::sleep(Duration::from_secs(15)).await;
    let token = std::fs::read_to_string(other.secrets_dir().join("launcher_token")).unwrap();
    let token = token.trim();
    let auth = format!("Authorization: Bearer {token}");
    let list = other.probe(
        &other.network("ctl"),
        &format!("wget -q -O- --header '{auth}' http://launcher:7070/v1/containers"),
    );
    let listed: Value = serde_json::from_str(list.trim()).unwrap_or(Value::Null);
    assert_eq!(
        listed["items"],
        json!([]),
        "the other launcher lists only its own: {list}"
    );
    let stop = other.probe(
        &other.network("ctl"),
        &format!(
            "wget -q -O- --header '{auth}' --post-data '' http://launcher:7070/v1/containers/{}/stop 2>&1; echo \"exit $?\"",
            mine[0].docker_id
        ),
    );
    assert!(
        !stop.contains("exit 0"),
        "the other launcher stopped this stack's container: {stop}"
    );
    let now = s.user_containers();
    for c in mine {
        assert!(
            now.iter().any(|x| x.docker_id == c.docker_id && x.running),
            "a container of this stack was touched"
        );
        assert!(
            s.user_networks().contains(&c.network),
            "a network of this stack was touched"
        );
    }
    drop(other);
}

/// Rules I8 and I14: the bucket and a dump of the state database hold no marker and no
/// key secret, and no `doc` value in the dump parses as JSON.
fn at_rest_checks(s: &Stack, markers: &[String], secrets: &[String]) {
    let dump = s.compose(&[
        "exec",
        "-T",
        "db",
        "pg_dump",
        "-U",
        "postgres",
        "--data-only",
        "knoggin",
    ]);
    assert!(dump.status.success(), "pg_dump: {}", text(&dump));
    let dump = String::from_utf8_lossy(&dump.stdout).into_owned();
    let mut rows = 0;
    let mut in_state = false;
    let mut decoded: Vec<Vec<u8>> = Vec::new();
    for line in dump.lines() {
        if line.starts_with("COPY public.knoggin_state ") {
            in_state = true;
            continue;
        }
        if line == "\\." {
            in_state = false;
        }
        if in_state {
            let doc = line.rsplit('\t').next().unwrap();
            let hex = doc.strip_prefix("\\\\x").expect("doc is bytea");
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            assert!(
                serde_json::from_slice::<Value>(&bytes).is_err(),
                "a doc parses as JSON"
            );
            decoded.push(bytes);
            rows += 1;
        }
    }
    assert!(rows > 0, "the dump holds the state table");
    for m in markers.iter().chain(secrets) {
        assert!(!dump.contains(m.as_str()), "the dump holds `{m}`");
        assert!(
            !decoded
                .iter()
                .any(|d| d.windows(m.len()).any(|w| w == m.as_bytes())),
            "a sealed doc holds `{m}`"
        );
    }
    // The bucket stand-in's whole data folder, raw.
    let mut grep = vec!["exec", "-T", "s3", "grep", "-r", "-l", "-F"];
    for m in markers.iter().chain(secrets) {
        grep.extend(["-e", m.as_str()]);
    }
    grep.push("/data");
    let found = s.compose(&grep);
    assert_eq!(
        found.status.code(),
        Some(1),
        "the bucket holds a marker or a secret: {}",
        text(&found)
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs docker"]
async fn the_caddyfile_logs_no_request_address() {
    struct Gone(String);
    impl Drop for Gone {
        fn drop(&mut self) {
            let _ = docker(&["rm", "-f", "-v", &self.0]);
        }
    }
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let name = Gone(format!("kntest-caddy-{}", suffix()));
    docker_ok(&[
        "run",
        "-d",
        "--name",
        &name.0,
        "-e",
        "KNOGGIN_DOMAIN=:8080",
        "-p",
        &format!("127.0.0.1:{port}:8080"),
        "-v",
        &format!("{ROOT}/deploy/docker/Caddyfile:/etc/caddy/Caddyfile:ro"),
        &pinned_image("caddy"),
    ]);
    let marker = format!("caddymarker{}", suffix());
    let http = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/v1/ui/search?q={marker}");
    let mut status = 0;
    for _ in 0..60 {
        if let Ok(r) = http.get(&url).send().await {
            status = r.status().as_u16();
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert_eq!(status, 502, "no router behind the proxy");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let logs = text(&docker(&["logs", &name.0]));
    assert!(
        !logs.contains(&marker),
        "Caddy logged the request address:\n{logs}"
    );
}
