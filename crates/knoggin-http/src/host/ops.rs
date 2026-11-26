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

type Op<T> = Box<
    dyn FnOnce(&mut Store, &WriteContext, Timestamp) -> Result<(T, Vec<MemoryId>), EngineError>
        + Send,
>;

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
        self.write_on(&open, kind, move |s| {
            let reply = match keyed {
                Some((key, hash)) => {
                    s.run_idempotent(&key, hash, at, |s| op(s, &write, at))
                        .map(|i| match i {
                            Idempotent::Ran(t) => Reply::Fresh(t),
                            Idempotent::Replayed(bytes) => Reply::Replayed(bytes),
                        })
                }
                None => op(s, &write, at).map(|(t, _)| Reply::Fresh(t)),
            };
            reply.map_err(ApiError::from)
        })
        .await
    }

    /// The key with the request hash, when a key was sent.
    fn keyed<R: Serialize>(
        key: Option<String>,
        op: &str,
        request: &R,
    ) -> Result<Option<(String, [u8; 32])>, ApiError> {
        key.map(|k| request_hash(op, request).map(|h| (k, h)))
            .transpose()
    }

    /// The document vector for a save's duplicate check, or `None` when the content is
    /// not valid (the save then fails anyway), the model is not ready or the wait ran out.
    async fn duplicate_vector(&self, content: ContentRef<'_>, at: Timestamp) -> Option<Vec<f32>> {
        let tags = validate_content(content, at).ok()?;
        let labels = Labels {
            entities: content.entities.to_vec(),
            topics: content.topics.to_vec(),
            emotions: content.emotions.to_vec(),
        };
        let text = document_text_from_parts(
            content.entities,
            content.created_at.unwrap_or(at),
            content.summary,
            content.full_text,
            &tag_terms(&tags, &labels),
        );
        self.embedder.urgent(text).await
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
        let keyed = Self::keyed(key, "store_memory", &req)?;
        let at = now();
        let vector = if req.check_duplicates {
            self.duplicate_vector(req.content(), at).await
        } else {
            None
        };
        let skipped = req.check_duplicates && vector.is_none();
        let id = new_memory_id(req.created_at.unwrap_or(at));
        let out = self
            .idempotent(
                ctx,
                WriteKind::Client,
                keyed,
                at,
                Box::new(move |s, w, at| {
                    let mut saved = s.store_memory(req, id, vector, w, at)?;
                    if skipped {
                        saved.duplicate_check = Some(DuplicateCheck::Skipped);
                    }
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
        let keyed = Self::keyed(key, "store_memories", &req)?;
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
                Box::new(move |s, w, at| Ok((s.store_memories(req, ids, w, at)?, Vec::new()))),
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
        let keyed = Self::keyed(key, "supersede_memory", &req)?;
        let at = now();
        let vector = if req.check_duplicates {
            self.duplicate_vector(req.content(), at).await
        } else {
            None
        };
        let skipped = req.check_duplicates && vector.is_none();
        let id = new_memory_id(req.created_at.unwrap_or(at));
        let out = self
            .idempotent(
                ctx,
                WriteKind::Client,
                keyed,
                at,
                Box::new(move |s, w, at| {
                    let mut saved = s.supersede_memory(req, id, vector, w, at)?;
                    if skipped {
                        saved.duplicate_check = Some(DuplicateCheck::Skipped);
                    }
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
        let keyed = Self::keyed(key, "reinforce_memory", &req)?;
        self.idempotent(
            ctx,
            WriteKind::Client,
            keyed,
            now(),
            Box::new(move |s, w, at| Ok((s.reinforce_memory(req, w, at)?, Vec::new()))),
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
        let keyed = Self::keyed(key, "forget_memory", &req)?;
        self.idempotent(
            ctx,
            WriteKind::MakesRoom,
            keyed,
            now(),
            Box::new(move |s, w, at| Ok((s.forget_memory(req, w, at)?, Vec::new()))),
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
        let (kind, keyed) = if req.dry_run {
            (WriteKind::Bookkeeping, None)
        } else {
            (
                WriteKind::MakesRoom,
                Self::keyed(key, "forget_memories", &req)?,
            )
        };
        self.idempotent(
            ctx,
            kind,
            keyed,
            now(),
            Box::new(move |s, w, at| Ok((s.forget_memories(req, w, at)?, Vec::new()))),
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
        let keyed = Self::keyed(key, "move_memories", &req)?;
        self.idempotent(
            ctx,
            WriteKind::Client,
            keyed,
            now(),
            Box::new(move |s, w, at| Ok((s.move_memories(req, w, at)?, Vec::new()))),
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
        let keyed = Self::keyed(key, "create_namespace", &req)?;
        self.idempotent(
            ctx,
            WriteKind::Client,
            keyed,
            now(),
            Box::new(move |s, w, at| Ok((s.create_namespace(req, w, at)?, Vec::new()))),
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
        let (kind, keyed) = if req.dry_run {
            (WriteKind::Bookkeeping, None)
        } else {
            (
                WriteKind::Client,
                Self::keyed(key, "update_namespace_policy", &req)?,
            )
        };
        self.idempotent(
            ctx,
            kind,
            keyed,
            now(),
            Box::new(move |s, w, at| Ok((s.update_namespace_policy(req, w, at)?, Vec::new()))),
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
        let (kind, keyed) = match (req.dry_run, req.force) {
            (true, _) => (WriteKind::Bookkeeping, None),
            (false, force) => (
                if force {
                    WriteKind::MakesRoom
                } else {
                    WriteKind::Client
                },
                Self::keyed(key, "delete_namespace", &req)?,
            ),
        };
        self.idempotent(
            ctx,
            kind,
            keyed,
            now(),
            Box::new(move |s, w, at| Ok((s.delete_namespace(req, w, at)?, Vec::new()))),
        )
        .await
    }

    /// One read under the read lock; returns the open store for access recording.
    async fn read<R: Send + 'static>(
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

    /// Records access after a read, when the request asked for it and the credential may
    /// write. A failure is logged; the read's answer stands.
    async fn record(
        &self,
        ctx: &RequestContext,
        open: &Arc<OpenStore>,
        asked: bool,
        hits: Vec<AccessHit>,
    ) {
        if !asked || !ctx.permissions.write || hits.is_empty() {
            return;
        }
        let result = self
            .write_on(open, WriteKind::Bookkeeping, move |s| {
                s.record_access(&hits, now());
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
            self.record(ctx, &open, asked, vec![hit]).await;
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
        self.record(ctx, &open, asked, hits).await;
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
        req.namespace = fill(req.namespace.take(), ctx);
        let vector = self.query_vector(&req.query).await;
        let asked = req.record_access;
        let (result, open) = self
            .read(ctx, move |s, at| {
                s.recall(&req, vector.as_deref(), false, at)
            })
            .await?;
        self.record(ctx, &open, asked, result.hits).await;
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
    async fn query_vector(&self, query: &str) -> Option<Vec<f32>> {
        if query.trim().is_empty() || query.len() > QUERY_MAX_BYTES {
            return None;
        }
        self.embedder.query(query_text(query)).await
    }
}
