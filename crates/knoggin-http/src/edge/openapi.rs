// SPDX-License-Identifier: AGPL-3.0-only
//! The OpenAPI 3.1 document of `/v1` (tab "REST API", The OpenAPI document), generated
//! with `utoipa`. The control routes carry their annotations on their handlers; the store
//! routes, whose handlers live in `store_api`, are described by the annotated stubs below.
//! A copy is committed at `docs/openapi.json`; a test fails when the two differ.

use std::path::Path;
use std::sync::LazyLock;

use knoggin_core::model::NamespacePolicy;
use knoggin_core::ops::{
    BatchResult, Context, CreateNamespace, FindSimilar, ForgetManyResult, ForgetMemories,
    Forgotten, Items, MemoryView, MoveMemories, Moved, NamespaceDeleted, NamespaceStatsResult,
    NamespaceView, Page, Recall, RecallResponse, ReinforceMemory, Reinforced, SimilarHit,
    StoreMemories, StoreMemory, StoredMemory, SupersedeMemory, TagCount,
};
use serde::Serialize;
use utoipa::{OpenApi, ToSchema};
use uuid::Uuid;

use super::control;
use crate::error::{Code, LockReason};

/// Every error answer: `{ "error": { ... } }`.
#[derive(Serialize, ToSchema, ts_rs::TS)]
#[schema(as = Error)]
#[ts(rename = "ApiError")]
pub(crate) struct ErrorEnvelope {
    error: ErrorBody,
}

/// The fields of an error.
#[derive(Serialize, ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ErrorBody {
    code: Code,
    message: String,
    /// Present only when one request field is at fault.
    #[ts(optional)]
    field: Option<String>,
    /// A fixed word, where the code has one (`store_locked`, `permission_denied`, `unavailable`).
    #[ts(optional)]
    reason: Option<String>,
    /// Only on `store_locked` from `GET /v1/session` on a locked local install.
    #[ts(optional)]
    docs_url: Option<String>,
    request_id: Uuid,
}

/// `POST /v1/import`'s answer.
#[derive(Serialize, ToSchema)]
#[schema(as = ImportResult)]
pub(crate) struct ImportResultDoc {
    imported: u64,
    skipped: u64,
    errors: Vec<ImportErrorDoc>,
}

/// One error line of an import.
#[derive(Serialize, ToSchema)]
#[schema(as = ImportError)]
pub(crate) struct ImportErrorDoc {
    line: u64,
    code: Code,
    message: String,
}

// The stubs below only carry the store routes' annotations; `store_api` serves them.

