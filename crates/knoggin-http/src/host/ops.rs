// SPDX-License-Identifier: AGPL-3.0-only
//! The memory operations as REST and MCP call them. Each takes the request context and
//! the operation's request value from `knoggin_core::ops`, and does the host's part:
//! permissions, the default namespace, the request hash and idempotency, the duplicate
//! check's vector, the query vector, and access recording after reads.

use std::sync::Arc;

use knoggin_core::embed::{document_text_from_parts, query_text};
use knoggin_core::model::limits::ContentRef;
use knoggin_core::model::limits::{DEFAULT_NAMESPACE, QUERY_MAX_BYTES, validate_content};
use knoggin_core::model::{AccessKind, Labels, Phase, tag_terms};
use knoggin_core::ops::{
    AccessHit, BatchResult, Context, CreateNamespace, DeleteNamespace, DuplicateCheck, FindSimilar,
    ForgetManyResult, ForgetMemories, ForgetMemory, Forgotten, GetContext, GetMemory, Idempotent,
    Items, ListMemories, ListTags, MemoryView, MoveMemories, Moved, NamespaceDeleted,
    NamespaceStats, NamespaceStatsResult, NamespaceView, Page, Recall, RecallResponse,
    ReinforceMemory, Reinforced, SimilarResult, StoreMemories, StoreMemory, StoredMemory,
    SupersedeMemory, TagCount, UpdatePolicy, WriteContext,
};
use knoggin_core::{EngineError, MemoryId, Store, Timestamp};
use serde::Serialize;

use super::live::{self, UiEvent};
use super::{OpenStore, StoreHost, WriteKind, new_memory_id, now, request_hash};
use crate::ctx::RequestContext;
use crate::error::ApiError;

/// The answer of an idempotent write: run now, or the stored JSON of an earlier run.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply<T> {
    /// The operation ran now.
    Fresh(T),
    /// The stored answer of an earlier request with the same key; `Idempotent-Replayed`.
    Replayed(Vec<u8>),
}

impl<T: Serialize> Reply<T> {
    /// Whether this is a stored answer.
    pub fn is_replayed(&self) -> bool {
        matches!(self, Self::Replayed(_))
    }

    /// The answer as JSON bytes, the same for a fresh and a replayed answer.
    pub fn into_json(self) -> Result<Vec<u8>, ApiError> {
        match self {
            Self::Fresh(value) => serde_json::to_vec(&value).map_err(|_| ApiError::internal()),
            Self::Replayed(bytes) => Ok(bytes),
        }
    }
}

/// The namespace a request names, else the header's, else `default`.
fn fill(namespace: Option<String>, ctx: &RequestContext) -> Option<String> {
    Some(
        namespace
            .or_else(|| ctx.default_namespace.clone())
            .unwrap_or_else(|| DEFAULT_NAMESPACE.to_owned()),
    )
}

/// The IDs of the memories whose text a save's answer carries: its near duplicates.
fn text_ids(saved: &StoredMemory) -> Vec<MemoryId> {
    saved
        .near_duplicates
        .as_ref()
        .map(|d| d.matches.iter().map(|m| m.id).collect())
        .unwrap_or_default()
}

/// One write: the engine call, which also notes the web UI's events of what it did.
/// It runs only when the request is not answered from the idempotency record, so a
/// replay makes no event.
type Op<T> = Box<
    dyn FnOnce(
            &mut Store,
            &WriteContext,
            Timestamp,
            &mut Vec<UiEvent>,
        ) -> Result<(T, Vec<MemoryId>), EngineError>
        + Send,
>;

/// The client of a write, for its events.
fn client(w: &WriteContext) -> Option<&str> {
    w.client.as_deref()
}

