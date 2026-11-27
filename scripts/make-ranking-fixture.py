# SPDX-License-Identifier: AGPL-3.0-only
"""Drives recalld to make tests/fixtures/ranking/fixture.json.

Run through scripts/make-ranking-fixture.sh, with `python3 -I`. Standard
library only. Follows spec tab "Testing, CI and releases", "Making the
ranking fixture", steps 1 to 7; tests/fixtures/ranking/README.md describes
the output.

Every recalld process runs from --work with HOME, data dir, socket and logs
inside it, a minimal environment and an explicit --config, so nothing reaches
a recalld that is installed or running on the machine.
"""

import argparse
import base64
import datetime
import json
import os
import random
import re
import signal
import socket
import struct
import subprocess
import sys
import time
import urllib.error
import urllib.request

DAY = 86_400_000
TEN_MINUTES = 600_000
STUB_ADDR = ("127.0.0.1", 11435)  # `recalld bench embed-stub` default
DIM = 64
# recalld's EmbeddingConfig defaults (src/config/types.rs); not overridden.
DOCUMENT_PREFIX = "title: none | text: "
QUERY_PREFIX = "task: search result | query: "
EDGE_TYPES = {1: "parent_child", 2: "associative", 5: "entity", 6: "temporal", 7: "supersedes"}
LABEL_TAG_PREFIXES = ("entity/", "topic/", "emotion/")
SEED = 20261125

# ── Content ──────────────────────────────────────────────────────────────

THEMES = {
    "atlas": {
        "deploy": ["deploy", "rollout", "canary", "cluster", "release", "pipeline", "staging", "helm"],
        "storage": ["database", "replica", "backup", "snapshot", "index", "vacuum", "postgres", "migration"],
        "budget": ["budget", "invoice", "vendor", "forecast", "spend", "quarter", "contract", "license"],
        "hiring": ["interview", "candidate", "offer", "recruiter", "onboarding", "mentor", "team", "headcount"],
        "incident": ["outage", "pager", "latency", "alert", "rollback", "postmortem", "timeout", "dashboard"],
        "design": ["schema", "endpoint", "versioning", "review", "proposal", "gateway", "pagination", "auth"],
    },
    "harbor": {
        "garden": ["tomato", "basil", "compost", "seedling", "greenhouse", "watering", "mulch", "harvest"],
        "travel": ["flight", "hotel", "itinerary", "passport", "train", "luggage", "ferry", "visa"],
        "cooking": ["recipe", "sourdough", "oven", "starter", "flour", "bake", "risotto", "spice"],
        "health": ["running", "marathon", "knee", "physio", "stretching", "sleep", "cycling", "hydration"],
        "family": ["birthday", "gift", "party", "cousin", "dinner", "anniversary", "wedding", "photos"],
        "money": ["mortgage", "savings", "tax", "refund", "pension", "insurance", "receipt", "bank"],
    },
}
# Multi-word names, honorifics and possessives, so entity normalization matters.
ENTITIES = {
    "atlas": {
        "deploy": ["Maya Chen", "Northwind Labs", "Kubernetes"],
        "storage": ["Dr. Sarah Martinez", "Martinez", "Postgres"],
        "budget": ["Acme Robotics", "Mrs. Grace Okafor", "Okafor"],
        "hiring": ["Prof. Ivan Petrov", "Petrov's", "Lena Ortiz"],
        "incident": ["Captain Lena Ortiz", "Maya Chen's", "PagerDuty"],
        "design": ["Ivan Petrov", "Sarah Martinez", "API Guild"],
    },
    "harbor": {
        "garden": ["Aunt Rosa", "Rosa's", "Green Valley Nursery"],
        "travel": ["Lisbon", "Kenji Tanaka", "Mr. Tanaka"],
        "cooking": ["Chef Amara Diallo", "Diallo", "Tartine"],
        "health": ["Coach Daniel Reyes", "Reyes", "Elena"],
        "family": ["Elena's", "Uncle Bo", "Rosa"],
        "money": ["First Harbor Bank", "Mr. Kenji Tanaka", "Tanaka's"],
    },
}
# Names that often appear together, for queries with two entities.
ENTITY_PAIRS = {"atlas": ("Okafor", "Acme Robotics"), "harbor": ("Kenji Tanaka", "First Harbor Bank")}
# One name on every memory but the hub children: a query with it has more
# eligible memories than the vector channel fetches at limit 12 or less (100),
# so the entity channel finds the rest.
COMMON_ENTITY = {"atlas": "Atlas Crew", "harbor": "Harbor Club"}
VERBS = ["reviewed", "planned", "moved", "questioned", "approved", "postponed", "documented", "checked"]
LINKERS = ["before the", "after the", "during the", "alongside the", "instead of the"]
CODENAMES = ["zephyr", "obsidian", "marigold", "quartz", "juniper", "saffron"]
EXTRA_TAGS = ["kind/decision", "kind/note", "status:open", "prio-high", "kind/todo"]
EMOTIONS = ["worried", "relieved", "excited", "frustrated", "curious"]
COUNTS = {"atlas": 160, "harbor": 135}

