# SPDX-License-Identifier: AGPL-3.0-only
"""Pydantic models of the REST shapes, with snake-case fields and camel-case aliases.

Class names are the schema names of `docs/openapi.json`; `tests/test_openapi.py` checks the fields.
Unknown fields in an answer are ignored, so a newer server does not break an older SDK.
"""

from __future__ import annotations

from datetime import datetime
from typing import Any, Literal

from pydantic import BaseModel, ConfigDict
from pydantic.alias_generators import to_camel

__all__ = [
    "AccessEvent",
    "BatchItemResult",
    "BatchResult",
    "CompactHit",
    "Context",
    "EmbeddingCounts",
    "EmbeddingProgress",
    "ForgetItem",
    "ForgetManyResult",
    "Forgotten",
    "FullHit",
    "ImportError",
    "ImportResult",
    "ItemError",
    "Memory",
    "ModelState",
    "Moved",
    "Namespace",
    "NamespaceCount",
    "NamespaceDeleted",
    "NamespacePolicy",
    "NamespaceStats",
    "NearDuplicate",
    "NearDuplicates",
    "NewMemory",
    "PhaseCounts",
    "PhaseThresholds",
    "RecallResult",
    "Reinforced",
    "Related",
    "SimilarHit",
    "StoredMemory",
    "TagCount",
    "WouldChange",
]

Phase = Literal["full", "summary", "ghost"]
EmbeddingState = Literal["ready", "pending", "failed", "none"]


class _Model(BaseModel):
    model_config = ConfigDict(alias_generator=to_camel, populate_by_name=True, frozen=True)


class AccessEvent(_Model):
    at: datetime
    kind: Literal["direct", "associative", "reinforcement"]


class Memory(_Model):
    """The full view of a memory. `summary` is None for a ghost; `full_text` is None when the route leaves it out."""

    id: str
    namespace: str
    created_at: datetime
    summary: str | None = None
    full_text: str | None = None
    tags: list[str]
    entities: list[str]
    topics: list[str]
    emotions: list[str]
    phase: Phase
    strength: float
    effective_strength: float
    stability: float
    is_permastore: bool
    retrieved_at: datetime | None = None
    reinforced_at: datetime | None = None
    access_count: int
    edge_count: int
    embedding: EmbeddingState
    saved_by: str
    client: str | None = None
    access_history: list[AccessEvent] | None = None


class NearDuplicate(_Model):
    id: str
    score: float
    summary: str


class NearDuplicates(_Model):
    threshold: float
    matches: list[NearDuplicate]


class StoredMemory(_Model):
    """The answer to a save or a supersede."""

    id: str
    namespace: str
    phase: Phase
    strength: float
    stability: float
    created_at: datetime
    embedding: EmbeddingState
    near_duplicates: NearDuplicates | None = None
    duplicate_check: Literal["skipped"] | None = None
    supersedes: str | None = None


class NewMemory(_Model):
    """One item of `memories.store_many` (schema `StoreMemoryItem`)."""

    summary: str
    full_text: str | None = None
    namespace: str | None = None
    tags: list[str] = []
    entities: list[str] = []
    topics: list[str] = []
    emotions: list[str] = []
    parent_id: str | None = None
    created_at: datetime | None = None


class ItemError(_Model):
    code: str
    message: str
    field: str | None = None


class BatchItemResult(_Model):
    index: int
    status: Literal["ok", "error"]
    memory: StoredMemory | None = None
    error: ItemError | None = None


class BatchResult(_Model):
    results: list[BatchItemResult]
    stored: int
    errors: int


class CompactHit(_Model):
    """A recall or context hit in the compact form: a short date, the summary, and the full text for the top ones."""

    id: str
    date: str
    summary: str
    full_text: str | None = None


class Related(_Model):
    id: str
    edge_type: Literal["parent_child", "associative", "entity", "temporal", "supersedes"]
    weight: float


class FullHit(Memory):
    """A recall hit in the full form: the memory, its score and its edges to other hits."""

    score: float
    related: list[Related]
    explanation: dict[str, Any] | None = None


class RecallResult(_Model):
    """The answer of a recall (schemas `CompactRecall` and `FullRecall`): `memories` holds the form `detail` asked for."""

    memories: list[CompactHit] | list[FullHit]
    count: int
    degraded: Literal["model_unavailable"] | None = None


class ModelState(_Model):
    state: Literal["ready", "loading", "downloading", "unavailable"]
    identity: str
    name: str
    bytes_done: int | None = None
    bytes_total: int | None = None
    reason: str | None = None


class EmbeddingProgress(_Model):
    pending: int
    total: int


class NamespaceCount(_Model):
    name: str
    memory_count: int


class Context(_Model):
    """The answer of `GET /v1/context`, the one call an agent makes at the start of a session."""

    default_namespace: str
    namespaces: list[NamespaceCount]
    memories: list[CompactHit]
    model: ModelState
    embedding: EmbeddingProgress
    web_url: str | None = None
    degraded: Literal["model_unavailable"] | None = None


class Reinforced(_Model):
    id: str
    strength: float
    stability: float
    phase: Phase
    is_permastore: bool


class Forgotten(_Model):
    id: str
    deleted: bool


class ForgetItem(_Model):
    index: int
    id: str
    deleted: bool


class ForgetManyResult(_Model):
    results: list[ForgetItem]
    deleted: int


class Moved(_Model):
    moved: int
    destination_namespace: str


class SimilarHit(_Model):
    memory: Memory
    score: float


class PhaseThresholds(_Model):
    full_to_summary: float
    summary_to_ghost: float
    ghost_to_delete: float


class NamespacePolicy(_Model):
    """A namespace's decay policy. `namespaces.update_policy` sends all five fields."""

    initial_stability: float
    default_difficulty: float
    phase_thresholds: PhaseThresholds
    permastore_threshold: float
    decay_rate_multiplier: float | None


class WouldChange(_Model):
    full_to_summary: int
    summary_to_ghost: int
    deleted: int


class Namespace(_Model):
    name: str
    created_at: datetime
    memory_count: int
    policy: NamespacePolicy
    would_change: WouldChange | None = None


class PhaseCounts(_Model):
    full: int
    summary: int
    ghost: int


class EmbeddingCounts(_Model):
    ready: int
    pending: int
    failed: int
    none: int


class NamespaceStats(_Model):
    name: str
    memory_count: int
    phase_counts: PhaseCounts
    permastore_count: int
    average_strength: float
    edge_count: int
    embedding: EmbeddingCounts
    text_bytes: int
    oldest_created_at: datetime | None = None
    newest_created_at: datetime | None = None
    policy: NamespacePolicy


class NamespaceDeleted(_Model):
    name: str
    deleted: bool
    memories_deleted: int
    edges_removed: int


class TagCount(_Model):
    tag: str
    count: int


class ImportError(_Model):  # noqa: A001 - the schema's name
    line: int
    code: str
    message: str


class ImportResult(_Model):
    imported: int
    skipped: int
    errors: list[ImportError]
