# Security

Knoggin encrypts everything it keeps, keeps users apart, and treats memory text as data, never as instructions. It does not protect you from whoever runs it. This page says what it defends, and every limit, once.

To report a vulnerability, see [SECURITY.md](../SECURITY.md): report privately through GitHub's private vulnerability reporting.

## Who is trusted

- **The operator is trusted.** Whoever runs a deployment holds the master key and can read every store. On a local install that is you.
- **Users are not trusted with each other's data.** That is the main promise of a server: one user's credentials reach only that user's stores.
- **A user's container is not trusted** by the router. It is treated as possibly compromised, and it holds no credentials.
- **Memory text is not trusted** by anything. It is data that some agent once wrote. Knoggin never follows a link in it, runs it, or treats it as an instruction or a credential.
- **On a local install,** other accounts on the machine are kept out. Programs that run as you are not.

## What Knoggin does

- **Encryption at rest.** Every store is one encrypted snapshot, with a key of its own, wrapped by the master key: in the OS keychain or a key file locally, in a key file on one host, in KMS on AWS. API keys and sessions are stored only as hashes. On a server, emails, names and app names are sealed in the state database.
- **Isolation.** On a server each user gets a container of their own, with no credentials, no disk and no network but the router. A container can get keys and storage links only for its own user's stores.
- **Narrow credentials.** An API key reaches one store, read-only or read-write, and always expires. An agent's sign-in token reaches only memories, never keys, apps, the account or admin pages.
- **The local socket.** Your own programs reach the local daemon through a Unix socket that only your account can open. There is no token file for another program to steal.
- **The browser.** The web UI shows every stored string as text, never as HTML, under a strict content security policy. The server refuses cross-site requests.
- **Logs.** Logs and metrics never hold memory text, keys, tokens, emails or names.

## Limits

These are known limits, not vulnerabilities:

1. **The operator of a deployment can read every store.**
2. **A container escape** reaches the other containers on the same host. On one host it reaches everything, because the router shares the kernel.
3. **A compromised router reads everything**, and on one host it can also delete snapshots. So can anyone who holds the router's internal secret or certificate key and has a place in the deployment's network outside the user containers. On AWS, whoever may describe ECS tasks or read CloudTrail can read the containers' launch secrets; on one host, whoever can use the Docker socket. These people are operators.
4. **Whoever can write to the state database controls every account.** On one host the lifecycle task's database login can delete any record; the nightly dump is the way back.
5. **A local install** does not defend against other programs that run as you, nor against someone who has both the key file and the data folder.
6. **A local data folder replaced by an older copy** is accepted as it is. Keys revoked since that copy work again.
7. **Another account on the same machine can take the local daemon's TCP port while Knoggin is stopped,** and keep it from starting. It then receives what is sent to that port: an API key from a program that uses the TCP address (the SDK with a `base_url`, or `knoggin mcp --url`) and, with sign-in on, whatever arrives through a tunnel. While Knoggin runs, it holds the port on both loopback addresses. Do not use tunnel mode on a machine you share with accounts you do not trust. Behind a tunnel, anyone who knows the address can also use up the daemon's allowance for requests without a credential, and so keep new sign-ins out while they keep it up.
8. **A crash loses the changes since the last snapshot,** a forget included. That is usually a few seconds, up to 30 seconds on a server under continuous writing, and longer while storage cannot be reached. A server container that was asked to park and has not saved after 2 hours is stopped and loses what it had not saved.
9. **A forgotten memory stays in older encrypted snapshots** until they expire: 7 days by default.
10. **Rotating the master key does not help against a key that has already leaked:** the store keys stay the same. Export each store, import it into a new one, and delete the old one.
11. **Removing a person at the identity provider** does not end their existing sessions and API keys. Remove the user in Knoggin too. Removal is not a ban: after the undo period, the same person can sign in again with a new, empty account.
12. **Knoggin cannot stop an agent from being misled by a memory it reads.**
13. **No cap on how much a user stores.** What a user can have open at once is bounded by the container's memory. On AWS a container has a CPU share and no cap, so one busy user can slow others on the same host. The hourly upload budget bounds the rate, not the total: a user whose container runs hostile code could fill a small bucket within hours. Watch the bucket's size on a public server, and set `limits.upload_bytes_per_hour` lower on a small disk.
14. **With sign-in on and no allow-list,** everyone your identity provider admits gets an account and a container. `containers.max` bounds the cost.
15. **Apps separate stores, not people.** Locally, every program that runs as you can reach every store. On a server, a compromised container reaches all stores of its own user, and one user's apps share that user's limits, so an agent with a key to one app can slow the same user's other apps, though it can read none of them.
16. **A sign-in token for any of the three addresses** (`/mcp`, `/mcp/plugin`, the REST API) works on all three. The plugin address is a smaller tool set, not a smaller permission. Such a token can save and forget memories in the personal store, but cannot delete a store or the account.
17. **A store key taken from a compromised container** keeps opening that store's later snapshots. The way out is the same as in item 10.
18. **On macOS, and on Linux systems that do not resolve names under `localhost` by themselves,** the local web UI runs on `127.0.0.1` with a port that changes on every start. Another account that once held the same port could have planted a cached script for it.
19. **Knoggin does not scan saved text for secrets.** The memory skill and the tool descriptions tell agents never to save them.

A deployer who needs a stronger wall between users can run the user containers under a sandboxing runtime such as gVisor. That is not built or tested in this version.

## What you can do

- Back up the master key, and keep it apart from the data ([Local install](local-install.md)).
- Use full-disk encryption on machines that run Knoggin.
- Give agents that run tools without asking a read-only key ([Apps and API keys](apps-and-keys.md)).
- On a server, set an allow-list and name admins by subject ([Sign-in](sign-in.md)).
- Never log request addresses in front of Knoggin: a request line can carry a search text or a one-time code.