# Superseded chains: [(summary, index of the member it supersedes or None)], labels.
# recalld refuses a second successor of one memory (503 STORAGE_UNAVAILABLE),
# so both chains are linear.
CHAINS = {
    "atlas": (
        [
            ("Backup retention policy keeps postgres snapshots for 7 days", None),
            ("Backup retention policy keeps postgres snapshots for 14 days", 0),
            ("Backup retention policy keeps postgres snapshots for 30 days", 1),
            ("Backup retention policy keeps postgres snapshots for 45 days", 2),
            ("Backup retention policy keeps postgres snapshots for 60 days", 3),
        ],
        {"tags": ["kind/policy"], "entities": ["Dr. Sarah Martinez"], "topics": ["storage"]},
    ),
    "harbor": (
        [
            ("Sourdough starter feeding schedule is twice a day with rye flour", None),
            ("Sourdough starter feeding schedule is once a day with rye flour", 0),
            ("Sourdough starter feeding schedule is every morning with spelt flour", 1),
            ("Sourdough starter feeding schedule is every evening with spelt flour", 2),
            ("Sourdough starter feeding schedule is every morning with wholemeal flour", 3),
        ],
        {"tags": ["kind/policy"], "entities": ["Chef Amara Diallo"], "topics": ["cooking"]},
    ),
}
CHAIN_SLOTS = [20, 50, 80, 110, 130]  # where chain members go among the plain memories
# Hubs: memories in words of their own, each the parent of the first 12 memories
# of one theme. A query on that theme fires the children, which spread into the
# hub, so the graph channel finds a memory the vector and text channels miss.
HUBS = {
    "atlas": [
        ("Bike rack locker keys", "incident"),
        ("Office plants watering rota", "design"),
    ],
    "harbor": [
        ("Neighborhood association minutes about parking signage streetlights", "health"),
        ("Book club reading list of novels poetry memoirs", "money"),
    ],
}
HUB_SLOTS = [3, 4]
HUB_CHILDREN = 12


