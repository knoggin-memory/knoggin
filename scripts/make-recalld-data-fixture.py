# SPDX-License-Identifier: AGPL-3.0-only
"""Drives recalld to make the data directory of tests/fixtures/recalld-data/.

Run through scripts/make-recalld-data-fixture.sh, with `python3 -I`. Standard
library only. Follows spec tab "Config, CLI, local install", "Import from
recalld", "Test fixture".

Every recalld process runs from --work with HOME, data dir, socket and logs
inside it, a minimal environment and an explicit --config, so nothing reaches
a recalld that is installed or running on the machine. Only the processes this
script started are signalled.

Writes <work>/data (the recalld data directory, recalld stopped) and
<work>/recalld-stats.json (recalld's own namespace statistics, read while it
ran), then exits.
"""

import argparse
import json
import os
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

DAY = 86_400_000
STUB_ADDR = ("127.0.0.1", 11435)  # `recalld bench embed-stub` default
DIM = 16
POLICY_HEADERS = {"Recalld-Contract-Version": "2"}


def die(msg):
    print(f"make-recalld-data-fixture: {msg}", file=sys.stderr)
    sys.exit(1)


def port_in_use(port):
    with socket.socket() as s:
        return s.connect_ex(("127.0.0.1", port)) == 0


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def http(method, url, body=None, headers=None, timeout=60):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method=method, headers=headers or {})
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, json.loads(r.read() or b"null")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")


def ok(method, url, body=None, headers=None, expect=(200, 201)):
    status, value = http(method, url, body, headers)
    if status not in expect:
        die(f"{method} {url} -> {status}: {value}")
    return value


def data(v):
    return v["data"] if isinstance(v, dict) and "data" in v else v


class Procs:
    """Processes this script started, by PID. Stops only those."""

    def __init__(self, work, env):
        self.work, self.env, self.started = work, env, []

    def start(self, name, args):
        log = open(os.path.join(self.work, "logs", f"{name}.log"), "wb")
        p = subprocess.Popen(args, cwd=self.work, env=self.env, stdout=log, stderr=subprocess.STDOUT,
                             stdin=subprocess.DEVNULL)
        print(f"started {name} pid {p.pid}", file=sys.stderr)
        self.started.append((name, p))
        return p

    def stop(self, p):
        # `recalld serve` stops its API on SIGINT and exits on SIGTERM.
        for sig, wait in ((signal.SIGINT, 3), (signal.SIGTERM, 15), (signal.SIGKILL, 5)):
            if p.poll() is not None:
                break
            p.send_signal(sig)
            try:
                p.wait(wait)
            except subprocess.TimeoutExpired:
                pass
        self.started = [(n, q) for n, q in self.started if q is not p]

    def stop_all(self):
        for _, p in list(reversed(self.started)):
            self.stop(p)


def wait_until(check, what, seconds=30):
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            if check():
                return
        except OSError:
            pass
        time.sleep(0.2)
    die(f"timed out waiting for {what}")


