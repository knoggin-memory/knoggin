<!-- SPDX-License-Identifier: Apache-2.0 -->
# knoggin

<!-- mcp-name: io.github.knoggin-memory/knoggin -->

The Python client for [Knoggin](https://github.com/knoggin-memory/knoggin), long-term memory for AI agents.
It works against a local Knoggin daemon and against a Knoggin server: both serve the same REST API.

```sh
pip install knoggin                  # or: uv add knoggin
uvx knoggin init                     # install Knoggin and connect the agents on this machine
```

## Use

```python
from knoggin import Client

client = Client()  # the local daemon, over its Unix socket; no key needed
# client = Client(base_url="https://memory.example.com", api_key="knoggin_live_...")

saved = client.memories.store("Maya prefers window seats", tags=["travel"], entities=["Maya"])
for hit in client.memories.recall("Which seat does Maya like?").memories:
    print(hit.summary)
```

`AsyncClient` has the same methods as coroutines. The methods: `context`, `memories.store`, `store_many`,
`supersede`, `recall`, `get`, `list`, `reinforce`, `forget`, `forget_many`, `move`, `similar`,
`store_model`, `recall_models`; `namespaces.list`, `create`, `stats`, `update_policy`, `delete`;
`tags.list`; `export` and `import_`.

| Argument | Environment | Default |
| --- | --- | --- |
| `base_url` | `KNOGGIN_BASE_URL` | None: the local daemon at `<KNOGGIN_HOME or ~/.knoggin>/run/daemon.sock` |
| `api_key` | `KNOGGIN_API_KEY` | None |
| `namespace` | `KNOGGIN_NAMESPACE` | None; sent as `Knoggin-Namespace` |
| `timeout` | | 30 seconds |
| `max_retries` | | 2 |

Arguments win over the environment. The client never sends an API key over plain `http` to a host that is
not a loopback address, and never looks for a local daemon on a TCP port: with no daemon running it raises
`ConfigurationError` with the hint `knoggin start`.

Errors are subclasses of `KnogginError`, which carries `code`, `message`, `field`, `reason`, `request_id`
and `status`. Connection errors, timeouts and the codes `rate_limited`, `unavailable`, `upgrading` and
`timeout` are retried, honouring `Retry-After`. Every write except an import carries one idempotency key
for all its attempts, so a retry cannot apply it twice. A write that ends without an outcome raises
`WriteOutcomeUnknown`.

## Typed memories

```python
from pydantic import BaseModel


class Trip(BaseModel):
    city: str
    days: int


client.memories.store_model(Trip(city="Lisbon", days=3))
trips = client.memories.recall_models("Lisbon", Trip)  # list[Trip]
```

## Agent frameworks

```python
# pip install "knoggin[pydantic-ai]"
from pydantic_ai import Agent
from knoggin.pydantic_ai import Memory

agent = Agent("<model>", capabilities=[Memory()])  # a recall tool, and a remember tool that needs approval

# pip install "knoggin[openai-agents]"
from knoggin.openai_agents import mcp_server

server = mcp_server("https://memory.example.com", api_key="knoggin_live_...")  # writing tools need approval
```

## The `knoggin` command

`knoggin init` installs the Knoggin binary and connects Claude Code, Codex, Cursor, VS Code, Zed and
opencode, showing each change and asking first (`--dry-run`, `--yes`, `--server <url>`, `--uninstall`).
Any other command is passed to the binary at `~/.knoggin/bin/knoggin`, which is installed first if it is
missing, so `uvx knoggin mcp` is a working stdio MCP server.

## Develop

```sh
uv run --locked pytest       # unit tests, and contract tests against a daemon in test mode
uv run ruff check . && uv run ruff format --check . && uv run pyright && uv run mypy
uv build
```

The framework adapters' tests run one framework at a time, with that framework's dependency group
(`litellm`, `pydantic-ai`, `openai-agents`, `langchain` or `google-adk`):

```sh
uv run --locked --group pydantic-ai pytest tests/adapters/pydantic_ai
uv run --locked --group pydantic-ai pyright -p pyright-adapters.json src/knoggin/pydantic_ai.py tests/adapters/*.py tests/adapters/pydantic_ai
```

The contract tests start `knoggin daemon` in test mode in a temporary home, from `KNOGGIN_BIN` or the
repository's `target/debug/knoggin` (`cargo build -p knoggin`), and skip without one. With
`KNOGGIN_TEST_URL` and `KNOGGIN_TEST_API_KEY` they run against that address instead.

The Python SDK is licensed under the Apache License 2.0 (`Apache-2.0`); the rest of Knoggin is `AGPL-3.0-only`.
