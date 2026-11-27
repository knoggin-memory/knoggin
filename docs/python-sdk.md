# Python SDK

The `knoggin` package is a small Python client for Knoggin's REST API. The same code works against a local install and against a server. It needs Python 3.11 or newer, and depends only on `httpx` and `pydantic`.

```sh
pip install knoggin        # or: uv add knoggin
```

## Connect

```python
import os
from knoggin import Client

client = Client()   # the local daemon, over its Unix socket: no key needed
client = Client(base_url="https://memory.example.com", api_key=os.environ["KNOGGIN_API_KEY"])
```

| Argument | Environment | Default |
| --- | --- | --- |
| `base_url` | `KNOGGIN_BASE_URL` | None: the local daemon's socket, `<KNOGGIN_HOME or ~/.knoggin>/run/daemon.sock` |
| `api_key` | `KNOGGIN_API_KEY` | None. Over the socket a key selects that key's app store |
| `namespace` | `KNOGGIN_NAMESPACE` | None: the server's default namespace |
| `timeout` | | 30 seconds |
| `max_retries` | | 2 |

Arguments win over the environment. Two rules hold before anything is sent:

- **The local daemon is reached only over its socket.** If it is not running, the client raises `ConfigurationError` with the hint `knoggin start`. It never looks for it on a TCP port.
- **A key never travels in the clear.** The client refuses to send a key over plain `http` to anything but `localhost`, `127.0.0.0/8` or `[::1]`.

## Use

```python
saved = client.memories.store(
    "Maya prefers window seats",
    full_text="She said so when booking the Lisbon trip.",
    tags=["travel"],
    entities=["Maya"],
)

for hit in client.memories.recall("Which seat does Maya like?").memories:
    print(hit.date, hit.summary)

context = client.context(topic="trip planning")   # what an agent loads at the start of a task
client.memories.supersede(saved.id, "Maya prefers aisle seats now")
client.memories.forget(saved.id)                  # cannot be undone
```

`AsyncClient` has the same methods as coroutines. The full set:

- `context`; `memories.store`, `store_many`, `supersede`, `recall`, `get`, `list`, `reinforce`, `forget`, `forget_many`, `move`, `similar`;
- `namespaces.list`, `create`, `stats`, `update_policy`, `delete`; `tags.list`;
- `export(path)` and `import_(path)`.

There are no admin or account methods: the SDK is for memories. Apps and keys are managed in the web UI or with the `knoggin` command ([Apps and API keys](apps-and-keys.md)).

A recall returns 25 memories by default, the best 5 with full text. Pass `limit` and `full_text_count` to change that. `list` follows the pages for you.

## Errors and retries

Every error is a `KnogginError` with `code`, `message`, `field`, `reason`, `request_id` and `status`. The subclasses follow the code: `AuthenticationError`, `PermissionDeniedError`, `NotFoundError`, `ConflictError`, `InvalidRequestError`, `RateLimitedError` (with `retry_after`), `StoreUnavailableError`, `ServerError`, `KnogginConnectionError`, `KnogginTimeoutError` and `ConfigurationError`.

Connection errors, timeouts and the codes `rate_limited`, `unavailable`, `upgrading` and `timeout` are retried with back-off, honouring `Retry-After`. Every write but an import carries one idempotency key for all its attempts, so a retry is never applied twice. A write that ends without a known outcome raises `WriteOutcomeUnknown`. An import is sent once; sending the same file again is safe, because lines already there are skipped.

## Typed memories

```python
from pydantic import BaseModel

class Trip(BaseModel):
    city: str
    days: int

client.memories.store_model(Trip(city="Lisbon", days=3))
trips = client.memories.recall_models("Lisbon", Trip)   # list[Trip]
```

`store_model` saves the model as JSON in the full text, with the tag `type/trip`. `recall_models` recalls only memories with that tag and parses each one; a memory that does not fit is skipped.

## Pydantic AI

```sh
pip install "knoggin[pydantic-ai]"
```

```python
from pydantic_ai import Agent
from knoggin.pydantic_ai import Memory

agent = Agent("<model>", capabilities=[Memory()])
```

`Memory` gives the agent two tools, `recall_memories` and `remember`, and a short instruction on when to use them. `remember` needs your approval by default (`approve_writes=True`). It reads `KNOGGIN_BASE_URL` and `KNOGGIN_API_KEY`, or uses the local daemon; pass `client=` to give it your own `AsyncClient`.

## OpenAI Agents SDK

```sh
pip install "knoggin[openai-agents]"
```

```python
from agents import Agent, Runner
from knoggin.openai_agents import mcp_server

async with mcp_server("https://memory.example.com") as server:   # reads KNOGGIN_API_KEY
    agent = Agent(name="assistant", mcp_servers=[server])
    result = await Runner.run(agent, "What do you remember about the Lisbon trip?")
```

`mcp_server` returns the Agents SDK's streamable-HTTP MCP server pointed at Knoggin's `/mcp`, with approval required for every tool that writes. It needs an address: an MCP client cannot use the daemon's socket, so a local install is reached at `http://127.0.0.1:7690` with an API key.

Do not send a key to that address on a machine shared with accounts you do not trust. While Knoggin is stopped, another account can take port 7690 and receive the key ([Security](security.md), limit 7). The socket has no such gap, so it is the safe default: `Client()` uses it, and an MCP client can run the stdio command `knoggin mcp`, which reaches the daemon over the socket (in the Agents SDK, `MCPServerStdio`).

## The `knoggin` command

The package also installs a `knoggin` command. `knoggin init` installs the Knoggin program and connects your agents ([Getting started](getting-started.md)). Any other command is passed to the program in `~/.knoggin/bin`, which is installed first if it is missing. So `uvx knoggin mcp` works as an MCP server on a fresh machine.
