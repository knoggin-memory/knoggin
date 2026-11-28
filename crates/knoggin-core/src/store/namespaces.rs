// SPDX-License-Identifier: AGPL-3.0-only
//! The namespace writes: create, policy update and delete.

use std::collections::BTreeSet;

use crate::error::EngineError;
use crate::model::limits::{DEFAULT_NAMESPACE, validate_namespace_name};
use crate::model::{EdgeKey, Namespace, NamespacePolicy};
use crate::ops::{
    CreateNamespace, DeleteNamespace, NamespaceDeleted, NamespaceView, UpdatePolicy, WriteContext,
};
use crate::time::Timestamp;

use super::Store;

impl Store {
    /// The API view of a namespace.
    pub(crate) fn namespace_view(&self, namespace: &Namespace) -> NamespaceView {
        NamespaceView {
            name: namespace.name.clone(),
            created_at: namespace.created_at,
            memory_count: self.namespace_counts(namespace.id).memories,
            policy: namespace.policy,
            would_change: None,
        }
    }

    /// Creates a namespace. Names are unique ignoring ASCII case.
    pub fn create_namespace(
        &mut self,
        req: CreateNamespace,
        ctx: &WriteContext,
        now: Timestamp,
    ) -> Result<NamespaceView, EngineError> {
        validate_namespace_name("name", &req.name)?;
        let policy = NamespacePolicy {
            initial_stability: req.initial_stability,
            decay_rate_multiplier: req.decay_rate_multiplier,
            ..NamespacePolicy::default()
        };
        policy.validate()?;
        if self
            .namespaces
            .values()
            .any(|n| n.name.eq_ignore_ascii_case(&req.name))
        {
            return Err(EngineError::AlreadyExists(format!(
                "namespace '{}' already exists",
                req.name
            )));
        }
        let id = self
            .allocate_namespace_id()
            .ok_or_else(|| EngineError::invalid("name", "no namespace IDs are left"))?;
        let namespace = Namespace {
            id,
            name: req.name,
            created_at: now,
            policy,
        };
        let view = self.namespace_view(&namespace);
        self.insert_namespace(namespace);
        self.note_client_write(ctx, now);
        Ok(view)
    }

    /// Replaces a namespace's policy. A dry run changes nothing and reports what the
    /// next sweep would move under the new policy.
    pub fn update_namespace_policy(
        &mut self,
        req: UpdatePolicy,
        ctx: &WriteContext,
        now: Timestamp,
    ) -> Result<NamespaceView, EngineError> {
        let id = self.resolve_namespace(Some(&req.name))?.id;
        req.policy.validate()?;
        if req.dry_run {
            let would_change = self.would_change(id, req.policy, now);
            let mut view = self.namespace_view(self.resolve_namespace(Some(&req.name))?);
            view.policy = req.policy;
            view.would_change = Some(would_change);
            return Ok(view);
        }
        // The policy feeds no index, so it is set in place.
        if let Some(namespace) = self.namespaces.get_mut(&id) {
            namespace.policy = req.policy;
        }
        self.note_client_write(ctx, now);
        Ok(self.namespace_view(self.resolve_namespace(Some(&req.name))?))
    }

