    #![allow(unused_imports)]

    use super::*;
    use crate::domain::entities::{
        AdrDocument, AdrStatus, Constraint, ConstraintSummary, Decision, DecisionCodeLink,
        DecisionSummary, EntityNode, LinkSource, LinkType, Repository, TemporalMode,
    };

    async fn test_store() -> SqliteStore {
        let store = SqliteStore::connect("sqlite::memory:")
            .await
            .expect("in-memory store");
        store.run_migrations().await.expect("migrations");
        store
    }

    async fn column_names(store: &SqliteStore, table: &str) -> Vec<String> {
        sqlx::query(&format!("PRAGMA table_info({})", table))
            .fetch_all(&store.pool)
            .await
            .expect("table info")
            .iter()
            .map(|row| row.get("name"))
            .collect()
    }

    #[tokio::test]
    async fn migrations_add_episode_decision_link_columns() {
        let store = test_store().await;

        let decision_columns = column_names(&store, "decisions").await;
        assert!(decision_columns.contains(&"title".to_string()));
        assert!(decision_columns.contains(&"episode_id".to_string()));

        let episode_columns = column_names(&store, "episodes").await;
        assert!(episode_columns.contains(&"repo_id".to_string()));
    }

    #[tokio::test]
    async fn migrations_add_symbol_end_line_column() {
        let store = test_store().await;

        let symbol_columns = column_names(&store, "symbols").await;
        assert!(symbol_columns.contains(&"end_line".to_string()));
    }

    #[tokio::test]
    async fn insert_symbol_persists_end_line() {
        let store = test_store().await;
        let repo = store
            .upsert_repository("C:/repo/symbol-spans", None)
            .await
            .expect("repo");
        let file_id = store
            .upsert_file(
                repo.id,
                "src/lib.rs",
                "2026-05-05T09:00:00Z",
                "2026-05-05T09:00:00Z",
            )
            .await
            .expect("file");

        store
            .insert_symbol(
                file_id,
                "multi_line",
                "function",
                10,
                14,
                "2026-05-05T09:00:00Z",
                "2026-05-05T09:00:00Z",
                None,
                None,
                None,
                false,
                None,
                None,
            )
            .await
            .expect("symbol");

        let row = sqlx::query("SELECT line, end_line FROM symbols WHERE file_id = ? AND name = ?")
            .bind(file_id.to_string())
            .bind("multi_line")
            .fetch_one(&store.pool)
            .await
            .expect("symbol row");

        assert_eq!(row.get::<i64, _>("line"), 10);
        assert_eq!(row.get::<i64, _>("end_line"), 14);
    }

    async fn insert_snapshot_symbol_edge_fixture(store: &SqliteStore) -> (Uuid, String, String) {
        let now = "2026-05-05T09:00:00Z";
        let repo = store
            .upsert_repository("C:/repo/symbol-snapshot", None)
            .await
            .expect("repo");
        let source_file = store
            .upsert_file(repo.id, "src/source.rs", now, now)
            .await
            .expect("source file");
        let sink_file = store
            .upsert_file(repo.id, "src/sink.rs", now, now)
            .await
            .expect("sink file");

        store
            .insert_symbol(
                source_file,
                "source",
                "function",
                1,
                3,
                now,
                now,
                None,
                None,
                None,
                false,
                None,
                None,
            )
            .await
            .expect("source symbol");
        store
            .insert_symbol(
                sink_file,
                "sink",
                "function",
                5,
                7,
                now,
                now,
                None,
                None,
                None,
                false,
                None,
                None,
            )
            .await
            .expect("sink symbol");

        let source_id = store
            .find_symbol_id_in_file(source_file, "source")
            .await
            .expect("source id")
            .expect("source symbol id");
        let sink_id = store
            .find_symbol_id_in_file(sink_file, "sink")
            .await
            .expect("sink id")
            .expect("sink symbol id");

        store
            .insert_symbol_edge(&SymbolEdge {
                id: Uuid::new_v4(),
                repo_id: repo.id,
                from_id: source_id,
                to_id: Some(sink_id),
                to_name: Some("sink".to_string()),
                edge_type: "calls".to_string(),
                confidence: 0.95,
                valid_from: now.to_string(),
            })
            .await
            .expect("symbol edge");

        (repo.id, source_id.to_string(), sink_id.to_string())
    }

    #[tokio::test]
    async fn graph_snapshot_uses_symbol_edges_without_file_symbol_projection() {
        let store = test_store().await;
        let (repo_id, source_id, sink_id) = insert_snapshot_symbol_edge_fixture(&store).await;

        let (nodes, edges) = store
            .fetch_graph_snapshot(repo_id, "2026-05-05T10:00:00Z", Some(10), Some(50))
            .await
            .expect("snapshot");

        assert!(nodes.iter().any(|n| n.id == source_id && n.kind == "symbol"));
        assert!(nodes.iter().any(|n| n.id == sink_id && n.kind == "symbol"));
        assert!(
            nodes
                .iter()
                .filter(|n| n.kind == "symbol")
                .all(|n| n.detail.as_deref().unwrap_or("").contains("src/")),
            "symbol file paths should be metadata on symbol nodes"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.source == source_id && e.target == sink_id && e.edge_type == "calls")
        );
        assert!(
            !edges
                .iter()
                .any(|e| e.edge_type == "contains" && e.source.starts_with("file:")),
            "file-to-symbol containment should not organize the symbol graph"
        );
        assert!(
            !edges
                .iter()
                .any(|e| e.edge_type == "depends_on" && e.source.starts_with("file:")),
            "cross-file symbol calls should stay as direct symbol edges"
        );
    }

    #[tokio::test]
    async fn focused_snapshot_keeps_file_path_as_symbol_metadata() {
        let store = test_store().await;
        let (repo_id, source_id, sink_id) = insert_snapshot_symbol_edge_fixture(&store).await;

        let (nodes, edges) = store
            .fetch_focused_snapshot(repo_id, "source", 1, "2026-05-05T10:00:00Z")
            .await
            .expect("focused snapshot");

        assert!(nodes.iter().any(|n| n.id == source_id && n.kind == "symbol"));
        assert!(nodes.iter().any(|n| n.id == sink_id && n.kind == "symbol"));
        assert!(
            nodes.iter().all(|n| n.kind != "file"),
            "file nodes are not needed when no file-scoped fact is present"
        );
        assert!(
            nodes
                .iter()
                .filter(|n| n.kind == "symbol")
                .all(|n| n.detail.as_deref().unwrap_or("").contains("src/"))
        );
        assert!(
            edges
                .iter()
                .any(|e| e.source == source_id && e.target == sink_id && e.edge_type == "calls")
        );
        assert!(
            !edges
                .iter()
                .any(|e| e.edge_type == "contains" && e.source.starts_with("file:"))
        );
    }

    #[tokio::test]
    async fn migrations_add_claim_and_anchor_tables() {
        let store = test_store().await;
        for table in [
            "claims",
            "evidence_anchors",
            "evidence_verifications",
            "index_lanes",
            "freshness_manifests",
        ] {
            let cols = column_names(&store, table).await;
            assert!(!cols.is_empty(), "{table} should exist after migrations");
        }
    }

    #[tokio::test]
    async fn claim_anchor_verification_round_trip() {
        use crate::domain::entities::{
            AnchorIdentity, AnchorSource, AnchorVerification, Claim, ClaimKind, EditClass,
            EvidenceAnchor, EvidenceGrade, Freshness, Locator, Polarity,
        };

        let store = test_store().await;
        let repo = store
            .upsert_repository("C:/repo/claims", None)
            .await
            .expect("repo");
        let now = "2026-09-01T09:00:00Z";
        let subject_id = Uuid::new_v4();

        let claim = Claim {
            id: Uuid::new_v4(),
            repo_id: repo.id,
            kind: ClaimKind::Constraint,
            subject_type: "constraint".to_string(),
            subject_id,
            text: "Order state must never be mutated in place".to_string(),
            polarity: Some(Polarity::MustNot),
            evidence_grade: EvidenceGrade::Proven,
            read_set: vec![AnchorIdentity {
                source_kind: AnchorSource::Adr,
                source_uri: "ADR-0042".to_string(),
                subpath: "Decision".to_string(),
            }],
            valid_from: now.to_string(),
            valid_to: None,
            ingested_at: now.to_string(),
            source_time: None,
            confidence: 1.0,
        };
        store.insert_claim(&claim).await.expect("insert claim");

        let anchor = EvidenceAnchor {
            id: Uuid::new_v4(),
            repo_id: repo.id,
            claim_id: claim.id,
            identity: AnchorIdentity {
                source_kind: AnchorSource::Symbol,
                source_uri: "src/orders/service.rs".to_string(),
                subpath: "OrderService::apply".to_string(),
            },
            locator: Locator::SymbolQn {
                qn: "OrderService::apply".to_string(),
            },
            anchored_text: "fn apply(&mut self, e: Event) { self.state = e.fold(); }".to_string(),
            content_hash: "abc123".to_string(),
            context_hash: Some("ctx456".to_string()),
            alias_of: None,
            ingested_at: now.to_string(),
            source_time: None,
        };
        store
            .insert_evidence_anchor(&anchor)
            .await
            .expect("insert anchor");
        // idempotent on the unique key
        store
            .insert_evidence_anchor(&anchor)
            .await
            .expect("insert anchor again");

        let claims = store
            .claims_for_subjects(&[subject_id])
            .await
            .expect("claims for subject");
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].polarity, Some(Polarity::MustNot));
        assert_eq!(claims[0].evidence_grade, EvidenceGrade::Proven);
        assert_eq!(claims[0].read_set.len(), 1);

        let anchors = store
            .anchors_for_claims(&[claim.id])
            .await
            .expect("anchors for claims");
        assert_eq!(anchors.len(), 1, "duplicate insert must be ignored");
        assert_eq!(anchors[0].identity.subpath, "OrderService::apply");
        assert!(matches!(anchors[0].locator, Locator::SymbolQn { .. }));

        let verification = AnchorVerification {
            id: Uuid::new_v4(),
            anchor_id: anchor.id,
            checked_at: now.to_string(),
            repo_ref: "HEAD".to_string(),
            repo_commit: "9f1c2ab".to_string(),
            edit_class: EditClass::Deleted,
            freshness: Freshness::Stale,
            observed_hash: None,
            relocated_locator: None,
            similarity: None,
            detail: Some("symbol absent from index".to_string()),
        };
        store
            .insert_anchor_verification(&verification)
            .await
            .expect("insert verification");

        let latest = store
            .latest_verification(anchor.id, "9f1c2ab")
            .await
            .expect("latest verification")
            .expect("some verification");
        assert_eq!(latest.freshness, Freshness::Stale);
        assert_eq!(latest.edit_class, EditClass::Deleted);

        store
            .close_claims_for_subject("constraint", subject_id, now)
            .await
            .expect("close claims");
        let after = store
            .claims_for_subjects(&[subject_id])
            .await
            .expect("claims after close");
        assert!(after.is_empty(), "closed claims are no longer open");
    }

    #[tokio::test]
    async fn index_lane_upsert_replaces_status() {
        let store = test_store().await;
        let repo = store
            .upsert_repository("C:/repo/lanes", None)
            .await
            .expect("repo");

        store
            .upsert_index_lane(repo.id, "symbol", Some("aaa111"), "2026-09-01T09:00:00Z", "ok", None)
            .await
            .expect("first upsert");
        store
            .upsert_index_lane(
                repo.id,
                "symbol",
                Some("bbb222"),
                "2026-09-01T10:00:00Z",
                "failed",
                Some("parse error"),
            )
            .await
            .expect("second upsert");

        let lanes = store.index_lanes(repo.id).await.expect("lanes");
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].status, "failed");
        assert_eq!(lanes[0].last_ingested_commit.as_deref(), Some("bbb222"));
    }

    #[tokio::test]
    async fn stores_episode_decision_without_adr_document() {
        let store = test_store().await;
        let repo = store
            .upsert_repository("C:/repo/episode-decision", None)
            .await
            .expect("repo");

        let episode_id = Uuid::new_v4();
        store
            .insert_episode(
                &episode_id,
                repo.id,
                "meeting:storage",
                None,
                "Episode discussion",
                "2026-05-05T09:00:00Z",
                "2026-05-05T09:01:00Z",
            )
            .await
            .expect("episode");

        let decision = Decision {
            id: Uuid::new_v4(),
            title: Some("Direct episode link".to_string()),
            adr_id: None,
            episode_id: Some(episode_id),
            text: "Episode decisions are stored without ADR wrappers.".to_string(),
            source_uri: format!("episode:{}", episode_id),
            valid_from: "2026-05-05T09:01:00Z".to_string(),
            valid_to: None,
            ingested_at: "2026-05-05T09:01:00Z".to_string(),
            source_time: Some("2026-05-05T09:00:00Z".to_string()),
            confidence: 1.0,
            evidence_refs: vec![],
        };
        store.insert_decision(&decision).await.expect("decision");

        let adrs = store.list_current_adrs(repo.id).await.expect("adrs");
        assert!(
            adrs.is_empty(),
            "episode decisions should not create ADR documents"
        );

        let found = store
            .search_decisions(repo.id, "wrappers", None)
            .await
            .expect("search decisions");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].episode_id, Some(episode_id.to_string()));
        assert_eq!(found[0].adr_id, format!("episode:{}", episode_id));
        assert_eq!(found[0].title, "Direct episode link");
        assert_eq!(found[0].status, "episode");
    }
