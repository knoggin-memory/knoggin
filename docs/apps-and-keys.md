# Apps and API keys

An **app** is a separate store of memories for one agent or one program, with its own API keys. Your **personal store** is the one your own agents use by default. Apps keep stores apart: a key for one app reaches nothing else.

## Apps

Make an app in the web UI (Apps, then New app), or on a local install:

```sh
knoggin apps create research
knoggin apps list
knoggin apps delete research
```

- A name is 1 to 64 characters and unique among your stores, ignoring letter case.
- You can have up to 1,000 apps.
- Deleting an app stops its keys at once. The app can be brought back for `retention.soft_delete_days` (7 days by default), from the web UI; after that it is erased for good. A store brought back returns as of its last snapshot, so writes from the last seconds before the delete may be missing.

## API keys

A key looks like `knoggin_live_` followed by 43 characters. It is shown **once**, when it is made. Knoggin keeps only a hash of it and its first 8 characters, so that you can tell keys apart.

```sh
knoggin keys create --app research --name laptop-agent             # read and write, 90 days
knoggin keys create --app research --read-only --expires-days 30   # read only
knoggin keys list --app research
knoggin keys revoke <key id> --app research
```

Without `--app`, these commands act on your personal store. On a server, make keys in the web UI's API keys page.

- **A key reaches one store**, with the permissions it was made with: read, or read and write. There is no write-only key.
- **A read-only key** can recall and list, but never save, change or forget. A recall with it is not recorded either. It is how you stop an agent from saving: an agent with a read-write key saves without asking you.
- **Every key expires**, after 30, 90, 180 or 365 days; 90 by default. From 14 days before, every answer carries the header `Knoggin-Key-Expires`, and the Python SDK warns once.
- **Rotating** a key, in the web UI, makes a new key with the same name and permissions and revokes the old one at once. The new key lives 90 days unless you choose another lifetime.
- **Revoking** stops a key on its next request. An open event stream, export or import that uses it ends within a minute.
- A key gives access only; it encrypts nothing. Revoking or rotating it never touches your memories.
- A store holds at most 1,000 key records, revoked and expired ones included. Records leave 30 days after a key expired or was revoked, so normal rotation never reaches the limit.

## Using a key

Put the key in the environment variable `KNOGGIN_API_KEY`. Never write it into a config file.

- **With `knoggin mcp`** on a local install: the bridge sends the key, and the agent reaches that key's store instead of your personal one.
- **By address:** send `Authorization: Bearer <key>` to `https://<your server>/mcp`, or to `http://127.0.0.1:7690/mcp` on a local install. Agents that can only run a command use `knoggin mcp --url <address>/mcp`, which reads `KNOGGIN_API_KEY` and refuses to send it over plain `http` to anything but a loopback address.
- **The Python SDK** reads `KNOGGIN_API_KEY` too ([Python SDK](python-sdk.md)).

A key cannot reach the account, other apps, other keys or admin pages. Only a signed-in browser, or on a local install your own account, can manage those.

## Where keys can be made

- **Local install:** only by you, on your machine: the command line, or the web UI opened with `knoggin open`. A browser that signed in through a tunnel cannot make, rotate or revoke keys.
- **Server:** in the web UI, while signed in. An agent's sign-in token cannot make keys.

See also [Connecting agents](connect-agents.md) and [Security](security.md).