def make_memories():
    """The memories to store, in store order. Deterministic."""
    rng = random.Random(SEED)
    per_ns = {}
    for ns, count in COUNTS.items():
        names = list(THEMES[ns])
        hub_of = {theme: k for k, (_, theme) in enumerate(HUBS[ns])}
        children = [0] * len(HUBS[ns])
        items = []
        for i in range(count):
            theme = names[(i * 5 + i // 6) % len(names)]
            other = names[(names.index(theme) + 1 + i % (len(names) - 1)) % len(names)]
            words = rng.sample(THEMES[ns][theme], 4)
            if i % 3 == 1:
                words[3] = rng.choice(THEMES[ns][other])
            pool = ENTITIES[ns][theme]
            entities = []
            if i % 5 != 4:
                entities.append(pool[i % len(pool)])
            if i % 4 == 0:
                extra = ENTITIES[ns][other][i % 3]
                if extra not in entities:
                    entities.append(extra)
            pair = ENTITY_PAIRS[ns]
            if pair[0] in entities and pair[1] not in entities and i % 2 == 0:
                entities.append(pair[1])
            entities.append(COMMON_ENTITY[ns])
            # Hub children have no names: no entity links, so a low degree and
            # a fan that lets them spread.
            k = hub_of.get(theme)
            hub_child = i >= HUB_SLOTS[-1] and k is not None and children[k] < HUB_CHILDREN
            if hub_child:
                children[k] += 1
                entities = []
            named = [e for e in entities if e != COMMON_ENTITY[ns]]
            subject = named[0] if named else "The team"
            summary = (
                f"{subject} {VERBS[i % len(VERBS)]} the {words[0]} {words[1]} "
                f"{LINKERS[i % len(LINKERS)]} {words[2]} {words[3]}"
            )
            m = {"namespace": ns, "summary": summary, "tags": [f"area/{theme}"]}
            if i % 2 == 0:
                m["tags"].append(EXTRA_TAGS[i % len(EXTRA_TAGS)])
            if i % 3 == 0:
                more = rng.sample(THEMES[ns][other], 3)
                code = CODENAMES[(i // 3) % len(CODENAMES)]
                m["fullText"] = (
                    f"Details: the {more[0]} and {more[1]} work is tracked as project {code}. "
                    f"Follow up on the {more[2]} with {subject} next week."
                )
            if entities:
                m["entities"] = entities
            m["topics"] = [theme] if i % 4 else [theme, other]
            if i % 7 == 3:
                m["emotions"] = [EMOTIONS[i % len(EMOTIONS)]]
            if hub_child:
                m["parent"] = (ns, "hub", k)
            elif i >= 10 and i % 8 == 5:
                m["parent"] = (ns, i - 5)
            items.append(m)
        chain, labels = CHAINS[ns]
        for slot, (summary, sup) in reversed(list(zip(CHAIN_SLOTS, chain))):
            m = {"namespace": ns, "summary": summary, **json.loads(json.dumps(labels))}
            m["key"] = (ns, "chain", chain.index((summary, sup)))
            if sup is not None:
                m["supersedes"] = (ns, "chain", sup)
            items.insert(slot, m)
        for slot, (k, (summary, _)) in reversed(list(zip(HUB_SLOTS, enumerate(HUBS[ns])))):
            items.insert(slot, {"namespace": ns, "summary": summary, "tags": ["area/events"],
                                "topics": ["events"], "key": (ns, "hub", k)})
        plain = 0
        for m in items:
            if "key" not in m:
                m["key"] = (ns, plain)
                plain += 1
        per_ns[ns] = items
    # Interleave the namespaces so creation times mix.
    order = []
    longest = max(len(v) for v in per_ns.values())
    for i in range(longest):
        for ns in COUNTS:
            if i < len(per_ns[ns]):
                order.append(per_ns[ns][i])
    return order


# Queries. No query word is a stop word that every summary has ("the"), so
# the full-text channel stays selective. Graph expansion can only add memories
# that the vector channel did not fetch, so queries with `depth` use a limit of
# 12 or less (fetch_k = 100, below either namespace's size).
# `time` is resolved against the stored creation times:
#   window: one hour either side of all creation times
#   recent: from two days before the first memory to the last one
#   past:   three to one days before the first memory
#   future: one to three days after the last memory
#   narrow: the first minute after the first memory (sigma floor of one day)
QUERIES = [
    {"namespace": "atlas", "query": "canary rollout cluster release", "limit": 25, "depth": 0},
    {"namespace": "atlas", "query": "postgres backup snapshot retention", "limit": 25, "depth": 0},
    {"namespace": "atlas", "query": "backup retention days", "limit": 10, "depth": 1},
    {"namespace": "atlas", "query": "vendor invoice forecast", "limit": 10, "depth": 0},
    {"namespace": "atlas", "query": "interview candidate onboarding", "limit": 25, "depth": 0, "entities": ["Prof. Ivan Petrov"]},
    {"namespace": "atlas", "query": "latency alert rollback", "limit": 10, "depth": 2},
    {"namespace": "atlas", "query": "schema versioning proposal", "limit": 12, "depth": 3},
    {"namespace": "atlas", "query": "zephyr", "limit": 25, "depth": 0},
    {"namespace": "atlas", "query": "obsidian migration", "limit": 10, "depth": 1},
    {"namespace": "atlas", "query": "outage postmortem", "limit": 25, "depth": 1, "entities": ["Captain Lena Ortiz"]},
    {"namespace": "atlas", "query": "replica vacuum index", "limit": 25, "depth": 0, "time": "window"},
    {"namespace": "atlas", "query": "budget quarter spend", "limit": 25, "depth": 0, "time": "recent"},
    {"namespace": "atlas", "query": "Maya Chen deploy pipeline", "limit": 5, "depth": 0, "entities": ["Maya Chen"]},
    {"namespace": "atlas", "query": "Martinez database migration", "limit": 25, "depth": 2, "entities": ["martinez"]},
    {"namespace": "atlas", "query": "headcount mentor recruiter", "limit": 50, "depth": 0},
    {"namespace": "atlas", "query": "gateway auth pagination", "limit": 100, "depth": 2},
    {"namespace": "atlas", "query": "Okafor license contract", "limit": 25, "depth": 0, "entities": ["Okafor", "Acme Robotics"]},
    {"namespace": "atlas", "query": "helm staging release", "limit": 10, "depth": 1, "time": "future"},
    {"namespace": "atlas", "query": "dashboard timeout pager", "limit": 10, "depth": 3, "time": "past"},
    {"namespace": "atlas", "query": "invoice vendor contract", "limit": 5, "depth": 0, "entities": ["Atlas Crew"]},
    {"namespace": "harbor", "query": "tomato seedling greenhouse", "limit": 25, "depth": 0},
    {"namespace": "harbor", "query": "sourdough starter feeding schedule", "limit": 25, "depth": 0},
    {"namespace": "harbor", "query": "starter flour schedule", "limit": 10, "depth": 2},
    {"namespace": "harbor", "query": "flight hotel itinerary", "limit": 10, "depth": 0},
    {"namespace": "harbor", "query": "passport visa Lisbon", "limit": 25, "depth": 1, "entities": ["LISBON"]},
    {"namespace": "harbor", "query": "marathon knee physio", "limit": 10, "depth": 3},
    {"namespace": "harbor", "query": "marigold", "limit": 25, "depth": 0},
    {"namespace": "harbor", "query": "quartz juniper", "limit": 12, "depth": 2},
    {"namespace": "harbor", "query": "birthday party cousin", "limit": 25, "depth": 0, "entities": ["Uncle Bo"]},
    {"namespace": "harbor", "query": "mortgage tax refund", "limit": 25, "depth": 0, "time": "recent"},
    {"namespace": "harbor", "query": "compost mulch watering", "limit": 25, "depth": 1, "entities": ["Rosa's"]},
    {"namespace": "harbor", "query": "Tanaka pension savings", "limit": 5, "depth": 0, "entities": ["Kenji Tanaka", "First Harbor Bank"]},
    {"namespace": "harbor", "query": "risotto spice bake", "limit": 50, "depth": 1, "time": "window"},
    {"namespace": "harbor", "query": "Reyes cycling hydration", "limit": 25, "depth": 2, "entities": ["Coach Daniel Reyes"]},
    {"namespace": "harbor", "query": "wedding anniversary photos", "limit": 100, "depth": 0},
    {"namespace": "harbor", "query": "ferry train luggage", "limit": 25, "depth": 0, "time": "narrow"},
    {"namespace": "harbor", "query": "insurance receipt bank", "limit": 10, "depth": 3, "time": "future"},
    {"namespace": "harbor", "query": "harvest dinner recipe", "limit": 10, "depth": 1, "time": "past"},
    {"namespace": "harbor", "query": "volcano saxophone", "limit": 25, "depth": 0},
    {"namespace": "harbor", "query": "gift party photos", "limit": 5, "depth": 1, "entities": ["Harbor Club"]},
]


def document_v1(m):
    """recalld's NativeInsert::document_v1 (src/storage/native_records.rs)."""
    tags = [t.lower() for t in m.get("tags", [])]
    for key, prefix in (("entities", "entity/"), ("topics", "topic/"), ("emotions", "emotion/")):
        for value in m.get(key, []):
            tag = prefix + value.lower()
            if tag not in tags:
                tags.append(tag)
    text = m["summary"]
    if m.get("fullText") is not None:
        text += "\n\n" + m["fullText"]
    if tags:
        text += " " + " ".join(tags)
    return text


# ── Helpers ──────────────────────────────────────────────────────────────


def now_ms():
    return time.time_ns() // 1_000_000


def rfc3339(ms):
    dt = datetime.datetime.fromtimestamp(ms // 1000, tz=datetime.timezone.utc)
    return dt.strftime("%Y-%m-%dT%H:%M:%S.") + f"{ms % 1000:03d}Z"


def f32(x):
    """Shortest decimal that reads back as the same f32."""
    bits = struct.pack("<f", x)
    for p in range(1, 10):
        s = f"{x:.{p}g}"
        if struct.pack("<f", float(s)) == bits:
            return float(s)
    return x


def die(msg):
    print(f"make-ranking-fixture: {msg}", file=sys.stderr)
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


def stub_embed(texts):
    url = f"http://{STUB_ADDR[0]}:{STUB_ADDR[1]}/api/embed"
    out = []
    for i in range(0, len(texts), 32):
        r = ok("POST", url, {"model": "stub", "input": texts[i:i + 32]})
        # The stub sends f32 values widened to f64; keep them as f32.
        out.extend([f32(x) for x in v] for v in r["embeddings"])
    if len(out) != len(texts) or any(len(v) != DIM for v in out):
        die("stub returned the wrong number or size of vectors")
    return out


def read_memory(api, mid, ns):
    r = ok("GET", f"{api}/memories/{mid}?recordAccess=false&namespace={ns}")
    return r["data"]


def lifecycle_view(d):
    return {k: d.get(k) for k in ("phase", "stability", "difficulty", "createdAt", "lastAccessedAt",
                                  "isPermastore", "summary", "tags", "labels")}


# ── Main ─────────────────────────────────────────────────────────────────


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--recalld", required=True)
    ap.add_argument("--commit", required=True)
    ap.add_argument("--work", required=True)
    ap.add_argument("--out", required=True)
    a = ap.parse_args()

    work = a.work
    for d in ("home", "data", "run", "logs", "tmp"):
        os.makedirs(os.path.join(work, d), exist_ok=True)
    if port_in_use(STUB_ADDR[1]):
        die(f"port {STUB_ADDR[1]} (the embedding stub's default) is in use")
    api_port, ui_port = free_port(), free_port()
    api = f"http://127.0.0.1:{api_port}"
    config = os.path.join(work, "recalld.toml")
    with open(config, "w") as f:
        f.write(
            "# Written by scripts/make-ranking-fixture.py. Everything else is recalld's default.\n"
            f"[server]\nbind_address = \"127.0.0.1\"\nport = {api_port}\n\n"
            f"[storage]\ndata_dir = \"{os.path.join(work, 'data')}\"\n\n"
            # Link caps only: recalld's defaults give every memory saved within
            # one hour 20 temporal links and up to 25 more, so every degree is
            # above 6, fan is 0 and neither spreading activation nor the
            # connection bonus does anything. Nothing here is read by ranking.
            "[graph]\nauto_link_threshold = 0.95\nmax_auto_links = 1\nmax_entity_links = 1\n"
            "max_temporal_links = 1\n\n"
            "[log]\nlevel = \"info\"\n"
        )
    env = {
        "HOME": os.path.join(work, "home"),
        "TMPDIR": os.path.join(work, "tmp"),
        "PATH": "/usr/bin:/bin",
        "RECALLD_CONFIG": config,
        # Relative: an absolute path here can exceed the 104-byte limit of a
        # Unix socket path on macOS. Every process runs with cwd = work.
        "RECALLD_DAEMON_SOCKET": "run/recalld.sock",
        "RECALLD_EMBEDDING_PROVIDER": "ollama",
        "RECALLD_EMBEDDING_BASE_URL": f"http://{STUB_ADDR[0]}:{STUB_ADDR[1]}",
        "RECALLD_EMBEDDING_DIMENSIONS": str(DIM),
    }
    procs = Procs(work, env)
    try:
        run(a, work, api, api_port, ui_port, config, procs)
    finally:
        procs.stop_all()


def run(a, work, api, api_port, ui_port, config, procs):
    bin_ = a.recalld
    procs.start("embed-stub", [bin_, "--config", config, "bench", "embed-stub", "--dim", str(DIM)])
    wait_until(lambda: port_in_use(STUB_ADDR[1]), "the embedding stub")

    # Steps 1 to 3: an empty data dir, then namespaces and memories in a fixed order.
    serve = procs.start("serve", [bin_, "--config", config, "serve", "--bind", f"127.0.0.1:{api_port}"])
    wait_until(lambda: http("GET", f"{api}/health")[0] == 200, "recalld serve")
    namespaces = []
    for ns in COUNTS:
        d = ok("POST", f"{api}/namespaces", {"name": ns})["data"]
        namespaces.append({k: v for k, v in d.items() if k not in ("id", "createdAt", "memoryCount")})

    specs = make_memories()
    ids = {}  # memory key -> id
    stored = []
    for m in specs:
        body = {k: m[k] for k in ("namespace", "summary", "fullText", "tags", "entities", "topics", "emotions")
                if k in m}
        if "parent" in m:
            body["parentId"] = ids[m["parent"]]
        if "supersedes" in m:
            body["supersedes"] = ids[m["supersedes"]]
        d = ok("POST", f"{api}/memories", body)["data"]
        ids[m["key"]] = d["id"]
        stored.append((body, d))
    print(f"stored {len(stored)} memories", file=sys.stderr)

    # Step 4: vectors from the stub with the strings recalld builds.
    documents = [DOCUMENT_PREFIX + document_v1(body) for body, _ in stored]
    doc_vectors = stub_embed(documents)
    vec_by_id = {d["id"]: v for (_, d), v in zip(stored, doc_vectors)}
    query_vectors = stub_embed([QUERY_PREFIX + q["query"] for q in QUERIES])

    # Proof: recalld's own stored vectors (includeEmbeddings) equal the stub's.
    proof = []
    for ns in COUNTS:
        probe = next(b for b, _ in stored if b["namespace"] == ns)
        r = ok("POST", f"{api}/search", {"query": probe["summary"], "namespace": ns, "limit": 100,
                                         "recordAccess": False, "includeEmbeddings": True})
        for hit in r["data"]["hits"] if isinstance(r.get("data"), dict) else r["hits"]:
            # recalld normalizes the stub's vector again, which can move a last bit.
            got, want = hit["memory"]["embedding"], vec_by_id[hit["memory"]["id"]]
            diff = max(abs(x - y) for x, y in zip(got, want))
            if len(got) != DIM or diff > 1e-6:
                die(f"vector of {hit['memory']['id']} differs from recalld's by {diff}")
            proof.append(diff)
    print(f"proof: {len(proof)} stored vectors equal the stub's, largest difference {max(proof)}", file=sys.stderr)

    # Step 5: state before any query.
    before = {}
    for body, d in stored:
        cur = read_memory(api, d["id"], body["namespace"])
        if cur["lastAccessedAt"] != cur["createdAt"]:
            die(f"{d['id']}: lastAccessedAt {cur['lastAccessedAt']} != createdAt {cur['createdAt']}")
        before[d["id"]] = cur

    created = [before[d["id"]]["createdAt"] for _, d in stored]
    t_first, t_last = min(created), max(created)
    ranges = {
        "window": (t_first - 3_600_000, t_last + 3_600_000),
        "recent": (t_first - 2 * DAY, t_last),
        "past": (t_first - 3 * DAY, t_first - DAY),
        "future": (t_last + DAY, t_last + 3 * DAY),
        "narrow": (t_first, t_first + 60_000),
    }

    # Step 6: the queries. Each is replayed once with the recorded vector as
    # `embedding`, which recalld then uses instead of embedding the query
    # itself: equal IDs and scores prove the query vector.
    queries, graph_changed = [], []
    for n, (q, vec) in enumerate(zip(QUERIES, query_vectors), 1):
        req = {"query": q["query"], "namespace": q["namespace"], "limit": q["limit"], "depth": q["depth"]}
        if "entities" in q:
            req["entities"] = q["entities"]
        if "time" in q:
            req["timeRangeStart"], req["timeRangeEnd"] = ranges[q["time"]]
        req["recordAccess"] = False
        at = now_ms()
        if n == 1 and at - t_first >= TEN_MINUTES:
            die(f"first query is {at - t_first} ms after the first save; the limit is {TEN_MINUTES}")
        r = ok("POST", f"{api}/search", req)
        hits = r["data"]["hits"] if isinstance(r.get("data"), dict) else r["hits"]
        replay = ok("POST", f"{api}/search", {**req, "embedding": vec})
        replay_hits = replay["data"]["hits"] if isinstance(replay.get("data"), dict) else replay["hits"]
        if [(h["memory"]["id"], h["score"]) for h in hits] != [(h["memory"]["id"], h["score"]) for h in replay_hits]:
            die(f"query {n}: the stub's query vector does not reproduce recalld's result")
        if req["depth"] > 0:  # coverage note only: does the graph change this result?
            flat = ok("POST", f"{api}/search", {**req, "depth": 0})
            flat_hits = flat["data"]["hits"] if isinstance(flat.get("data"), dict) else flat["hits"]
            if [h["memory"]["id"] for h in flat_hits] != [h["memory"]["id"] for h in hits]:
                graph_changed.append(n)
        queries.append({
            "name": f"q{n:02d}",
            "request": req,
            "atMs": at,
            "at": rfc3339(at),
            "vector": vec,
            "expectedIds": [h["memory"]["id"] for h in hits],
        })
    print(f"ran {len(queries)} queries; depth changed the result of {graph_changed}", file=sys.stderr)

    # recordAccess:false must have left every memory as it was.
    for body, d in stored:
        after = read_memory(api, d["id"], body["namespace"])
        if lifecycle_view(after) != lifecycle_view(before[d["id"]]):
            die(f"{d['id']} changed during the queries")

    procs.stop(serve)

    # Step 5, rest: edges from `recalld ui`'s graph route, access counts from its
    # memory route (recalld's REST memory route has no access count). Both need
    # the daemon, which cannot share the data dir with `serve`.
    procs.start("daemon", [bin_, "--config", config, "daemon", "start", "--foreground",
                                    "--idle-timeout", "0"])
    wait_until(lambda: os.path.exists(os.path.join(work, "run", "recalld.sock")), "the daemon socket")
    procs.start("ui", [bin_, "--config", config, "ui", "--port", str(ui_port), "--no-open"])
    ui_log = os.path.join(work, "logs", "ui.log")
    token = []

    def ui_ready():
        m = re.search(r"[?&]t=([0-9a-fA-F]+)", open(ui_log).read())
        if m:
            token[:] = [m.group(1)]
        return bool(m) and port_in_use(ui_port)

    wait_until(ui_ready, "recalld ui")
    ui = f"http://127.0.0.1:{ui_port}/api/ui"
    cookie = {"Cookie": f"recalld_ui_{ui_port}={token[0]}"}

    offset, raw_edges, nodes = 0, [], None
    while True:
        g = ok("GET", f"{ui}/graph?edgeOffset={offset}", headers=cookie)
        if g["nodes"] is not None:
            nodes = g["nodes"]
        src = struct.unpack(f"<{len(base64.b64decode(g['edges']['src'])) // 4}I", base64.b64decode(g["edges"]["src"]))
        dst = struct.unpack(f"<{len(src)}I", base64.b64decode(g["edges"]["dst"]))
        kind = base64.b64decode(g["edges"]["type"])
        weight = struct.unpack(f"<{len(src)}f", base64.b64decode(g["edges"]["weight"]))
        raw_edges.extend(zip(src, dst, kind, weight))
        if g["nextEdgeOffset"] is None:
            break
        offset = g["nextEdgeOffset"]
    node_ids = nodes["ids"]
    if sorted(node_ids) != sorted(before):
        die("the graph's nodes are not the stored memories")
    node_created = struct.unpack(f"<{len(node_ids)}d", base64.b64decode(nodes["createdAt"]))
    node_access = struct.unpack(f"<{len(node_ids)}d", base64.b64decode(nodes["lastAccess"]))
    for mid, c, la in zip(node_ids, node_created, node_access):
        if int(c) != before[mid]["createdAt"] or int(la) != before[mid]["lastAccessedAt"]:
            die(f"{mid}: daemon's times differ from serve's")
    edges = []
    for s, t, k, w in raw_edges:
        if k not in EDGE_TYPES:
            die(f"unknown edge type {k}")
        edges.append({"source": node_ids[s], "target": node_ids[t], "type": EDGE_TYPES[k],
                      "typeNumber": k, "weight": f32(w)})
    edges.sort(key=lambda e: (e["source"], e["typeNumber"], e["target"]))

    access = {}
    for _, d in stored:
        # `total` is the length of the record's access history, which is what
        # recalld's get_memory reports as accessCount (src/search/get_wire.rs).
        v = ok("GET", f"{ui}/memories/{d['id']}/history", headers=cookie)
        access[d["id"]] = v["total"]
    procs.stop_all()

    # Step 7.
    memories = []
    for (body, d), vec, doc in zip(stored, doc_vectors, documents):
        cur = before[d["id"]]
        labels = cur.get("labels") or {}
        memories.append({
            "id": d["id"],
            "namespace": body["namespace"],
            "createdAtMs": cur["createdAt"],
            "createdAt": rfc3339(cur["createdAt"]),
            "summary": cur["summary"],
            "fullText": cur.get("fullText"),
            "tags": [t for t in cur["tags"] if not t.startswith(LABEL_TAG_PREFIXES)],
            "entities": labels.get("entities", cur.get("entities", [])),
            "topics": labels.get("topics", cur.get("topics", [])),
            "emotions": labels.get("emotions", cur.get("emotions", [])),
            "parentId": body.get("parentId"),
            "supersedes": body.get("supersedes"),
            "phase": cur["phase"],
            "stability": cur["stability"],
            "difficulty": cur["difficulty"],
            "isPermastore": cur["isPermastore"],
            "accessCount": access[d["id"]],
            "decayAnchorAtMs": cur["lastAccessedAt"],
            "decayAnchorAt": rfc3339(cur["lastAccessedAt"]),
            "document": doc,
            "vector": vec,
        })
    for m, (body, _) in zip(memories, stored):
        for k in ("summary", "fullText"):
            if m[k] != body.get(k):
                die(f"{m['id']}: {k} read back differs from what was sent")
        for k in ("entities", "topics", "emotions"):
            if m[k] != body.get(k, []):
                die(f"{m['id']}: {k} read back {m[k]} differs from what was sent {body.get(k, [])}")

    fixture = {
        "format": 1,
        "source": {
            "recalldCommit": a.commit,
            "build": "cargo build --release --features bench",
            "script": "scripts/make-ranking-fixture.sh",
            "embedding": {"stub": "recalld bench embed-stub --dim 64", "dimensions": DIM,
                          "documentPrefix": DOCUMENT_PREFIX, "queryPrefix": QUERY_PREFIX},
        },
        "namespaces": namespaces,
        "memories": memories,
        "edges": edges,
        "queries": queries,
    }
    write_fixture(a.out, fixture)
    counts = {}
    for e in edges:
        counts[e["type"]] = counts.get(e["type"], 0) + 1
    print(f"wrote {a.out}: {len(memories)} memories, edges {counts}, {len(queries)} queries", file=sys.stderr)


def write_fixture(path, fixture):
    """Pretty JSON, one line per vector."""
    vectors = []

    def stash(v):
        vectors.append(v)
        return f"@@VECTOR{len(vectors) - 1}@@"

    for m in fixture["memories"]:
        m["vector"] = stash(m["vector"])
    for q in fixture["queries"]:
        q["vector"] = stash(q["vector"])
    text = json.dumps(fixture, indent=2, ensure_ascii=False)
    for i, v in enumerate(vectors):
        text = text.replace(f'"@@VECTOR{i}@@"', json.dumps(v), 1)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text + "\n")


if __name__ == "__main__":
    main()
