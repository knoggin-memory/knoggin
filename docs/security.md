# Security

Knoggin encrypts everything it keeps, keeps users apart, and treats memory text as data, never as instructions. This page says what it defends and how to run it safely.

To report a vulnerability, see [SECURITY.md](../SECURITY.md): report privately through GitHub's private vulnerability reporting.

## Who is trusted

- **The operator is trusted.** Whoever runs a deployment holds the master key. On a local install that is you.
- **Users are not trusted with each other's data.** That is the main promise of a server: one user's credentials reach only that user's stores.
- **A user's container is not trusted** by the router. It is treated as possibly compromised, and it holds no credentials.
- **Memory text is not trusted** by anything. It is data that some agent once wrote. Knoggin never follows a link in it, runs it, or treats it as an instruction or a credential.
- **On a local install,** other accounts on the machine are kept out.

## What Knoggin does

- **Encryption at rest.** Every store is one encrypted snapshot, with a key of its own, wrapped by the master key: in the OS keychain or a key file locally, in a key file on one host, in KMS on AWS. API keys and sessions are stored only as hashes. On a server, emails, names and app names are sealed in the state database.
- **Isolation.** On a server each user gets a container of their own, with no credentials, no disk and no network but the router. A container can get keys and storage links only for its own user's stores.
- **Narrow credentials.** An API key reaches one store, read-only or read-write, and always expires. An agent's sign-in token reaches only memories, never keys, apps, the account or admin pages.
- **The local socket.** Your own programs reach the local daemon through a Unix socket that only your account can open. There is no token file for another program to steal.
- **The browser.** The web UI shows every stored string as text, never as HTML, under a strict content security policy. The server refuses cross-site requests.
- **Logs.** Logs and metrics never hold memory text, keys, tokens, emails or names.

## What you can do

- Back up the master key, and keep it apart from the data ([Local install](local-install.md)).
- Use full-disk encryption on machines that run Knoggin.
- Give agents that run tools without asking a read-only key ([Apps and API keys](apps-and-keys.md)).
- On a server, set an allow-list and name admins by subject ([Sign-in](sign-in.md)).
- Never log request addresses in front of Knoggin: a request line can carry a search text or a one-time code.
