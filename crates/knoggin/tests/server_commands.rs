// SPDX-License-Identifier: AGPL-3.0-only
//! The server commands' arguments and refusals (tab "Config, CLI, local install", Server
//! commands): every missing or unsupported setting is refused before anything connects,
//! with the setting's name, and wrong usage exits with 2. Nothing here reaches a network.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use support::Home;

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// Runs `args` with `--config <file>` holding `config`; asserts the exit code and that
/// stderr names `needle`.
fn refused(home: &Home, config: &str, args: &[&str], code: i32, needle: &str) {
    let file = home.dir.path().join("server.toml");
    write(&file, config);
    let mut all = vec!["--config", file.to_str().unwrap()];
    all.extend_from_slice(args);
    let o = home.run(&all);
    assert_eq!(o.code, code, "{args:?}: {}{}", o.stdout, o.stderr);
    assert!(
        o.stderr.contains(needle),
        "{args:?} wants {needle:?}: {}",
        o.stderr
    );
}

#[test]
fn the_router_refuses_each_missing_or_unbuilt_setting_before_it_connects() {
    let home = Home::new();
    let d = home.dir.path();
    let token = d.join("launcher_token");
    write(&token, "t0ken\n");
    let o = home.run(&[
        "key",
        "generate",
        "--out",
        d.join("master_key").to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = home.run(&[
        "key",
        "generate",
        "--out",
        d.join("state_key").to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let secret = d.join("internal_secret");
    write(&secret, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\n");
    let o = home.run(&["admin", "internal-cert", "--out-dir", d.to_str().unwrap()]);
    assert_eq!(o.code, 0, "{}", o.stderr);

    let mut config = String::new();
    let step = |config: &mut String, add: &str, needle: &str| {
        config.push_str(add);
        refused(&home, config, &["router"], 1, needle);
    };
    step(&mut config, "", "launcher.kind is required");
    step(&mut config, "[launcher]\nkind = \"ecs\"\n", "milestone 7");
    config = config.replace("kind = \"ecs\"", "kind = \"docker\"");
    step(&mut config, "", "launcher.docker.token");
    step(
        &mut config,
        &format!("[launcher.docker]\ntoken_file = \"{}\"\n", token.display()),
        "containers.max must be set",
    );
    step(
        &mut config,
        "[containers]\nmax = 4\n",
        "keys.provider is required",
    );
    step(
        &mut config,
        "[keys]\nprovider = \"kms\"\nkms_key_id = \"arn:aws:kms:x\"\n",
        "keys.provider = \"kms\" is not built yet",
    );
    config = config.replace("provider = \"kms\"", "provider = \"key_file\"");
    step(&mut config, "", "keys.master_key_file");
    config = config.replace(
        "kms_key_id = \"arn:aws:kms:x\"",
        &format!("master_key_file = \"{}\"", d.join("master_key").display()),
    );
    step(&mut config, "", "state.backend is required");
    step(
        &mut config,
        "[state]\nbackend = \"dynamodb\"\n",
        "milestone 7",
    );
    config = config.replace("backend = \"dynamodb\"", "backend = \"local_file\"");
    step(&mut config, "", "local daemon's");
    config = config.replace("backend = \"local_file\"", "backend = \"postgres\"");
    step(&mut config, "", "state.postgres_url");
    let url = d.join("postgres_url");
    // No `sslmode`: refused before any connection is tried.
    write(&url, "postgres://router@127.0.0.1:1/knoggin\n");
    config = config.replace(
        "backend = \"postgres\"",
        &format!(
            "backend = \"postgres\"\npostgres_url_file = \"{}\"",
            url.display()
        ),
    );
    step(&mut config, "", "state.postgres_doc_key_file");
    config = config.replace(
        "backend = \"postgres\"",
        &format!(
            "backend = \"postgres\"\npostgres_doc_key_file = \"{}\"",
            d.join("state_key").display()
        ),
    );
    step(&mut config, "", "internal.secret is required");
    step(
        &mut config,
        &format!(
            "[internal]\nsecret_file = \"{}\"\ncert_file = \"{}\"\nkey_file = \"{}\"\n",
            secret.display(),
            d.join("internal_cert.pem").display(),
            d.join("internal_key.pem").display()
        ),
        "storage.bucket is required",
    );
    step(
        &mut config,
        "[storage]\nbucket = \"snapshots\"\n",
        "server.public_url is required",
    );
    step(
        &mut config,
        "[server]\npublic_url = \"http://127.0.0.1:8080\"\n",
        "sslmode",
    );
}

#[test]
fn the_other_roles_refuse_what_they_miss_and_wrong_usage_exits_2() {
    let home = Home::new();
    refused(
        &home,
        "",
        &["lifecycle", "--every", "10x"],
        2,
        "whole number",
    );
    refused(
        &home,
        "",
        &["lifecycle", "--every", "0s"],
        2,
        "whole number",
    );
    refused(&home, "", &["lifecycle"], 1, "state.backend is required");
    refused(
        &home,
        "[state]\nbackend = \"postgres\"\n",
        &["lifecycle", "--every", "10m"],
        1,
        "state.postgres_url",
    );
    refused(&home, "", &["launcher"], 1, "launcher.docker.token");
    // A user container reads no config file and needs its five values.
    refused(
        &home,
        "this is not toml",
        &["serve"],
        1,
        "KNOGGIN_CONTAINER_ID",
    );
    refused(
        &home,
        "",
        &["admin", "restore", "--store", "x"],
        2,
        "--snapshot",
    );
    refused(
        &home,
        "",
        &[
            "admin",
            "restore",
            "--store",
            "x",
            "--snapshot",
            "1-1",
            "--cancel",
        ],
        2,
        "cannot be used with",
    );
    refused(&home, "", &["admin", "retire-before"], 2, "<WHEN>");
    refused(
        &home,
        "[keys]\nprovider = \"kms\"\nkms_key_id = \"arn\"\n",
        &["admin", "selftest"],
        1,
        "keys.provider = \"kms\" is not built yet",
    );
    // On KMS, rotation is the provider's own and nothing is done.
    let file = home.dir.path().join("server.toml");
    write(&file, "[keys]\nprovider = \"kms\"\nkms_key_id = \"arn\"\n");
    let o = home.run(&[
        "--config",
        file.to_str().unwrap(),
        "admin",
        "rotate-master-key",
    ]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("KMS rotates its own key material"));
}

#[test]
fn key_generate_and_internal_cert_write_new_private_files_only() {
    let home = Home::new();
    let d = home.dir.path();
    let out = d.join("state_key");
    let o = home.run(&["--json", "key", "generate", "--out", out.to_str().unwrap()]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.json()["kekId"].as_str().unwrap().starts_with("local:"));
    assert_eq!(
        std::fs::metadata(&out).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let set: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(set["version"], 1);
    let o = home.run(&["key", "generate", "--out", out.to_str().unwrap()]);
    assert_ne!(o.code, 0, "an existing file is never replaced");

    let o = home.run(&["admin", "internal-cert", "--out-dir", d.to_str().unwrap()]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    for name in ["internal_cert.pem", "internal_key.pem"] {
        let p = d.join(name);
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600,
            "{name}"
        );
    }
    assert!(
        std::fs::read_to_string(d.join("internal_cert.pem"))
            .unwrap()
            .starts_with("-----BEGIN CERTIFICATE-----")
    );
    let o = home.run(&["admin", "internal-cert", "--out-dir", d.to_str().unwrap()]);
    assert_ne!(o.code, 0, "existing files are never replaced");
}