def long_text():
    """About 12 KiB of text that is not the same line repeated."""
    words = ["harbor", "ledger", "copper", "violet", "meadow", "signal", "lantern", "orchard"]
    lines = [f"Line {i}: the {words[i % 8]} and the {words[(i * 3) % 8]} were noted on day {i}."
             for i in range(200)]
    return "\n".join(lines) + "\nUnicode survives: café, naïve, 日本語, emoji 🦀."


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--recalld", required=True)
    ap.add_argument("--work", required=True)
    a = ap.parse_args()

    work = a.work
    for d in ("home", "data", "run", "logs", "tmp"):
        os.makedirs(os.path.join(work, d), exist_ok=True)
    if port_in_use(STUB_ADDR[1]):
        die(f"port {STUB_ADDR[1]} (the embedding stub's default) is in use")
    api_port = free_port()
    api = f"http://127.0.0.1:{api_port}"
    config = os.path.join(work, "recalld.toml")
    with open(config, "w") as f:
        f.write(
            "# Written by scripts/make-recalld-data-fixture.py. Everything else is recalld's default.\n"
            f"[server]\nbind_address = \"127.0.0.1\"\nport = {api_port}\n\n"
            f"[storage]\ndata_dir = \"{os.path.join(work, 'data')}\"\n\n"
            # Few links, so the fixture stays small and readable; a low
            # threshold, so the stub's vectors still give associative links.
            "[graph]\nauto_link_threshold = 0.2\nmax_auto_links = 2\nmax_entity_links = 2\n"
            "max_temporal_links = 1\n\n"
            "[log]\nlevel = \"info\"\n"
        )
    env = {
        "HOME": os.path.join(work, "home"),
        "TMPDIR": os.path.join(work, "tmp"),
        "PATH": "/usr/bin:/bin",
        "RECALLD_CONFIG": config,
        # Relative: an absolute path can exceed the 104-byte limit of a Unix
        # socket path on macOS. Every process runs with cwd = work.
        "RECALLD_DAEMON_SOCKET": "run/recalld.sock",
        "RECALLD_EMBEDDING_PROVIDER": "ollama",
        "RECALLD_EMBEDDING_BASE_URL": f"http://{STUB_ADDR[0]}:{STUB_ADDR[1]}",
        "RECALLD_EMBEDDING_DIMENSIONS": str(DIM),
    }
    procs = Procs(work, env)
    try:
        run(a.recalld, work, api, config, procs)
    finally:
        procs.stop_all()


