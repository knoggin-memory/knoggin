# Contributing to Knoggin

Thank you for helping. This page says how to build and test Knoggin, what a commit must look like, and how to sign off your work.

## Build and test

You need Rust (rustup installs the version that `rust-toolchain.toml` pins) and [cargo-nextest](https://nexte.st). The web UI needs Node and pnpm, the Python SDK needs uv, and the stack tests need Docker.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked
cargo nextest run --workspace --locked        # the fast tests
cargo test --doc --workspace --locked
```

No fast test needs the network, an account or the real embedding model. Some tiers are ignored by default and have their own command:

```sh
# Stack tests (need Docker)
cargo nextest run --workspace --locked --run-ignored only -E 'kind(test) & binary(#stack_*)'
# Latency budgets (meant for a quiet 4-core arm64 machine)
cargo nextest run -p knoggin-core --release --locked --run-ignored only -E 'binary(budgets)'
```

CI also runs `cargo deny check`, `cargo machete`, `cargo doc` with warnings denied, and `actionlint` and `zizmor` on the workflows. CI denies compiler warnings (`CARGO_BUILD_WARNINGS=deny`); a local build does not, so run clippy before you push.

## Commits

- **Sign off every commit** with `git commit -s`, using your real name and email as configured in git. CI checks that every commit in a pull request has a `Signed-off-by` line that matches its author. To sign off commits you already made: `git rebase --signoff main`.
- **Use [Conventional Commit](https://www.conventionalcommits.org/) subjects**: `feat:`, `fix:`, `docs:`, `test:`, `refactor:`, `chore:`, `ci:`. The release tool builds the changelog and the version from them.
- **One logical change per commit.** Each commit builds and passes `cargo fmt --check`, clippy and the fast tests.
- Every source file starts with `// SPDX-License-Identifier: AGPL-3.0-only`, and every Python file under `sdk/python` with `# SPDX-License-Identifier: Apache-2.0`.

Pull requests are squash-merged into `main`.

## Licence and sign-off

Knoggin is licensed under the AGPL-3.0 (`AGPL-3.0-only`), and your contributions are too. The Python SDK in `sdk/python` is licensed under the Apache License 2.0 (`Apache-2.0`), and contributions to it are too. There is **no contributor licence agreement**. Instead, the `Signed-off-by` line states that you agree to the Developer Certificate of Origin below.

```text
Developer Certificate of Origin
Version 1.1

Copyright (C) 2004, 2006 The Linux Foundation and its contributors.

Everyone is permitted to copy and distribute verbatim copies of this
license document, but changing it is not allowed.


Developer's Certificate of Origin 1.1

By making a contribution to this project, I certify that:

(a) The contribution was created in whole or in part by me and I
    have the right to submit it under the open source license
    indicated in the file; or

(b) The contribution is based upon previous work that, to the best
    of my knowledge, is covered under an appropriate open source
    license and I have the right under that license to submit that
    work with modifications, whether created in whole or in part
    by me, under the same open source license (unless I am
    permitted to submit under a different license), as indicated
    in the file; or

(c) The contribution was provided directly to me by some other
    person who certified (a), (b) or (c) and I have not modified
    it.

(d) I understand and agree that this project and the contribution
    are public and that a record of the contribution (including all
    personal information I submit with it, including my sign-off) is
    maintained indefinitely and may be redistributed consistent with
    this project or the open source license(s) involved.
```

## Security issues

Do not open a public issue for a vulnerability. [SECURITY.md](SECURITY.md) says how to report one.