impl StoreHost {
    /// One client write, idempotent when it has a key (and is not a dry run).
    async fn idempotent<T: Serialize + Send + 'static>(
        &self,
        ctx: &RequestContext,
        kind: WriteKind,
        keyed: Option<(String, [u8; 32])>,
        at: Timestamp,
        op: Op<T>,
    ) -> Result<Reply<T>, ApiError> {
        let write = ctx.write_context();
        let (open, _permit) = self.open_for(ctx).await?;
        let (reply, events) = self
            .write_on(&open, kind, move |s| {
                let mut events = Vec::new();
                let reply = match keyed {
                    Some((key, hash)) => s
                        .run_idempotent(&key, hash, at, |s| op(s, &write, at, &mut events))
                        .map(|i| match i {
                            Idempotent::Ran(t) => Reply::Fresh(t),
                            Idempotent::Replayed(bytes) => Reply::Replayed(bytes),
                        }),
                    None => op(s, &write, at, &mut events).map(|(t, _)| Reply::Fresh(t)),
                };
                reply.map(|r| (r, events)).map_err(ApiError::from)
            })
            .await?;
        open.events().publish(events);
        Ok(reply)
    }

    /// The request back, with the key and its request hash when a key was sent. The hash
    /// reads the whole request, which can be 10 MiB, so it is made on a blocking thread.
    async fn keyed<R: Serialize + Send + 'static>(
        key: Option<String>,
        op: &'static str,
        request: R,
    ) -> Result<(R, Option<(String, [u8; 32])>), ApiError> {
        let Some(key) = key else {
            return Ok((request, None));
        };
        tokio::task::spawn_blocking(move || {
            let hash = request_hash(op, &request)?;
            Ok((request, Some((key, hash))))
        })
        .await
        .map_err(|e| ApiError::from_join(&e))?
    }

    /// The request back, with the document vector for its duplicate check: `None` when
    /// the content is not valid (the save then fails anyway), the model is not ready or the
    /// wait ran out. The checks and the document text are made on a blocking thread.
    async fn duplicate_vector<R: Send + 'static>(
        &self,
        req: R,
        content: for<'a> fn(&'a R) -> ContentRef<'a>,
        at: Timestamp,
    ) -> Result<(R, Option<Vec<f32>>), ApiError> {
        let (req, text) = tokio::task::spawn_blocking(move || {
            let text = duplicate_text(content(&req), at);
            (req, text)
        })
        .await
        .map_err(|e| ApiError::from_join(&e))?;
        let vector = match text {
            Some(text) => self.embedder.urgent(text).await,
            None => None,
        };
        Ok((req, vector))
    }
    /// `POST /v1/memories`, the `store_memory` tool.
    pub async fn store_memory(
        &self,
        ctx: &RequestContext,
        mut req: StoreMemory,
        key: Option<String>,
    ) -> Result<Reply<StoredMemory>, ApiError> {
        ctx.require_write()?;
        req.namespace = fill(req.namespace.take(), ctx);
        let (req, keyed) = Self::keyed(key, "store_memory", req).await?;
        let at = now();
        let (req, vector) = if req.check_duplicates {
            self.duplicate_vector(req, StoreMemory::content, at).await?
        } else {
            (req, None)
        };
        let skipped = req.check_duplicates && vector.is_none();
        let id = new_memory_id(req.created_at.unwrap_or(at));
        let out = self
            .idempotent(
                ctx,
                WriteKind::Client,
                keyed,
                at,
                Box::new(move |s, w, at, ev| {
                    let mut saved = s.store_memory(req, id, vector, w, at)?;
                    if skipped {
                        saved.duplicate_check = Some(DuplicateCheck::Skipped);
                    }
                    ev.push(live::stored(s, saved.id, client(w), at));
                    let ids = text_ids(&saved);
                    Ok((saved, ids))
                }),
            )
            .await;
        self.notify_work();
        out
    }

    /// `POST /v1/memories/batch`.
    pub async fn store_memories(
        &self,
        ctx: &RequestContext,
        mut req: StoreMemories,
        key: Option<String>,
    ) -> Result<Reply<BatchResult<StoredMemory>>, ApiError> {
        ctx.require_write()?;
        for item in &mut req.memories {
            item.namespace = fill(item.namespace.take(), ctx);
        }
        let (req, keyed) = Self::keyed(key, "store_memories", req).await?;
        let at = now();
        let ids = req
            .memories
            .iter()
            .map(|m| new_memory_id(m.created_at.unwrap_or(at)))
            .collect();
        let out = self
            .idempotent(
                ctx,
                WriteKind::Client,
                keyed,
                at,
                Box::new(move |s, w, at, ev| {
                    let result = s.store_memories(req, ids, w, at)?;
                    for item in &result.results {
                        if let Some(m) = &item.memory {
                            ev.push(live::stored(s, m.id, client(w), at));
                        }
                    }
                    Ok((result, Vec::new()))
                }),
            )
            .await;
        self.notify_work();
        out
    }

    /// `POST /v1/memories/{id}/supersede`; `req.id` is the replaced memory.
    pub async fn supersede_memory(
        &self,
        ctx: &RequestContext,
        req: SupersedeMemory,
        key: Option<String>,
    ) -> Result<Reply<StoredMemory>, ApiError> {
        ctx.require_write()?;
        let (req, keyed) = Self::keyed(key, "supersede_memory", req).await?;
        let at = now();
        let (req, vector) = if req.check_duplicates {
            self.duplicate_vector(req, SupersedeMemory::content, at)
                .await?
        } else {
            (req, None)
        };
        let skipped = req.check_duplicates && vector.is_none();
        let id = new_memory_id(req.created_at.unwrap_or(at));
        let out = self
            .idempotent(
                ctx,
                WriteKind::Client,
                keyed,
                at,
                Box::new(move |s, w, at, ev| {
                    let mut saved = s.supersede_memory(req, id, vector, w, at)?;
                    if skipped {
                        saved.duplicate_check = Some(DuplicateCheck::Skipped);
                    }
                    ev.push(live::stored(s, saved.id, client(w), at));
                    let ids = text_ids(&saved);
                    Ok((saved, ids))
                }),
            )
            .await;
        self.notify_work();
        out
    }

    /// `POST /v1/memories/{id}/reinforce`.
    pub async fn reinforce_memory(
        &self,
        ctx: &RequestContext,
        req: ReinforceMemory,
        key: Option<String>,
    ) -> Result<Reply<Reinforced>, ApiError> {
        ctx.require_write()?;
        let (req, keyed) = Self::keyed(key, "reinforce_memory", req).await?;
        self.idempotent(
            ctx,
            WriteKind::Client,
            keyed,
            now(),
            Box::new(move |s, w, at, ev| {
                let (id, rating) = (req.id, req.quality);
                let before = s.memory(id).map(|m| m.stability);
                let done = s.reinforce_memory(req, w, at)?;
                ev.push(UiEvent::Reinforce(live::ReinforceEvent {
                    seq: 0,
                    at: at.millis(),
                    client: w.client.clone(),
                    id,
                    rating,
                    stability_before: before,
                    stability_after: done.stability,
                }));
                Ok((done, Vec::new()))
            }),
        )
        .await
    }

    /// `DELETE /v1/memories/{id}`. Its `namespace` is only a check and never filled in.
    pub async fn forget_memory(
        &self,
        ctx: &RequestContext,
        req: ForgetMemory,
        key: Option<String>,
    ) -> Result<Reply<Forgotten>, ApiError> {
        ctx.require_write()?;
        let (req, keyed) = Self::keyed(key, "forget_memory", req).await?;
        self.idempotent(
            ctx,
            WriteKind::MakesRoom,
            keyed,
            now(),
            Box::new(move |s, w, at, ev| {
                let done = s.forget_memory(req, w, at)?;
                if done.deleted {
                    ev.push(live::forgotten(done.id, client(w), at));
                }
                Ok((done, Vec::new()))
            }),
        )
        .await
    }

    /// `POST /v1/memories/forget`. A dry run is never recorded.
    pub async fn forget_memories(
        &self,
        ctx: &RequestContext,
        req: ForgetMemories,
        key: Option<String>,
    ) -> Result<Reply<ForgetManyResult>, ApiError> {
        ctx.require_write()?;
        let kind = if req.dry_run {
            WriteKind::Bookkeeping
        } else {
            WriteKind::MakesRoom
        };
        let key = key.filter(|_| !req.dry_run);
        let (req, keyed) = Self::keyed(key, "forget_memories", req).await?;
        self.idempotent(
            ctx,
            kind,
            keyed,
            now(),
            Box::new(move |s, w, at, ev| {
                let dry_run = req.dry_run;
                let done = s.forget_memories(req, w, at)?;
                if !dry_run {
                    ev.extend(
                        done.results
                            .iter()
                            .filter(|r| r.deleted)
                            .map(|r| live::forgotten(r.id, client(w), at)),
                    );
                }
                Ok((done, Vec::new()))
            }),
        )
        .await
    }

    /// `POST /v1/memories/move`. `sourceNamespace` is only a check and never filled in.
    pub async fn move_memories(
        &self,
        ctx: &RequestContext,
        req: MoveMemories,
        key: Option<String>,
    ) -> Result<Reply<Moved>, ApiError> {
        ctx.require_write()?;
        let (req, keyed) = Self::keyed(key, "move_memories", req).await?;
        self.idempotent(
            ctx,
            WriteKind::Client,
            keyed,
            now(),
            Box::new(move |s, w, at, _| Ok((s.move_memories(req, w, at)?, Vec::new()))),
        )
        .await
    }

    /// `POST /v1/namespaces`.
    pub async fn create_namespace(
        &self,
        ctx: &RequestContext,
        req: CreateNamespace,
        key: Option<String>,
    ) -> Result<Reply<NamespaceView>, ApiError> {
        ctx.require_write()?;
        let (req, keyed) = Self::keyed(key, "create_namespace", req).await?;
        self.idempotent(
            ctx,
            WriteKind::Client,
            keyed,
            now(),
            Box::new(move |s, w, at, _| Ok((s.create_namespace(req, w, at)?, Vec::new()))),
        )
        .await
    }

    /// `PUT /v1/namespaces/{name}/policy`. A dry run is never recorded.
    pub async fn update_namespace_policy(
        &self,
        ctx: &RequestContext,
        req: UpdatePolicy,
        key: Option<String>,
    ) -> Result<Reply<NamespaceView>, ApiError> {
        ctx.require_write()?;
        let kind = if req.dry_run {
            WriteKind::Bookkeeping
        } else {
            WriteKind::Client
        };
        let key = key.filter(|_| !req.dry_run);
        let (req, keyed) = Self::keyed(key, "update_namespace_policy", req).await?;
        self.idempotent(
            ctx,
            kind,
            keyed,
            now(),
            Box::new(move |s, w, at, _| Ok((s.update_namespace_policy(req, w, at)?, Vec::new()))),
        )
        .await
    }

    /// `DELETE /v1/namespaces/{name}`. A forced delete only makes room; a dry run is never
    /// recorded.
    pub async fn delete_namespace(
        &self,
        ctx: &RequestContext,
        req: DeleteNamespace,
        key: Option<String>,
    ) -> Result<Reply<NamespaceDeleted>, ApiError> {
        ctx.require_write()?;
        let kind = match (req.dry_run, req.force) {
            (true, _) => WriteKind::Bookkeeping,
            (false, true) => WriteKind::MakesRoom,
            (false, false) => WriteKind::Client,
        };
        let key = key.filter(|_| !req.dry_run);
        let (req, keyed) = Self::keyed(key, "delete_namespace", req).await?;
        self.idempotent(
            ctx,
            kind,
            keyed,
            now(),
            Box::new(move |s, w, at, ev| {
                // The memories a real delete removes, for their `forget` events.
                let gone: Vec<MemoryId> = match s.namespace_by_name(&req.name) {
                    Some(ns) if !req.dry_run => {
                        let ns = ns.id;
                        s.memories()
                            .filter(|m| m.namespace == ns)
                            .map(|m| m.id)
                            .collect()
                    }
                    _ => Vec::new(),
                };
                let done = s.delete_namespace(req, w, at)?;
                if done.deleted {
                    ev.extend(
                        gone.into_iter()
                            .map(|id| live::forgotten(id, client(w), at)),
                    );
                }
                Ok((done, Vec::new()))
            }),
        )
        .await
    }

    /// One read under the read lock; returns the open store for access recording.
    pub(crate) async fn read<R: Send + 'static>(
        &self,
        ctx: &RequestContext,
        f: impl FnOnce(&Store, Timestamp) -> Result<R, EngineError> + Send + 'static,
    ) -> Result<(R, Arc<OpenStore>), ApiError> {
        ctx.require_read()?;
        let (open, _permit) = self.open_for(ctx).await?;
        let out = self
            .read_on(&open, move |s| f(s, now()).map_err(ApiError::from))
            .await?;
        Ok((out, open))
    }

    /// One read for the web UI's read models, with the open store's own figures; it
    /// records nothing.
    pub(crate) async fn read_with<R: Send + 'static>(
        &self,
        ctx: &RequestContext,
        f: impl FnOnce(&Store, &OpenStore, Timestamp) -> Result<R, ApiError> + Send + 'static,
    ) -> Result<R, ApiError> {
        ctx.require_read()?;
        let (open, _permit) = self.open_for(ctx).await?;
        let figures = open.clone();
        self.read_on(&open, move |s| f(s, &figures, now())).await
    }

    /// Records access after a read, when the request asked for it and the credential may
    /// write. A failure is logged; the read's answer stands.
    async fn record(
        &self,
        ctx: &RequestContext,
        open: &Arc<OpenStore>,
        asked: bool,
        hits: Vec<AccessHit>,
        at: Timestamp,
    ) {
        if !asked || !ctx.permissions.write || hits.is_empty() {
            return;
        }
        let result = self
            .write_on(open, WriteKind::Bookkeeping, move |s| {
                s.record_access(&hits, at);
                Ok(())
            })
            .await;
        if let Err(e) = result {
            tracing::warn!(code = e.code.as_str(), "recording access failed");
        }
    }

    /// `GET /v1/memories/{id}`. Its `namespace` is only a check.
    pub async fn get_memory(
        &self,
        ctx: &RequestContext,
        req: GetMemory,
    ) -> Result<MemoryView, ApiError> {
        let asked = req.record_access;
        let (view, open) = self.read(ctx, move |s, at| s.get_memory(&req, at)).await?;
        if view.phase != Phase::Ghost {
            let hit = AccessHit {
                id: view.id,
                kind: AccessKind::Direct,
            };
            self.record(ctx, &open, asked, vec![hit], now()).await;
        }
        Ok(view)
    }

    /// `POST /v1/memories/{id}/similar`. Hits are recorded as `associative`.
    pub async fn find_similar(
        &self,
        ctx: &RequestContext,
        req: FindSimilar,
    ) -> Result<SimilarResult, ApiError> {
        let asked = req.record_access;
        let (result, open) = self
            .read(ctx, move |s, at| s.find_similar(&req, at))
            .await?;
        let hits = result
            .items
            .iter()
            .map(|h| AccessHit {
                id: h.memory.id,
                kind: AccessKind::Associative,
            })
            .collect();
        self.record(ctx, &open, asked, hits, now()).await;
        Ok(result)
    }

    /// `GET /v1/memories`. Never records access.
    pub async fn list_memories(
        &self,
        ctx: &RequestContext,
        mut req: ListMemories,
    ) -> Result<Page<MemoryView>, ApiError> {
        req.namespace = fill(req.namespace.take(), ctx);
        Ok(self
            .read(ctx, move |s, at| s.list_memories(&req, at))
            .await?
            .0)
    }

    /// `GET /v1/namespaces`, by name.
    pub async fn list_namespaces(
        &self,
        ctx: &RequestContext,
    ) -> Result<Items<NamespaceView>, ApiError> {
        let items = self.read(ctx, |s, _| Ok(s.list_namespaces())).await?.0;
        Ok(Items { items })
    }

    /// `GET /v1/tags`.
    pub async fn list_tags(
        &self,
        ctx: &RequestContext,
        mut req: ListTags,
    ) -> Result<Page<TagCount>, ApiError> {
        req.namespace = fill(req.namespace.take(), ctx);
        Ok(self.read(ctx, move |s, _| s.list_tags(&req)).await?.0)
    }

    /// `GET /v1/namespaces/{name}/stats`; without a name, the request's namespace.
    pub async fn namespace_stats(
        &self,
        ctx: &RequestContext,
        mut req: NamespaceStats,
    ) -> Result<NamespaceStatsResult, ApiError> {
        req.namespace = fill(req.namespace.take(), ctx);
        Ok(self
            .read(ctx, move |s, at| s.namespace_stats(&req, at))
            .await?
            .0)
    }

    /// `POST /v1/recall`. Without a query vector the answer is `degraded`.
    pub async fn recall(
        &self,
        ctx: &RequestContext,
        mut req: Recall,
    ) -> Result<RecallResponse, ApiError> {
        ctx.require_read()?;
        let started = std::time::Instant::now();
        req.namespace = fill(req.namespace.take(), ctx);
        let vector = self.query_vector(&req.query).await;
        let asked = req.record_access;
        let ((result, namespace), open) = self
            .read(ctx, move |s, at| {
                let result = s.recall(&req, vector.as_deref(), false, at)?;
                // The engine finds a namespace ignoring ASCII case; the event names it as
                // it is stored.
                let namespace = req
                    .namespace
                    .as_deref()
                    .and_then(|n| s.namespace_by_name(n))
                    .map(|n| (n.id, n.name.clone()));
                Ok((result, namespace))
            })
            .await?;
        // The event and the access it records share one time, so that the Timeline can
        // join them into one row.
        let at = now();
        if let Some(namespace) = namespace {
            open.events().publish(vec![live::recalled(
                &result.trace,
                namespace,
                ctx.client.as_deref(),
                at,
            )]);
        }
        self.record(ctx, &open, asked, result.hits, at).await;
        self.recall_ms.note(started.elapsed());
        Ok(result.response)
    }

    /// `GET /v1/context`. Never records access.
    pub async fn context(
        &self,
        ctx: &RequestContext,
        mut req: GetContext,
    ) -> Result<Context, ApiError> {
        ctx.require_read()?;
        req.namespace = fill(req.namespace.take(), ctx);
        let vector = match req.topic.as_deref() {
            Some(topic) => self.query_vector(topic).await,
            None => None,
        };
        let result = self
            .read(ctx, move |s, at| s.context(&req, vector.as_deref(), at))
            .await?
            .0;
        Ok(Context {
            result,
            model: self.model_state(),
            web_url: self.config.web_url.clone(),
        })
    }

    /// The query vector; none for a query the engine will refuse anyway.
    pub(crate) async fn query_vector(&self, query: &str) -> Option<Vec<f32>> {
        if query.trim().is_empty() || query.len() > QUERY_MAX_BYTES {
            return None;
        }
        self.embedder.query(query_text(query)).await
    }
}

/// The document text of a save's duplicate check, when its content is valid.
fn duplicate_text(content: ContentRef<'_>, at: Timestamp) -> Option<String> {
    let tags = validate_content(content, at).ok()?;
    let labels = Labels {
        entities: content.entities.to_vec(),
        topics: content.topics.to_vec(),
        emotions: content.emotions.to_vec(),
    };
    Some(document_text_from_parts(
        content.entities,
        content.created_at.unwrap_or(at),
        content.summary,
        content.full_text,
        &tag_terms(&tags, &labels),
    ))
}
