# Security

## Reporting a vulnerability

Report it privately through GitHub's private vulnerability reporting on this repository: **Security → Report a vulnerability** at <https://github.com/knoggin-memory/knoggin/security>. Do not open a public issue.

- You get a first answer within 7 days.
- Fixes are released as a new version and described in the changelog.
- There is no bounty.

## Known limits

These are known and stated limits of Knoggin's design. They are not vulnerabilities, so please do not report them as such.

1. The operator of a deployment can read every store.
2. A container escape reaches the other containers on the same host, and on the one-host target it reaches everything.
3. A compromised router reads everything, and on one host it can also delete snapshots. So can anyone who holds `internal.secret` or the internal certificate's key and has a place in the deployment's network outside the user containers. One container's launch secret opens that container's user to someone with the same place in the network. On AWS whoever may describe ECS tasks or read CloudTrail can read launch secrets, and on one host whoever can use the Docker socket. These are operators of the deployment (item 1).
4. Whoever can write to the state database controls every account. On one host the lifecycle task's database login can delete any record, the field key among them; the nightly dump is the way back. On AWS the lifecycle role cannot delete the field key.
5. A local install does not defend against other programs that run as the same person, nor against someone with the key file and the data folder.
6. A local data folder that is replaced by an older copy is accepted as it is. Keys that were revoked after that copy was made work again. Restoring a backup is the owner's own act.
7. Another account on the same machine can take the daemon's fixed TCP port while Knoggin is stopped, and so keep it from starting. It then receives whatever is sent to that port: an API key from a local program that uses the TCP address (the SDK with a `base_url`, or `knoggin mcp --url`, are such programs) and, with sign-in on, whatever arrives through the tunnel. The owner's browser session is not exposed, because it lives on a listener whose address is new on every run (except where the browser listener falls back to `127.0.0.1`). While Knoggin runs it holds its ports on both loopback addresses, `127.0.0.1` and `[::1]`, so nobody else can take them then. Do not use the tunnel mode on a machine that is shared with accounts you do not trust. Behind a tunnel, anyone who knows the public address can also use up the daemon's one bucket for requests without a credential, and so keep new sign-ins and new agent connections out for as long as they keep it up. Requests with a valid credential are not affected.
8. A crash loses the changes since the last finished snapshot, including a forget. While the bucket cannot be reached, that can be much more than a few seconds. A container that was asked to park and still has not saved after 2 hours, although the bucket works, is stopped and loses what it had not saved.
9. A forgotten memory stays in older encrypted snapshots until they expire.
10. Rotating the master key does not help against a key that has already leaked: the data keys stay the same. The way out is to export each store, import it into a new store and delete the old one.
11. Removing a person at the identity provider does not end their existing sessions and API keys. An admin must also remove the user in Knoggin. Removing a user in Knoggin is not a ban.
12. Knoggin cannot stop an agent from being misled by a memory it reads.
13. Knoggin sets no cap on how much a user stores over time. What a user can have open at once is bounded by the memory of the container, which the deployer sets. That is a limit of the machine, not a product limit. On AWS a user's container has a CPU share and no cap, so one busy user can slow the others on the same host. The hourly upload budget bounds the rate, not the total, and its default is set so that honest use never meets it: about 171 GiB an hour at the default container size. A user whose container runs hostile code can therefore fill a small bucket within hours, and on one host that would stop every user's snapshots. A deployer with a small disk sets `limits.upload_bytes_per_hour` lower. A deployer of a public server watches the bucket's size and removes such a user.
14. With sign-in on and no allow-list, everyone whom the identity provider admits gets an account and a container. The router logs a warning at start when both allow-lists are empty, and `containers.max` bounds the cost.
15. Apps separate stores, not people. On a local install every program that runs as the owner can reach every store through the socket. On a server a compromised container can reach all stores of its own user. On a server the apps of one user also share that user's limits: the request rate, the open event streams, exports and imports, and the container's memory budget. So an agent that holds a key to one app can deny service to the same user's other stores, although it can read none of them.
16. An access token that the identity provider issued for any of the three addresses (`/mcp`, `/mcp/plugin`, the REST API) is accepted on all three. The plugin address is a smaller tool set, not a smaller permission. Such a token can save and forget memories in the personal store. It cannot delete a store or the account.
17. A data key that was taken from a compromised container keeps opening that store's later snapshots. The way out is the same as in item 10.

A deployer who needs a stronger wall between users can run the user containers under a sandboxing runtime such as gVisor, by setting the container runtime in the launcher's description. That is not built, tested or documented in the first version.
