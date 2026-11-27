# Local install

A local install is one program, the daemon, on your own machine, for you alone. Everything it keeps is encrypted on disk. This guide covers its files, the master key and its backup, what the encryption protects, export and import, and what to do when Knoggin says your data is locked.

## The files

Everything lives under `~/.knoggin/`, or under `KNOGGIN_HOME` when that is set.

| Path | What it is |
| --- | --- |
| `bin/knoggin`, `lib/` | The program and its ONNX Runtime library |
| `config.toml` | Settings, if you made the file. It holds no secret |
| `data/key-source.json` | Which master-key source is in use |
| `data/state/` | The local state database: apps, API-key hashes. Encrypted |
| `data/stores/<store id>/` | Snapshots of each store. Encrypted |
| `models/` | The embedding model. Not secret |
| `logs/daemon.log` | The daemon's log. It never holds memory text |
| `run/` | The lock file and the Unix socket `daemon.sock` |

The daemon refuses to start if another account can write to the home folder, the `run` or `data` folders, or the key file's folder.

**How agents reach it.** Your own programs talk to the daemon over the Unix socket `run/daemon.sock`, which only your account can open. That is the credential: `knoggin mcp`, the command line and the Python SDK need no key. The daemon also listens on `127.0.0.1:7690` for agents that connect by address; there every request needs an API key (see [Apps and API keys](apps-and-keys.md)). The web UI runs on a second loopback port that changes on every start.

**Settings** go in `~/.knoggin/config.toml` or in environment variables (`KNOGGIN_` plus the key path, with `__` between levels, such as `KNOGGIN_SERVER__BIND`). `knoggin config check` prints the effective settings. The ones most people touch:

| Setting | Default | Meaning |
| --- | --- | --- |
| `server.bind` | `127.0.0.1:7690` | The fixed TCP port. It must be a loopback address |
| `local.master_key` | `auto` | `auto`, `keychain` or `key_file`. Read on the first run only |
| `local.master_key_file` | `~/.config/knoggin/master.key` (with `KNOGGIN_HOME`: `<home>/master.key`) | The key file, when that source is used |
| `retention.days` | 7 | Days of daily snapshots kept |
| `retention.soft_delete_days` | 7 | Days in which a deleted app or account can be undone |
| `log.level` | `info` | `error`, `warn`, `info`, `debug` or `trace` |

A `.knoggin.toml` file with one line, `namespace = "<name>"`, in a project folder or above it, makes `knoggin mcp` use that namespace by default for agents started there.

## The master key

Every snapshot is encrypted with a key of its own store, and those keys are wrapped by the master key. On the first run Knoggin makes the master key and keeps it:

- on macOS, in your login keychain;
- on Linux with a desktop session, in the Secret Service (`secret-tool`);
- otherwise, or with `local.master_key = "key_file"`, in a key file with mode 0600.

`knoggin status` shows which source is in use.

**Back it up.** Losing every copy of the master key loses your memories. With the daemon stopped:

```sh
knoggin stop
knoggin key export --out ~/knoggin-master-key.json
knoggin start
```

The file has mode 0600. Anyone with this file and your data folder can read your memories, so keep it apart from the data: in a password manager, or on a drive you keep offline.

**Restore it** on a new machine, or after the keychain item was lost: copy the data folder back, then run `knoggin key import <file>` with the daemon stopped.

**Rotate it** with `knoggin key rotate`, with the daemon stopped. It makes a new key and wraps every store key again. Rotation does not help against a key that has already leaked, because the store keys stay the same. After a leak, export each store, import it into a new app, and delete the old one.

## What local encryption covers

- **Key in the keychain:** a stolen disk, a copied backup, a synced folder.
- **Key in a key file on the same disk:** a copied or synced data folder, but not a stolen disk. That holds only while the key file is outside what you copy or sync. With `KNOGGIN_HOME` set, the default key file is inside that home, so sync only the `data` folder, or set `local.master_key_file` to a path outside the synced folder.
- **Neither** protects against software that runs as you: it can read the keychain while it is unlocked, or the key file. Use full-disk encryption as well.
- **Core dumps on Linux.** Knoggin turns off its own core dumps, but a Linux system that pipes core dumps to a program, as systemd-coredump does, can still keep a memory image of a crash, readable by root. To stop that, set the core pattern so that no core dump lands on disk:

  ```sh
  sudo sysctl -w kernel.core_pattern='|/bin/false'
  ```

  Put the same line in a file under `/etc/sysctl.d/` so that it survives a reboot. This turns off core dumps for every program on the machine.

A data folder that is replaced by an older copy is accepted as it is: API keys revoked since that copy work again.

## Export and import

An export file holds a whole store in plain JSON Lines: text, labels, decay state, namespaces and links. It is **not encrypted**, so treat it like the memories themselves.

```sh
knoggin export --out personal.jsonl              # your personal store
knoggin export --app research --out research.jsonl
knoggin import personal.jsonl                    # into your personal store
knoggin import research.jsonl --app research
```

The export file is created with mode 0600, and `export` refuses a path that already exists. `import` skips memories that are already there, so running it twice changes nothing; `--on-conflict error` reports them instead. Vectors are not exported: they are computed again after an import, and `knoggin reembed` waits until that is done. The same file moves a store between a local install and a server.

The web UI's Settings page downloads the same export.

## When your data is locked

The daemon never starts empty beside data it cannot read. Instead it starts **locked**: `knoggin status` exits with code 4, `knoggin open` prints why, and every agent's call answers `store_locked` with a reason. Nothing is written while locked. When you unlock the keychain or put a file back, the daemon notices within a minute; no restart is needed.

| Reason | What happened | What to do |
| --- | --- | --- |
| `master_key_missing` | The master key cannot be read: the keychain is locked, the key file is gone, or the data came from another machine | Unlock the keychain, or put the key file back. Or run `knoggin stop`, then `knoggin key import <backup>` |
| `state_unreadable` | The newest state file is damaged | Run `knoggin stop`, then `knoggin state use-previous`. It shows what the previous file holds and asks first. Keys revoked and apps made after that file may be undone |
| `state_missing` | Snapshots exist but no state file | Recover each store from its snapshots (below) into a fresh install |
| `snapshot_unreadable` (one store only) | A store's newest snapshot is missing or damaged | Recover that store from an older snapshot (below) |

**Recovering a store from its snapshots.** Each folder under `data/stores/` holds that store's snapshots, the newest with the highest name. You need only the master key:

```sh
knoggin admin snapshot inspect ~/.knoggin/data/stores/<store id>/<name>.kgs
knoggin admin snapshot export ~/.knoggin/data/stores/<store id>/<name>.kgs --out store.jsonl
knoggin import store.jsonl                       # into a fresh install, or --app <name>
```

`inspect` reads only the header and needs no key. `export` decrypts with your local master key. Try the newest snapshot first and older ones if it fails.

## Uninstall

`uvx knoggin init --uninstall` removes the agent entries it added and offers to remove the program. It never removes your data. To remove the data too, stop the daemon and delete `~/.knoggin/data`; to remove the key, delete the keychain item named `knoggin` or the key file.

## See also

- [Getting started](getting-started.md)
- [Operations](operations.md): health, logs and metrics
- [Security](security.md)