    /// Deletes a namespace. `default` cannot be deleted; one that holds memories only
    /// with `force`, which removes them. A dry run reports the same counts and deletes
    /// nothing.
    pub fn delete_namespace(
        &mut self,
        req: DeleteNamespace,
        ctx: &WriteContext,
        now: Timestamp,
    ) -> Result<NamespaceDeleted, EngineError> {
        if req.name.eq_ignore_ascii_case(DEFAULT_NAMESPACE) {
            return Err(EngineError::invalid(
                "name",
                "the namespace 'default' cannot be deleted",
            ));
        }
        let namespace = self.resolve_namespace(Some(&req.name))?;
        let (id, name) = (namespace.id, namespace.name.clone());
        let ids: Vec<_> = self
            .memories
            .values()
            .filter(|m| m.namespace == id)
            .map(|m| m.id)
            .collect();
        if !ids.is_empty() && !req.force {
            return Err(EngineError::NamespaceNotEmpty(format!(
                "namespace '{name}' still has {} memories",
                ids.len()
            )));
        }
        let edges: BTreeSet<EdgeKey> = ids
            .iter()
            .flat_map(|m| self.graph.edges_of(*m).map(|e| e.key()))
            .collect();
        let answer = NamespaceDeleted {
            name,
            deleted: !req.dry_run,
            memories_deleted: ids.len() as u64,
            edges_removed: edges.len() as u64,
        };
        if req.dry_run {
            return Ok(answer);
        }
        if !ids.is_empty() {
            self.note_forget();
        }
        for memory in ids {
            self.remove_memory(memory);
        }
        self.remove_namespace(id);
        self.note_client_write(ctx, now);
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ErrorCode;
    use crate::store::write::tests::{DAY, T0, ctx, mid, namespace, req, store, t, vector};

    fn create(s: &mut Store, body: serde_json::Value) -> Result<NamespaceView, EngineError> {
        s.create_namespace(serde_json::from_value(body).unwrap(), &ctx(), t(T0))
    }

    fn delete(
        s: &mut Store,
        name: &str,
        force: bool,
        dry_run: bool,
    ) -> Result<NamespaceDeleted, EngineError> {
        let req = DeleteNamespace {
            name: name.into(),
            force,
            dry_run,
        };
        s.delete_namespace(req, &ctx(), t(T0))
    }

    #[test]
    fn create_validates_and_names_are_unique_ignoring_case() {
        let mut s = store();
        let v = create(
            &mut s,
            json!({ "name": "Work", "initialStability": 7.0, "decayRateMultiplier": 0.0 }),
        )
        .unwrap();
        assert_eq!(
            (v.name.as_str(), v.memory_count, v.created_at),
            ("Work", 0, t(T0))
        );
        assert_eq!(v.policy.initial_stability, 7.0);
        assert_eq!(v.policy.decay_rate_multiplier, Some(0.0));
        assert_eq!(s.changes().content, 1);
        assert_eq!(
            create(&mut s, json!({ "name": "work" }))
                .unwrap_err()
                .code(),
            ErrorCode::AlreadyExists
        );
        assert_eq!(
            create(&mut s, json!({ "name": "DEFAULT" }))
                .unwrap_err()
                .code(),
            ErrorCode::AlreadyExists
        );
        assert_eq!(
            create(&mut s, json!({ "name": "a b" }))
                .unwrap_err()
                .field(),
            Some("name")
        );
        assert_eq!(
            create(&mut s, json!({ "name": "" })).unwrap_err().field(),
            Some("name")
        );
        assert_eq!(
            create(&mut s, json!({ "name": "x", "initialStability": 0.001 }))
                .unwrap_err()
                .field(),
            Some("initialStability")
        );
        assert_eq!(
            create(&mut s, json!({ "name": "x", "decayRateMultiplier": -1.0 }))
                .unwrap_err()
                .field(),
            Some("decayRateMultiplier")
        );
        assert_eq!(s.changes().content, 1);
        assert_eq!(
            s.namespace_by_name("work").unwrap().id,
            crate::NamespaceId::new(2)
        );
    }

    #[test]
    fn policy_updates_and_dry_runs() {
        let mut s = store();
        s.store_memory(req(json!({ "summary": "a" })), mid(1), None, &ctx(), t(T0))
            .unwrap();
        let mut policy = NamespacePolicy {
            decay_rate_multiplier: Some(0.001),
            ..NamespacePolicy::default()
        };
        let up = |policy, dry_run| UpdatePolicy {
            name: "Default".into(),
            policy,
            dry_run,
        };
        let v = s
            .update_namespace_policy(up(policy, true), &ctx(), t(T0 + 10 * DAY))
            .unwrap();
        let w = v.would_change.unwrap();
        assert_eq!(
            (w.full_to_summary, w.summary_to_ghost, w.deleted),
            (1, 0, 0)
        );
        assert_eq!(v.policy, policy);
        assert_eq!(
            s.namespace_by_name("default").unwrap().policy,
            NamespacePolicy::default(),
            "a dry run changes nothing"
        );
        assert_eq!(s.changes().content, 1);
        let v = s
            .update_namespace_policy(up(policy, false), &ctx(), t(T0))
            .unwrap();
        assert!(v.would_change.is_none());
        assert_eq!(s.namespace_by_name("default").unwrap().policy, policy);
        assert_eq!(s.changes().content, 2);
        policy.phase_thresholds.ghost_to_delete = 0.9;
        assert_eq!(
            s.update_namespace_policy(up(policy, false), &ctx(), t(T0))
                .unwrap_err()
                .field(),
            Some("phaseThresholds")
        );
        let missing = UpdatePolicy {
            name: "nope".into(),
            policy,
            dry_run: false,
        };
        assert_eq!(
            s.update_namespace_policy(missing, &ctx(), t(T0))
                .unwrap_err()
                .code(),
            ErrorCode::NotFound
        );
    }

    #[test]
    fn delete_refuses_default_and_non_empty_namespaces_without_force() {
        let mut s = store();
        namespace(&mut s, "work");
        for name in ["default", "Default"] {
            let err = delete(&mut s, name, true, false).unwrap_err();
            assert_eq!(
                (err.code(), err.field()),
                (ErrorCode::InvalidArgument, Some("name"))
            );
        }
        assert_eq!(
            delete(&mut s, "nope", false, false).unwrap_err().code(),
            ErrorCode::NotFound
        );
        let body =
            |n: &str| req(json!({ "summary": n, "namespace": "work", "entities": ["Maya"] }));
        s.store_memory(body("a"), mid(1), Some(vector("a")), &ctx(), t(T0))
            .unwrap();
        s.store_memory(body("b"), mid(2), None, &ctx(), t(T0))
            .unwrap();
        s.store_memory(
            req(json!({ "summary": "c", "entities": ["Maya"] })),
            mid(3),
            None,
            &ctx(),
            t(T0),
        )
        .unwrap();
        // An edge across namespaces, as a move leaves behind.
        s.insert_edge(crate::model::Edge {
            source: mid(3),
            target: mid(1),
            edge_type: crate::model::EdgeType::ParentChild,
            weight: 1.0,
            auto_created: false,
            created_at: t(T0),
        });
        assert_eq!(
            delete(&mut s, "work", false, false).unwrap_err().code(),
            ErrorCode::NamespaceNotEmpty
        );
        let content = s.changes().content;
        let dry = delete(&mut s, "WORK", true, true).unwrap();
        assert_eq!(
            (dry.deleted, dry.memories_deleted, dry.edges_removed),
            (false, 2, 3)
        );
        assert_eq!((s.memories().count(), s.changes().content), (3, content));
        let done = delete(&mut s, "WORK", true, false).unwrap();
        assert_eq!(
            (
                done.name.as_str(),
                done.deleted,
                done.memories_deleted,
                done.edges_removed
            ),
            ("work", true, 2, 3)
        );
        assert!(s.namespace_by_name("work").is_none());
        assert_eq!(s.memories().count(), 1);
        assert_eq!(s.graph().len(), 0);
        assert_eq!(s.changes().content, content + 1);
        crate::store::write::tests::assert_matches_rebuild(&mut s);
        // An empty namespace goes without force; its ID is never reused.
        namespace(&mut s, "empty");
        assert!(delete(&mut s, "empty", false, false).unwrap().deleted);
        namespace(&mut s, "again");
        assert_eq!(
            s.namespace_by_name("again").unwrap().id,
            crate::NamespaceId::new(4)
        );
    }
}
