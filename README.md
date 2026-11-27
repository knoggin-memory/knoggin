# Knoggin

Knoggin is a memory server for AI agents. It stores short text memories and finds them again by meaning, with recall that weighs how recent, how strong and how connected each memory is. Agents reach it over MCP or REST, and the same engine runs as a local single-user daemon or as a multi-user server.

> Knoggin is being built. No release exists yet, so the commands below describe the first release.

## Run it

**On your own machine.** One command installs the binary and connects the agents it finds (Claude Code, Codex, Cursor, VS Code, Zed, opencode). It shows each change and asks before making it:

```sh
uvx knoggin init
```

Without Python, use the installer from the latest release, then start the daemon and open the web UI:

```sh
curl -fsSL https://github.com/knoggin-memory/knoggin/releases/latest/download/install.sh | sh
knoggin start
knoggin open
```

Everything stays on your machine, encrypted at rest under `~/.knoggin/`. The embedding model is downloaded once on first run.

**As a server for several people.** On one host with Docker and an S3-compatible bucket, from `deploy/docker/`:

```sh
./make-secrets.sh
docker compose up -d
```

`deploy/docker/README.md` lists the settings to fill in first. On AWS, use the OpenTofu module in `deploy/aws/`; [`docs/deploy-aws.md`](docs/deploy-aws.md) covers variables, sizing and upgrades.

## Connect an agent

- **Local:** the agent runs the command `knoggin mcp`, for example `claude mcp add knoggin -- knoggin mcp`. `knoggin init` sets this up for you.
- **Server:** point the agent at `https://<your server>/mcp`. The agent signs in through your identity provider.
- **Code:** the Python SDK (`pip install knoggin`) works against both.

[Connecting agents](docs/connect-agents.md) has a section per agent, and [`integrations/connectors/`](integrations/connectors) has the config snippets. Claude Code and Codex can also install the [Knoggin plugin](integrations/plugin/README.md).

## Verify a release

Every archive carries a build attestation, and every image is signed:

```sh
gh attestation verify knoggin-<version>-<target>.tar.gz --repo knoggin-memory/knoggin

cosign verify ghcr.io/knoggin-memory/knoggin@sha256:<digest> \
  --certificate-identity https://github.com/knoggin-memory/knoggin/.github/workflows/release.yml@refs/heads/main \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

`install.sh` always checks the SHA-256 of what it downloads, and checks the attestation too when `gh` or `cosign` is installed.

## Docs

- [Getting started](docs/getting-started.md)
- [Local install](docs/local-install.md) and [moving from recalld](docs/migrate-from-recalld.md)
- Deploying [on one host](docs/deploy-one-host.md) or [on AWS](docs/deploy-aws.md)
- [Connecting agents](docs/connect-agents.md)
- [Sign-in](docs/sign-in.md), [apps and keys](docs/apps-and-keys.md), [the Python SDK](docs/python-sdk.md)
- [Security](docs/security.md) and [operations](docs/operations.md)
- [Contributing](CONTRIBUTING.md)

## Licence

Knoggin is licensed under the [GNU Affero General Public License, version 3](LICENSE) (`AGPL-3.0-only`).

The AGPL gives the users of a network service the right to its source. Every deployment links to its source: the setting `source_url` (default `https://github.com/knoggin-memory/knoggin`) appears in the web UI, the session route and the MCP server instructions. **If you deploy a changed version, point `source_url` at your changed source.**