def run(bin_, work, api, config, procs):
    procs.start("embed-stub", [bin_, "--config", config, "bench", "embed-stub", "--dim", str(DIM)])
    wait_until(lambda: port_in_use(STUB_ADDR[1]), "the embedding stub")

    def serve(data_dir):
        p = procs.start("serve", [bin_, "--config", config, "--data-dir", data_dir, "serve",
                                  "--bind", api.removeprefix("http://")])
        wait_until(lambda: http("GET", f"{api}/health")[0] == 200, "recalld serve")
        return p

    first = serve(os.path.join(work, "data"))

    # Namespaces: `work` with a policy of its own, `personal` with recalld's
    # defaults, `default` as recalld made it. recalld refuses two names that
    # differ only in case, so the reader's renaming is covered by unit tests.
    ok("POST", f"{api}/namespaces", {"name": "work", "initialStability": 10.0,
                                     "desiredRetention": 0.85, "decayRateMultiplier": 0.5})
    ok("POST", f"{api}/namespaces", {"name": "personal"})
    namespaces = {n["name"]: n for n in data(ok("GET", f"{api}/namespaces"))}
    ok("PUT", f"{api}/namespaces/{namespaces['work']['id']}/policy", {"policy": {
        "initialStability": 10.0, "defaultDifficulty": 4.5, "permastoreThreshold": 900.0,
        "desiredRetention": 0.85, "decayRateMultiplier": 0.5,
        "phaseThresholds": {"fullToSummary": 0.8, "summaryToGhost": 0.4, "ghostToDelete": 0.1}}},
       headers=POLICY_HEADERS)

    now = int(time.time() * 1000)

    def store(body):
        return data(ok("POST", f"{api}/memories", body))["id"]

    ids = {}
    # Saved first, so that it has no links of its own: recalld keeps a forgotten
    # memory's edges, and its statistics count only those of live memories.
    ids["forgotten"] = store({
        "summary": "A note that was forgotten on purpose", "tags": ["kind/note"]})
    ids["trip"] = store({
        "summary": "Maya Chen planned the Lisbon trip for March",
        "fullText": long_text(),
        "tags": ["kind/plan", "Travel", "topic/travel"],
        "entities": ["Maya Chen", "Lisbon"], "topics": ["travel"], "emotions": ["excited"]})
    ids["trip-child"] = store({
        "summary": "Maya Chen booked the window seats on the Lisbon flight",
        "parentId": ids["trip"], "entities": ["Maya Chen"], "tags": ["kind/booking"]})
    ids["policy-v1"] = store({
        "summary": "Backups are kept for 7 days", "tags": ["kind/policy"],
        "entities": ["Postgres"], "topics": ["storage"]})
    ids["policy-v2"] = store({
        "summary": "Backups are kept for 30 days", "tags": ["kind/policy"],
        "entities": ["Postgres"], "topics": ["storage"], "supersedes": ids["policy-v1"]})
    ids["plain-tag"] = store({
        "summary": "A tag that looks like a label but has no label beside it",
        "tags": ["entity/rosa", "kind/note"]})
    ids["permanent"] = store({
        "summary": "Mother's birthday is on the 4th of May", "initialStability": 2000.0,
        "topics": ["family"], "emotions": ["warm"]})
    ids["faded"] = store({
        "summary": "The old office wifi password was rotated", "fullText": "It was rotated in spring.",
        "tags": ["kind/note"], "entities": ["Office"], "createdAt": now - 30 * DAY})
    ids["ghost"] = store({
        "summary": "A conference badge from long ago", "fullText": "Badge number 42.",
        "tags": ["kind/souvenir"], "topics": ["events"], "createdAt": now - 200 * DAY})
    ids["work-a"] = store({
        "namespace": "work", "summary": "Atlas release 2.1 shipped to the canary cluster",
        "fullText": "Rollout began at 09:00 and finished at 11:30.",
        "tags": ["area/deploy"], "entities": ["Atlas"], "topics": ["deploy"]})
    ids["work-b"] = store({
        "namespace": "work", "summary": "Atlas release 2.0 retrospective notes",
        "tags": ["area/deploy"], "entities": ["Atlas"], "createdAt": now - 120 * DAY})
    ids["work-c"] = store({
        "namespace": "work", "summary": "Quarterly budget review with the vendor",
        "fullText": "The vendor contract renews in June.", "tags": ["area/budget"],
        "createdAt": now - 45 * DAY})
    ids["personal-a"] = store({
        "namespace": "personal", "summary": "Started reading a novel about lighthouses",
        "tags": ["kind/note"], "emotions": ["curious"]})

    # Access history: direct reads, a search and a similar search that record
    # access, reinforcements.
    ok("GET", f"{api}/memories/{ids['trip']}?recordAccess=true")
    ok("GET", f"{api}/memories/{ids['trip']}?recordAccess=true")
    ok("POST", f"{api}/search", {"query": "Lisbon trip window seats", "namespace": "default",
                                 "limit": 5, "depth": 1, "recordAccess": True})
    ok("POST", f"{api}/similar/{ids['policy-v2']}", {"limit": 2, "recordAccess": True})
    ok("POST", f"{api}/memories/{ids['trip-child']}/reinforce", {"quality": 4})
    ok("POST", f"{api}/memories/{ids['work-a']}/reinforce", {})

    ok("GET", f"{api}/memories/{ids['personal-a']}?recordAccess=true")
    ok("POST", f"{api}/memories/{ids['personal-a']}/reinforce", {"quality": 3})

    ok("DELETE", f"{api}/memories/{ids['forgotten']}")
    # One phase step per memory and sweep: three sweeps take the oldest to ghost.
    for _ in range(3):
        sweep = data(ok("POST", f"{api}/decay/sweep", {}))
        print(f"sweep: {json.dumps(sweep)}", file=sys.stderr)

    stats = {}
    for n in data(ok("GET", f"{api}/namespaces")):
        stats[n["name"]] = data(ok("GET", f"{api}/namespaces/{n['id']}/stats"))
    procs.stop(first)

    with open(os.path.join(work, "recalld-stats.json"), "w") as f:
        json.dump({"ids": ids, "namespaces": stats}, f, indent=2, sort_keys=True)
    print(json.dumps(stats, indent=1), file=sys.stderr)


if __name__ == "__main__":
    main()
