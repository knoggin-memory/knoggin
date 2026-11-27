// SPDX-License-Identifier: AGPL-3.0-only
//! The Python SDK's contract tests against the one-host stack (tabs "Testing, CI, releases"
//! and "SDK and integrations"): the same calls the SDK tier makes against a daemon, here
//! against the router, once with an app's API key and once with a user's OAuth access
//! token. That is what "works against both versions" means. Needs `uv` on `PATH`; the
//! tests run with `uv run --locked pytest tests/contract` in `sdk/python`, given the
//! address as `KNOGGIN_TEST_URL` and the credential as `KNOGGIN_TEST_API_KEY`.
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

use std::process::Command;

use stack::{Cred, ROOT, Stack};

fn contract_tests(stack: &Stack, credential: &str, what: &str) {
    let status = Command::new("uv")
        .args([
            "run",
            "--locked",
            "pytest",
            "tests/contract",
            "-q",
            "-p",
            "no:cacheprovider",
        ])
        .current_dir(format!("{ROOT}/sdk/python"))
        .env("KNOGGIN_TEST_URL", &stack.base)
        .env("KNOGGIN_TEST_API_KEY", credential)
        .env_remove("KNOGGIN_BASE_URL")
        .env_remove("KNOGGIN_API_KEY")
        .env_remove("KNOGGIN_NAMESPACE")
        .env_remove("KNOGGIN_HOME")
        .status()
        .expect("uv must be on PATH to run the SDK contract tests");
    assert!(
        status.success(),
        "the SDK contract tests failed with {what}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs docker"]
async fn the_sdk_contract_tests_pass_against_the_one_host_stack() {
    let s = Stack::up().await;
    let ana = s.sign_in("ana").await;
    let app = s.app(&ana, "sdk-contract").await;
    let (_, key) = s.key(&ana, &app, "sdk").await;
    let Cred::Bearer(key) = key else {
        unreachable!("a key is a bearer credential")
    };
    contract_tests(&s, &key, "an app's API key");
    // A fresh token: the test issuer's tokens live 5 minutes.
    let token = s.token("ana").await;
    contract_tests(&s, &token, "an OAuth access token");
}
