use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use chrono::Utc;
use serde_json::Value;
use uuid::Uuid;

use super::graph::{TrajectoryGraph, TrajectoryGraphUpdater};
use super::normalizer::normalize_tool_event;
use super::storage::{
    DEFAULT_RECOMMENDER_DB_NAME, InMemoryStore, SqliteTrajectoryRepository, TrajectoryRepository,
    TrajectorySnapshot,
};
use super::types::{RawToolEvent, ToolEventOrigin, ToolEventStatus, TrajectoryEvent};

#[derive(Debug, Clone)]
struct PendingRawToolCall {
    session_id: String,
    turn_id: String,
    tool_name: String,
    input_summary: Value,
    started_at: Instant,
    origin: ToolEventOrigin,
}

#[derive(Debug, Default)]
pub struct RawEventCollector {
    pending: HashMap<String, PendingRawToolCall>,
    store: InMemoryStore,
    graph: TrajectoryGraph,
    graph_updater: TrajectoryGraphUpdater,
    sqlite: Option<SqliteTrajectoryRepository>,
    persistence_errors: Vec<String>,
}

impl RawEventCollector {
    pub fn open_at_workspace(workspace: &Path) -> Result<Self> {
        let db_path = workspace.join(".cog").join(DEFAULT_RECOMMENDER_DB_NAME);
        let sqlite = SqliteTrajectoryRepository::open(&db_path)?;
        let historical_edges = sqlite.list_trajectory_edges()?;
        let mut graph = TrajectoryGraph::default();
        graph.load_edges(historical_edges);
        Ok(Self {
            sqlite: Some(sqlite),
            graph,
            ..Self::default()
        })
    }

    #[cfg(test)]
    pub fn with_sqlite(sqlite: SqliteTrajectoryRepository) -> Self {
        Self {
            sqlite: Some(sqlite),
            ..Self::default()
        }
    }

    pub fn record_tool_call_started(
        &mut self,
        tool_call_id: impl Into<String>,
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
        tool_name: impl Into<String>,
        input_summary: Value,
    ) {
        self.record_tool_call_started_with_origin(
            tool_call_id,
            session_id,
            turn_id,
            tool_name,
            input_summary,
            ToolEventOrigin::Agent,
        );
    }

    pub fn record_tool_call_started_with_origin(
        &mut self,
        tool_call_id: impl Into<String>,
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
        tool_name: impl Into<String>,
        input_summary: Value,
        origin: ToolEventOrigin,
    ) {
        self.pending.insert(
            tool_call_id.into(),
            PendingRawToolCall {
                session_id: session_id.into(),
                turn_id: turn_id.into(),
                tool_name: tool_name.into(),
                input_summary,
                started_at: Instant::now(),
                origin,
            },
        );
    }

    pub fn record_tool_call_completed(
        &mut self,
        tool_call_id: &str,
        fallback_session_id: impl Into<String>,
        fallback_turn_id: impl Into<String>,
        fallback_tool_name: impl Into<String>,
        output_summary: impl Into<String>,
        status: ToolEventStatus,
    ) -> RawToolEvent {
        let fallback_session_id = fallback_session_id.into();
        let fallback_turn_id = fallback_turn_id.into();
        let fallback_tool_name = fallback_tool_name.into();
        let pending = self.pending.remove(tool_call_id);
        let duration_ms = pending
            .as_ref()
            .map(|pending| pending.started_at.elapsed().as_millis() as u64)
            .unwrap_or(0);

        let event = if let Some(pending) = pending {
            build_raw_event_with_origin(
                pending.session_id,
                pending.turn_id,
                pending.tool_name,
                pending.input_summary,
                output_summary,
                status,
                duration_ms,
                pending.origin,
            )
        } else {
            build_raw_event_with_origin(
                fallback_session_id,
                fallback_turn_id,
                fallback_tool_name,
                Value::Null,
                output_summary,
                status,
                duration_ms,
                ToolEventOrigin::Agent,
            )
        };

        self.store.record_raw_event(event.clone());
        if let Some(sqlite) = self.sqlite.as_ref()
            && let Err(err) = sqlite.record_raw_event(&event)
        {
            self.persistence_errors
                .push(format!("record raw event {}: {err}", event.id));
        }
        for trajectory_event in normalize_tool_event(&event) {
            let updated_edges = self
                .graph_updater
                .observe(&mut self.graph, trajectory_event.clone());
            self.store.record_trajectory_event(trajectory_event);
            if let Some(sqlite) = self.sqlite.as_ref() {
                if let Some(event) = self.store.trajectory_events.last()
                    && let Err(err) = sqlite.record_trajectory_event(event)
                {
                    self.persistence_errors
                        .push(format!("record trajectory event {}: {err}", event.id));
                }
                for edge in updated_edges {
                    if let Err(err) = sqlite.upsert_trajectory_edge(&edge) {
                        self.persistence_errors.push(format!(
                            "upsert trajectory edge {} -> {}: {err}",
                            edge.source.qualified_name, edge.target.qualified_name
                        ));
                    }
                }
            }
        }

        event
    }