#[utoipa::path(post, path = "/v1/memories", tag = "memories", request_body = StoreMemory,
    params(StoreHeaders, Idempotency),
    responses((status = 201, body = StoredMemory), (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn store_memory() {}

#[utoipa::path(post, path = "/v1/memories/batch", tag = "memories", request_body = StoreMemories,
    params(StoreHeaders, Idempotency),
    responses((status = 200, body = BatchResult<StoredMemory>), (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn store_memories() {}

#[utoipa::path(get, path = "/v1/memories", tag = "memories",
    params(StoreHeaders,
        ("namespace" = Option<String>, Query), ("tag" = Option<Vec<String>>, Query, description = "Repeatable; all must match"),
        ("phase" = Option<String>, Query), ("createdAfter" = Option<String>, Query), ("createdBefore" = Option<String>, Query),
        ("includeFullText" = Option<bool>, Query), ("limit" = Option<u32>, Query), ("cursor" = Option<String>, Query)),
    responses((status = 200, body = Page<MemoryView>), (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn list_memories() {}

#[utoipa::path(get, path = "/v1/memories/{id}", tag = "memories",
    params(StoreHeaders, ("id" = Uuid, Path), ("namespace" = Option<String>, Query),
        ("recordAccess" = Option<bool>, Query), ("includeHistory" = Option<bool>, Query)),
    responses((status = 200, body = MemoryView), (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn get_memory() {}

#[utoipa::path(delete, path = "/v1/memories/{id}", tag = "memories",
    params(StoreHeaders, Idempotency, ("id" = Uuid, Path), ("namespace" = Option<String>, Query)),
    responses((status = 200, body = Forgotten), (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn forget_memory() {}

#[utoipa::path(post, path = "/v1/memories/forget", tag = "memories", request_body = ForgetMemories,
    params(StoreHeaders, Idempotency),
    responses((status = 200, body = ForgetManyResult), (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn forget_memories() {}

#[utoipa::path(post, path = "/v1/memories/{id}/reinforce", tag = "memories", request_body = ReinforceMemory,
    params(StoreHeaders, Idempotency, ("id" = Uuid, Path)),
    responses((status = 200, body = Reinforced), (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn reinforce_memory() {}

#[utoipa::path(post, path = "/v1/memories/{id}/supersede", tag = "memories", request_body = SupersedeMemory,
    params(StoreHeaders, Idempotency, ("id" = Uuid, Path)),
    responses((status = 201, body = StoredMemory), (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn supersede_memory() {}

#[utoipa::path(post, path = "/v1/memories/move", tag = "memories", request_body = MoveMemories,
    params(StoreHeaders, Idempotency),
    responses((status = 200, body = Moved), (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn move_memories() {}

#[utoipa::path(post, path = "/v1/memories/{id}/similar", tag = "memories", request_body = FindSimilar,
    params(StoreHeaders, ("id" = Uuid, Path)),
    responses((status = 200, body = Items<SimilarHit>), (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn find_similar() {}

#[utoipa::path(post, path = "/v1/recall", tag = "recall", request_body = Recall,
    params(StoreHeaders),
    responses((status = 200, body = RecallResponse), (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn recall() {}

#[utoipa::path(get, path = "/v1/context", tag = "recall",
    params(StoreHeaders, ("topic" = Option<String>, Query), ("namespace" = Option<String>, Query), ("limit" = Option<u32>, Query)),
    responses((status = 200, body = Context), (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn context() {}

#[utoipa::path(get, path = "/v1/namespaces", tag = "namespaces", params(StoreHeaders),
    responses((status = 200, body = Items<NamespaceView>), (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn list_namespaces() {}

#[utoipa::path(post, path = "/v1/namespaces", tag = "namespaces", request_body = CreateNamespace,
    params(StoreHeaders, Idempotency),
    responses((status = 201, body = NamespaceView), (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn create_namespace() {}

#[utoipa::path(get, path = "/v1/namespaces/{name}/stats", tag = "namespaces",
    params(StoreHeaders, ("name" = String, Path)),
    responses((status = 200, body = NamespaceStatsResult), (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn namespace_stats() {}

#[utoipa::path(put, path = "/v1/namespaces/{name}/policy", tag = "namespaces", request_body = NamespacePolicy,
    params(StoreHeaders, Idempotency, ("name" = String, Path), ("dryRun" = Option<bool>, Query)),
    responses((status = 200, body = NamespaceView), (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn update_policy() {}

#[utoipa::path(delete, path = "/v1/namespaces/{name}", tag = "namespaces",
    params(StoreHeaders, Idempotency, ("name" = String, Path), ("force" = Option<bool>, Query), ("dryRun" = Option<bool>, Query)),
    responses((status = 200, body = NamespaceDeleted), (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn delete_namespace() {}

#[utoipa::path(get, path = "/v1/tags", tag = "namespaces",
    params(StoreHeaders, ("namespace" = Option<String>, Query), ("prefix" = Option<String>, Query),
        ("limit" = Option<u32>, Query), ("cursor" = Option<String>, Query)),
    responses((status = 200, body = Page<TagCount>), (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn list_tags() {}

#[utoipa::path(get, path = "/v1/export", tag = "transfer",
    params(StoreHeaders, ("store" = Option<Uuid>, Query, description = "The store, from a session only")),
    responses((status = 200, description = "The export file, streamed", content_type = "application/x-ndjson", body = String),
        (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn export() {}

#[utoipa::path(post, path = "/v1/import", tag = "transfer",
    request_body(content = String, content_type = "application/x-ndjson", description = "An export file, streamed"),
    params(StoreHeaders, ("onConflict" = Option<String>, Query, description = "`skip` (default) or `error`")),
    responses((status = 200, body = ImportResultDoc), (status = "default", body = ErrorEnvelope)))]
#[allow(dead_code)]
fn import() {}

/// The headers that select the store and the default namespace.
#[derive(utoipa::IntoParams)]
#[into_params(parameter_in = Header)]
#[allow(dead_code)]
struct StoreHeaders {
    /// The store, for a session or the local socket; refused with an API key.
    #[param(rename = "Knoggin-Store")]
    knoggin_store: Option<Uuid>,
    /// The request's default namespace.
    #[param(rename = "Knoggin-Namespace")]
    knoggin_namespace: Option<String>,
}

/// The idempotency key of a route marked "I".
#[derive(utoipa::IntoParams)]
#[into_params(parameter_in = Header)]
#[allow(dead_code)]
struct Idempotency {
    /// 1 to 128 printable ASCII characters.
    #[param(rename = "Idempotency-Key")]
    idempotency_key: Option<String>,
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Knoggin",
        description = "Memory for AI agents: the REST API under `/v1`."
    ),
    paths(
        store_memory,
        store_memories,
        list_memories,
        get_memory,
        forget_memory,
        forget_memories,
        reinforce_memory,
        supersede_memory,
        move_memories,
        find_similar,
        recall,
        context,
        list_namespaces,
        create_namespace,
        namespace_stats,
        update_policy,
        delete_namespace,
        list_tags,
        export,
        import,
        control::healthz,
        control::readyz,
        control::session,
        control::openapi,
        control::list_stores,
        control::create_store,
        control::get_store,
        control::rename_store,
        control::delete_store,
        control::undelete_store,
        control::list_keys,
        control::create_key,
        control::rotate_key,
        control::revoke_key,
        control::delete_account,
        control::undelete_account,
        control::list_users,
        control::user_stores,
        control::remove_user,
        control::restore_user,
        crate::local::store_status,
        crate::local::admin_status,
        crate::local::snapshots,
        crate::local::browser_code,
        crate::local::local_session,
        crate::local::logout,
    ),
    components(schemas(ErrorEnvelope, ErrorBody, Code, LockReason))
)]
struct ApiDoc;

/// The document as JSON text, made once.
pub(crate) fn document_json() -> String {
    static DOC: LazyLock<String> = LazyLock::new(|| {
        public_names(ApiDoc::openapi())
            .and_then(|doc| doc.to_pretty_json().ok())
            .unwrap_or_else(|| "{}".into())
    });
    DOC.clone()
}

/// The public name of a generic shape that utoipa names `<Type>_<Argument>`: a page of
/// memories is `MemoryPage`, a list that is not paged `StoreList`, the batch answer
/// `BatchResult`. utoipa 6 names a generic only that way, and the shapes live in
/// `knoggin-core`, so the names are set here.
fn public_name(name: &str) -> Option<String> {
    let (wrapper, argument) = name.split_once('_')?;
    match wrapper {
        "Page" => Some(format!("{argument}Page")),
        "Items" => Some(format!("{argument}List")),
        "BatchResult" | "BatchItemResult" => Some(wrapper.to_owned()),
        _ => None,
    }
}

/// `doc` with every generic shape renamed by [`public_name`], in the components and in
/// every `$ref`.
fn public_names(doc: utoipa::openapi::OpenApi) -> Option<utoipa::openapi::OpenApi> {
    fn rename_refs(value: &mut serde_json::Value) {
        const PREFIX: &str = "#/components/schemas/";
        match value {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::String(target)) = map.get_mut("$ref")
                    && let Some(new) = target.strip_prefix(PREFIX).and_then(public_name)
                {
                    *target = format!("{PREFIX}{new}");
                }
                map.values_mut().for_each(rename_refs);
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(rename_refs),
            _ => {}
        }
    }
    let mut value = serde_json::to_value(&doc).ok()?;
    if let Some(schemas) = value
        .pointer_mut("/components/schemas")
        .and_then(serde_json::Value::as_object_mut)
    {
        let old: Vec<String> = schemas
            .keys()
            .filter(|k| public_name(k).is_some())
            .cloned()
            .collect();
        for name in old {
            if let (Some(schema), Some(new)) = (schemas.remove(&name), public_name(&name)) {
                schemas.insert(new, schema);
            }
        }
    }
    rename_refs(&mut value);
    serde_json::from_value(value).ok()
}

/// Writes the document to `path` (`docs/openapi.json`), with a final newline.
pub fn write_document(path: &Path) -> std::io::Result<()> {
    std::fs::write(path, format!("{}\n", document_json()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn committed() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/openapi.json")
    }

    /// The explicit step that refreshes the committed copy.
    #[test]
    #[ignore = "writes docs/openapi.json"]
    fn write_openapi() {
        write_document(&committed()).unwrap();
    }

    #[test]
    fn the_committed_document_is_current() {
        let on_disk = std::fs::read_to_string(committed()).unwrap_or_default();
        assert!(
            on_disk == format!("{}\n", document_json()),
            "docs/openapi.json is stale: run `cargo nextest run -p knoggin-http --run-ignored only -E 'test(write_openapi)'`"
        );
    }

    #[test]
    fn the_document_names_the_public_shapes() {
        let doc: serde_json::Value = serde_json::from_str(&document_json()).unwrap();
        assert!(doc["openapi"].as_str().unwrap().starts_with("3.1"));
        let schemas = doc["components"]["schemas"].as_object().unwrap();
        for name in [
            "Memory",
            "Namespace",
            "Store",
            "ApiKey",
            "ModelState",
            "Context",
            "NamespaceStats",
            "SimilarHit",
            "Session",
            "StoreStatus",
            "Error",
        ] {
            assert!(schemas.contains_key(name), "{name} missing");
        }
        assert!(doc["paths"]["/v1/stores/{id}/keys/{keyId}/rotate"]["post"].is_object());
    }

    #[test]
    fn every_schema_has_a_public_name_and_every_ref_resolves() {
        let text = document_json();
        let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
        let schemas = doc["components"]["schemas"].as_object().unwrap();
        for name in schemas.keys() {
            assert!(!name.contains('_'), "{name} is not a public name");
        }
        for name in ["MemoryPage", "StoreList", "BatchResult", "BatchItemResult"] {
            assert!(schemas.contains_key(name), "{name} missing");
        }
        for target in text.split("\"$ref\": \"").skip(1) {
            let target = &target[..target.find('"').unwrap()];
            let name = target.strip_prefix("#/components/schemas/").unwrap();
            assert!(schemas.contains_key(name), "{target} does not resolve");
        }
    }
}