    pub fn clear_pending(&mut self) {
        self.pending.clear();
    }

    pub fn clear_session_graph_context(&mut self, session_id: &str) {
        self.graph_updater.clear_session(session_id);
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn store(&self) -> &InMemoryStore {
        &self.store
    }

    pub fn graph(&self) -> &TrajectoryGraph {
        &self.graph
    }

    pub fn recent_trajectory_events(&self, limit: usize) -> Vec<TrajectoryEvent> {
        if let Some(sqlite) = self.sqlite.as_ref()
            && let Ok(events) = sqlite.list_recent_trajectory_events(limit)
        {
            return events;
        }
        let mut events = self.store.trajectory_events.clone();
        events.reverse();
        events.truncate(limit);
        events
    }

    pub fn persistence_errors(&self) -> &[String] {
        &self.persistence_errors
    }

    pub fn trajectory_snapshot(&self, limit: usize) -> TrajectorySnapshot {
        if let Some(sqlite) = self.sqlite.as_ref() {
            let raw_events = sqlite.list_recent_raw_events(limit);
            let trajectory_events = sqlite.list_recent_trajectory_events(limit);
            let trajectory_edges = sqlite.list_trajectory_edges();
            if let (Ok(raw_events), Ok(trajectory_events), Ok(trajectory_edges)) =
                (raw_events, trajectory_events, trajectory_edges)
            {
                return TrajectorySnapshot {
                    raw_events,
                    trajectory_events,
                    trajectory_edges,
                    persistence_errors: self.persistence_errors.clone(),
                };
            }
        }

        let mut raw_events = self.store.raw_events.clone();
        raw_events.reverse();
        raw_events.truncate(limit);
        let mut trajectory_events = self.store.trajectory_events.clone();
        trajectory_events.reverse();
        trajectory_events.truncate(limit);
        TrajectorySnapshot {
            raw_events,
            trajectory_events,
            trajectory_edges: self.graph.edges().into_iter().cloned().collect(),
            persistence_errors: self.persistence_errors.clone(),
        }
    }
}

pub fn build_raw_event(
    session_id: impl Into<String>,
    turn_id: impl Into<String>,
    tool_name: impl Into<String>,
    input_summary: Value,
    output_summary: impl Into<String>,
    status: ToolEventStatus,
    duration_ms: u64,
) -> RawToolEvent {
    build_raw_event_with_origin(
        session_id,
        turn_id,
        tool_name,
        input_summary,
        output_summary,
        status,
        duration_ms,
        ToolEventOrigin::Agent,
    )
}

pub fn build_raw_event_with_origin(
    session_id: impl Into<String>,
    turn_id: impl Into<String>,
    tool_name: impl Into<String>,
    input_summary: Value,
    output_summary: impl Into<String>,
    status: ToolEventStatus,
    duration_ms: u64,
    origin: ToolEventOrigin,
) -> RawToolEvent {
    RawToolEvent {
        id: Uuid::new_v4().to_string(),
        session_id: session_id.into(),
        turn_id: turn_id.into(),
        ts: Utc::now(),
        tool_name: tool_name.into(),
        input_summary,
        output_summary: output_summary.into(),
        status,
        duration_ms,
        origin,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::cog_recommender::storage::SqliteTrajectoryRepository;
    use crate::cog_recommender::types::{ToolEventOrigin, TrajectoryKind};

    #[test]
    fn build_raw_event_populates_identity_and_status() {
        let event = build_raw_event(
            "session",
            "turn",
            "read_file",
            json!({"path": "src/lib.rs"}),
            "ok",
            ToolEventStatus::Success,
            7,
        );

        assert_eq!(event.session_id, "session");
        assert_eq!(event.turn_id, "turn");
        assert_eq!(event.tool_name, "read_file");
        assert_eq!(event.duration_ms, 7);
        assert!(!event.id.is_empty());
    }

    #[test]
    fn collector_records_raw_and_normalized_events() {
        let mut collector = RawEventCollector::default();
        collector.record_tool_call_started(
            "call-1",
            "session",
            "turn",
            "read_file",
            json!({"path": "src/lib.rs"}),
        );

        let raw = collector.record_tool_call_completed(
            "call-1",
            "fallback-session",
            "fallback-turn",
            "unknown",
            "ok",
            ToolEventStatus::Success,
        );

        assert_eq!(raw.session_id, "session");
        assert_eq!(raw.turn_id, "turn");
        assert_eq!(raw.tool_name, "read_file");
        assert_eq!(collector.pending_count(), 0);
        assert_eq!(collector.store().raw_events.len(), 1);
        assert_eq!(collector.store().trajectory_events.len(), 1);
        assert_eq!(collector.graph().edge_count(), 0);
        assert_eq!(
            collector.store().trajectory_events[0].kind,
            TrajectoryKind::ReadEntity
        );
        assert_eq!(
            collector.store().trajectory_events[0].file_path.as_deref(),
            Some("src/lib.rs")
        );
    }

    #[test]
    fn collector_keeps_internal_raw_event_but_skips_trajectory() {
        let mut collector = RawEventCollector::default();
        collector.record_tool_call_started_with_origin(
            "call-cog",
            "session",
            "turn",
            "exec_shell",
            json!({"cmd": "cog impact auth::login"}),
            ToolEventOrigin::RecommenderInternal,
        );

        let raw = collector.record_tool_call_completed(
            "call-cog",
            "session",
            "turn",
            "exec_shell",
            "ok",
            ToolEventStatus::Success,
        );

        assert_eq!(raw.origin, ToolEventOrigin::RecommenderInternal);
        assert_eq!(collector.store().raw_events.len(), 1);
        assert!(collector.store().trajectory_events.is_empty());
        assert_eq!(collector.graph().edge_count(), 0);
    }

    #[test]
    fn collector_records_error_signal_for_failed_tool() {
        let mut collector = RawEventCollector::default();
        collector.record_tool_call_started(
            "call-err",
            "session",
            "turn",
            "exec_shell",
            json!({"command": "cargo test"}),
        );

        collector.record_tool_call_completed(
            "call-err",
            "session",
            "turn",
            "exec_shell",
            "test failed",
            ToolEventStatus::Error,
        );

        let events = &collector.store().trajectory_events;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, TrajectoryKind::TestEntity);
        assert_eq!(events[1].kind, TrajectoryKind::ErrorSignal);
    }

    #[test]
    fn collector_preserves_orphan_completion() {
        let mut collector = RawEventCollector::default();

        let raw = collector.record_tool_call_completed(
            "missing-start",
            "session",
            "turn",
            "grep_files",
            "ok",
            ToolEventStatus::Success,
        );

        assert_eq!(raw.session_id, "session");
        assert_eq!(raw.turn_id, "turn");
        assert_eq!(raw.tool_name, "grep_files");
        assert_eq!(raw.input_summary, Value::Null);
        assert_eq!(raw.duration_ms, 0);
        assert_eq!(collector.store().raw_events.len(), 1);
        assert_eq!(
            collector.store().trajectory_events[0].kind,
            TrajectoryKind::SearchEntity
        );
    }

    #[test]
    fn collector_updates_trajectory_graph_from_event_sequence() {
        let mut collector = RawEventCollector::default();
        collector.record_tool_call_started(
            "read-call",
            "session",
            "turn",
            "read_file",
            json!({"path": "src/config.rs"}),
        );
        collector.record_tool_call_completed(
            "read-call",
            "session",
            "turn",
            "read_file",
            "ok",
            ToolEventStatus::Success,
        );

        collector.record_tool_call_started(
            "edit-call",
            "session",
            "turn",
            "apply_patch",
            json!({"path": "src/main.rs"}),
        );
        collector.record_tool_call_completed(
            "edit-call",
            "session",
            "turn",
            "apply_patch",
            "patched",
            ToolEventStatus::Success,
        );

        assert!(collector.graph().edge_count() >= 1);
    }

    #[test]
    fn collector_persists_raw_trajectory_and_edges_to_sqlite() {
        let sqlite = SqliteTrajectoryRepository::open_in_memory().expect("sqlite");
        let mut collector = RawEventCollector::with_sqlite(sqlite);
        collector.record_tool_call_started(
            "read-call",
            "session",
            "turn",
            "read_file",
            json!({"path": "src/config.rs"}),
        );
        collector.record_tool_call_completed(
            "read-call",
            "session",
            "turn",
            "read_file",
            "ok",
            ToolEventStatus::Success,
        );
        collector.record_tool_call_started(
            "edit-call",
            "session",
            "turn",
            "apply_patch",
            json!({"path": "src/main.rs"}),
        );
        collector.record_tool_call_completed(
            "edit-call",
            "session",
            "turn",
            "apply_patch",
            "patched",
            ToolEventStatus::Success,
        );

        let sqlite = collector.sqlite.as_ref().expect("sqlite");
        assert_eq!(sqlite.count_rows("tool_events").unwrap(), 2);
        assert_eq!(sqlite.count_rows("trajectory_events").unwrap(), 2);
        assert!(sqlite.count_rows("trajectory_edges").unwrap() >= 1);
        assert_eq!(collector.persistence_errors(), &[] as &[String]);
    }

    #[test]
    fn collector_rebuilds_graph_from_persisted_edges() {
        let workspace = tempfile::tempdir().expect("tempdir");
        {
            let mut collector =
                RawEventCollector::open_at_workspace(workspace.path()).expect("collector");
            collector.record_tool_call_started(
                "read-call",
                "session",
                "turn",
                "read_file",
                json!({"path": "src/config.rs"}),
            );
            collector.record_tool_call_completed(
                "read-call",
                "session",
                "turn",
                "read_file",
                "ok",
                ToolEventStatus::Success,
            );
            collector.record_tool_call_started(
                "edit-call",
                "session",
                "turn",
                "apply_patch",
                json!({"path": "src/main.rs"}),
            );
            collector.record_tool_call_completed(
                "edit-call",
                "session",
                "turn",
                "apply_patch",
                "patched",
                ToolEventStatus::Success,
            );
            assert!(collector.graph().edge_count() >= 1);
        }

        let rebuilt = RawEventCollector::open_at_workspace(workspace.path()).expect("rebuilt");
        assert!(rebuilt.graph().edge_count() >= 1);
    }

    #[test]
    fn collector_snapshot_returns_persisted_trajectory_data() {
        let sqlite = SqliteTrajectoryRepository::open_in_memory().expect("sqlite");
        let mut collector = RawEventCollector::with_sqlite(sqlite);
        collector.record_tool_call_started(
            "read-call",
            "session",
            "turn",
            "read_file",
            json!({"path": "src/config.rs"}),
        );
        collector.record_tool_call_completed(
            "read-call",
            "session",
            "turn",
            "read_file",
            "ok",
            ToolEventStatus::Success,
        );
        collector.record_tool_call_started(
            "edit-call",
            "session",
            "turn",
            "apply_patch",
            json!({"path": "src/main.rs"}),
        );
        collector.record_tool_call_completed(
            "edit-call",
            "session",
            "turn",
            "apply_patch",
            "patched",
            ToolEventStatus::Success,
        );

        let snapshot = collector.trajectory_snapshot(10);

        assert_eq!(snapshot.raw_events.len(), 2);
        assert_eq!(snapshot.trajectory_events.len(), 2);
        assert!(!snapshot.trajectory_edges.is_empty());
        assert!(snapshot.persistence_errors.is_empty());
    }

    #[test]
    fn collector_exposes_recent_trajectory_events_for_recommendation_context() {
        let mut collector = RawEventCollector::default();
        collector.record_tool_call_started(
            "read-call",
            "session",
            "turn",
            "read_file",
            json!({"path": "src/config.rs"}),
        );
        collector.record_tool_call_completed(
            "read-call",
            "session",
            "turn",
            "read_file",
            "ok",
            ToolEventStatus::Success,
        );
        collector.record_tool_call_started(
            "edit-call",
            "session",
            "turn",
            "apply_patch",
            json!({"path": "src/main.rs"}),
        );
        collector.record_tool_call_completed(
            "edit-call",
            "session",
            "turn",
            "apply_patch",
            "patched",
            ToolEventStatus::Success,
        );

        let recent = collector.recent_trajectory_events(1);

        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].kind, TrajectoryKind::EditEntity);
    }
}
