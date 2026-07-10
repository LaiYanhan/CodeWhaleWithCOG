use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::OptionalExtension;
use rusqlite::{Connection, Row, params};
use serde::{Serialize, de::DeserializeOwned};

use super::config::RecommenderConfig;
use super::graph::EdgeStats;
use super::types::{
    EntityRef, EvidenceSource, LineRange, RawToolEvent, Recommendation, RecommendationFeedback,
    StoredRecommendation, ToolEventOrigin, ToolEventStatus, TrajectoryEvent, TrajectoryKind,
};

pub const DEFAULT_RECOMMENDER_DB_NAME: &str = "recommender.db";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tool_events (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    ts TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    input_json TEXT NOT NULL,
    output_summary TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('success', 'error')),
    duration_ms INTEGER NOT NULL,
    origin TEXT NOT NULL DEFAULT 'agent' CHECK(origin IN ('agent', 'user', 'system', 'recommender_internal'))
);

CREATE TABLE IF NOT EXISTS trajectory_events (
    id TEXT PRIMARY KEY,
    raw_event_id TEXT NOT NULL REFERENCES tool_events(id) ON DELETE CASCADE,
    session_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    entity_id TEXT,
    entity_json TEXT,
    file_path TEXT,
    line_range_json TEXT,
    confidence REAL NOT NULL,
    payload_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS trajectory_edges (
    source_entity TEXT NOT NULL,
    source_json TEXT NOT NULL,
    target_entity TEXT NOT NULL,
    target_json TEXT NOT NULL,
    edge_type TEXT NOT NULL,
    weight REAL NOT NULL,
    count INTEGER NOT NULL,
    reason TEXT NOT NULL,
    last_seen_ts TEXT NOT NULL,
    PRIMARY KEY (source_entity, target_entity, edge_type)
);

CREATE TABLE IF NOT EXISTS recommendations (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    trigger_event_ids_json TEXT NOT NULL,
    target_entity TEXT NOT NULL,
    entity_json TEXT NOT NULL,
    suggested_action TEXT NOT NULL,
    score REAL NOT NULL,
    display_text TEXT NOT NULL,
    recommendation_json TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('pending', 'exposed', 'completed', 'expired')),
    created_at TEXT NOT NULL,
    last_triggered_at TEXT NOT NULL,
    exposed_at TEXT,
    expires_at TEXT NOT NULL,
    trigger_tool_index INTEGER NOT NULL,
    exposed_turn_index INTEGER
);

CREATE TABLE IF NOT EXISTS recommendation_evidence (
    id TEXT PRIMARY KEY,
    recommendation_id TEXT NOT NULL REFERENCES recommendations(id) ON DELETE CASCADE,
    source TEXT NOT NULL,
    weight REAL NOT NULL,
    target_entity TEXT NOT NULL,
    reason TEXT NOT NULL,
    payload_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS recommendation_feedback (
    id TEXT PRIMARY KEY,
    recommendation_id TEXT NOT NULL REFERENCES recommendations(id) ON DELETE CASCADE,
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    event_id TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS recommender_runtime_config (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    config_json TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_tool_events_session ON tool_events(session_id);
CREATE INDEX IF NOT EXISTS idx_tool_events_turn ON tool_events(turn_id);
CREATE INDEX IF NOT EXISTS idx_trajectory_events_session ON trajectory_events(session_id);
CREATE INDEX IF NOT EXISTS idx_trajectory_events_raw ON trajectory_events(raw_event_id);
CREATE INDEX IF NOT EXISTS idx_trajectory_edges_source ON trajectory_edges(source_entity);
CREATE INDEX IF NOT EXISTS idx_trajectory_edges_target ON trajectory_edges(target_entity);
CREATE INDEX IF NOT EXISTS idx_recommendations_session_status
    ON recommendations(session_id, status);
CREATE INDEX IF NOT EXISTS idx_recommendation_feedback_recommendation
    ON recommendation_feedback(recommendation_id);
"#;

#[derive(Debug, Default)]
pub struct InMemoryStore {
    pub raw_events: Vec<RawToolEvent>,
    pub trajectory_events: Vec<TrajectoryEvent>,
    pub recommendations: Vec<Recommendation>,
}

impl InMemoryStore {
    pub fn record_raw_event(&mut self, event: RawToolEvent) {
        self.raw_events.push(event);
    }

    pub fn record_trajectory_event(&mut self, event: TrajectoryEvent) {
        self.trajectory_events.push(event);
    }

    pub fn record_recommendations(&mut self, recommendations: Vec<Recommendation>) {
        self.recommendations.extend(recommendations);
    }
}

pub trait TrajectoryRepository {
    fn record_raw_event(&self, event: &RawToolEvent) -> Result<()>;
    fn record_trajectory_event(&self, event: &TrajectoryEvent) -> Result<()>;
    fn upsert_trajectory_edge(&self, edge: &EdgeStats) -> Result<()>;
    fn list_recent_raw_events(&self, limit: usize) -> Result<Vec<RawToolEvent>>;
    fn list_recent_trajectory_events(&self, limit: usize) -> Result<Vec<TrajectoryEvent>>;
    fn list_trajectory_edges(&self) -> Result<Vec<EdgeStats>>;
}

#[derive(Debug, Clone, Serialize)]
pub struct TrajectorySnapshot {
    pub raw_events: Vec<RawToolEvent>,
    pub trajectory_events: Vec<TrajectoryEvent>,
    pub trajectory_edges: Vec<EdgeStats>,
    pub persistence_errors: Vec<String>,
}

#[derive(Debug)]
pub struct SqliteTrajectoryRepository {
    conn: Connection,
}

impl SqliteTrajectoryRepository {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open sqlite db: {}", path.display()))?;
        configure_connection(&conn)?;
        Ok(Self { conn })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("failed to open in-memory sqlite db")?;
        configure_connection(&conn)?;
        Ok(Self { conn })
    }

    #[cfg(test)]
    pub fn count_rows(&self, table: &str) -> Result<u64> {
        let sql = match table {
            "tool_events" => "SELECT COUNT(*) FROM tool_events",
            "trajectory_events" => "SELECT COUNT(*) FROM trajectory_events",
            "trajectory_edges" => "SELECT COUNT(*) FROM trajectory_edges",
            "recommendations" => "SELECT COUNT(*) FROM recommendations",
            "recommendation_evidence" => "SELECT COUNT(*) FROM recommendation_evidence",
            "recommendation_feedback" => "SELECT COUNT(*) FROM recommendation_feedback",
            _ => anyhow::bail!("unknown table: {table}"),
        };
        let count: i64 = self.conn.query_row(sql, [], |row| row.get(0))?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    #[cfg(test)]
    pub fn edge_count_for_type(&self, edge_type: &str) -> Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM trajectory_edges WHERE edge_type = ?1",
            [edge_type],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    #[cfg(test)]
    pub fn get_edge_count(
        &self,
        source_entity: &str,
        target_entity: &str,
        edge_type: &str,
    ) -> Result<Option<u32>> {
        let count = self
            .conn
            .query_row(
                "SELECT count FROM trajectory_edges
                 WHERE source_entity = ?1 AND target_entity = ?2 AND edge_type = ?3",
                params![source_entity, target_entity, edge_type],
                |row| row.get(0),
            )
            .optional()?;
        Ok(count)
    }

    #[cfg(test)]
    pub fn count_edges(&self) -> Result<u64> {
        self.count_rows("trajectory_edges")
    }

    pub fn upsert_recommendation(&self, value: &StoredRecommendation) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO recommendations
             (id, session_id, turn_id, trigger_event_ids_json, target_entity, entity_json,
              suggested_action, score, display_text, recommendation_json, status, created_at,
              last_triggered_at, exposed_at, expires_at, trigger_tool_index, exposed_turn_index)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
             ON CONFLICT(id) DO UPDATE SET
               trigger_event_ids_json = excluded.trigger_event_ids_json,
               score = excluded.score,
               display_text = excluded.display_text,
               recommendation_json = excluded.recommendation_json,
               status = excluded.status,
               last_triggered_at = excluded.last_triggered_at,
               exposed_at = excluded.exposed_at,
               expires_at = excluded.expires_at,
               exposed_turn_index = excluded.exposed_turn_index",
            params![
                value.id,
                value.session_id,
                value.turn_id,
                to_json(&value.trigger_event_ids)?,
                value.recommendation.entity.qualified_name,
                to_json(&value.recommendation.entity)?,
                to_enum_text(&value.recommendation.suggested_action)?,
                value.recommendation.score,
                value.recommendation.display_text,
                to_json(&value.recommendation)?,
                to_enum_text(&value.status)?,
                value.created_at.to_rfc3339(),
                value.last_triggered_at.to_rfc3339(),
                value.exposed_at.map(|ts| ts.to_rfc3339()),
                value.expires_at.to_rfc3339(),
                i64::try_from(value.trigger_tool_index).unwrap_or(i64::MAX),
                value
                    .exposed_turn_index
                    .map(|index| i64::try_from(index).unwrap_or(i64::MAX)),
            ],
        )?;
        tx.execute(
            "DELETE FROM recommendation_evidence WHERE recommendation_id = ?1",
            [&value.id],
        )?;
        for (index, evidence) in value.recommendation.evidence.iter().enumerate() {
            tx.execute(
                "INSERT INTO recommendation_evidence
                 (id, recommendation_id, source, weight, target_entity, reason, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    format!("{}:{index}", value.id),
                    value.id,
                    to_enum_text(&evidence.source)?,
                    evidence.weight,
                    evidence.target.qualified_name,
                    evidence.reason,
                    to_json(&evidence.payload)?,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn record_recommendation_feedback(&self, value: &RecommendationFeedback) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO recommendation_feedback
             (id, recommendation_id, session_id, turn_id, kind, event_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                value.id,
                value.recommendation_id,
                value.session_id,
                value.turn_id,
                to_enum_text(&value.kind)?,
                value.event_id,
                value.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn load_runtime_config(&self) -> Result<RecommenderConfig> {
        let value = self
            .conn
            .query_row(
                "SELECT config_json FROM recommender_runtime_config WHERE id = 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        value
            .map(|json| from_json(&json))
            .unwrap_or_else(|| Ok(RecommenderConfig::default()))
    }

    pub fn save_runtime_config(&self, config: &RecommenderConfig) -> Result<()> {
        self.conn.execute(
            "INSERT INTO recommender_runtime_config (id, config_json, updated_at)
             VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET
               config_json = excluded.config_json,
               updated_at = excluded.updated_at",
            params![to_json(config)?, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }
}

impl TrajectoryRepository for SqliteTrajectoryRepository {
    fn record_raw_event(&self, event: &RawToolEvent) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO tool_events
             (id, session_id, turn_id, ts, tool_name, input_json, output_summary, status, duration_ms, origin)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                event.id,
                event.session_id,
                event.turn_id,
                event.ts.to_rfc3339(),
                event.tool_name,
                to_json(&event.input_summary)?,
                event.output_summary,
                to_enum_text(&event.status)?,
                i64::try_from(event.duration_ms).unwrap_or(i64::MAX),
                to_enum_text(&event.origin)?,
            ],
        )?;
        Ok(())
    }

    fn record_trajectory_event(&self, event: &TrajectoryEvent) -> Result<()> {
        let entity_id = event
            .entity_ref
            .as_ref()
            .and_then(|entity| entity.cog_entity_id.clone());
        self.conn.execute(
            "INSERT OR REPLACE INTO trajectory_events
             (id, raw_event_id, session_id, kind, entity_id, entity_json, file_path,
              line_range_json, confidence, payload_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                event.id,
                event.raw_event_id,
                event.session_id,
                to_enum_text(&event.kind)?,
                entity_id,
                to_json(&event.entity_ref)?,
                event.file_path,
                to_json(&event.line_range)?,
                event.confidence,
                to_json(&event.payload)?,
            ],
        )?;
        Ok(())
    }

    fn upsert_trajectory_edge(&self, edge: &EdgeStats) -> Result<()> {
        self.conn.execute(
            "INSERT INTO trajectory_edges
             (source_entity, source_json, target_entity, target_json, edge_type,
              weight, count, reason, last_seen_ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(source_entity, target_entity, edge_type) DO UPDATE SET
                source_json = excluded.source_json,
                target_json = excluded.target_json,
                weight = excluded.weight,
                count = excluded.count,
                reason = excluded.reason,
                last_seen_ts = excluded.last_seen_ts",
            params![
                edge.source.qualified_name,
                to_json(&edge.source)?,
                edge.target.qualified_name,
                to_json(&edge.target)?,
                to_enum_text(&edge.edge_type)?,
                edge.weight,
                edge.count,
                edge.reason,
                edge.last_seen_ts.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    fn list_recent_raw_events(&self, limit: usize) -> Result<Vec<RawToolEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, turn_id, ts, tool_name, input_json,
                    output_summary, status, duration_ms, origin
             FROM tool_events
             ORDER BY ts DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit_to_i64(limit)], map_raw_event_row)?;
        collect_rows(rows)
    }

    fn list_recent_trajectory_events(&self, limit: usize) -> Result<Vec<TrajectoryEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, raw_event_id, session_id, kind, entity_json, file_path,
                    line_range_json, confidence, payload_json
             FROM trajectory_events
             ORDER BY rowid DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit_to_i64(limit)], map_trajectory_event_row)?;
        collect_rows(rows)
    }

    fn list_trajectory_edges(&self) -> Result<Vec<EdgeStats>> {
        let mut stmt = self.conn.prepare(
            "SELECT source_json, target_json, edge_type, weight, count, reason, last_seen_ts
             FROM trajectory_edges
             ORDER BY last_seen_ts DESC",
        )?;
        let rows = stmt.query_map([], map_edge_row)?;
        collect_rows(rows)
    }
}

fn configure_connection(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")
        .context("failed to configure sqlite pragmas")?;
    conn.execute_batch(SCHEMA)
        .context("failed to initialize recommender sqlite schema")?;
    ensure_tool_events_origin_column(conn)?;
    Ok(())
}

fn ensure_tool_events_origin_column(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(tool_events)")?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for column in columns {
        if column? == "origin" {
            return Ok(());
        }
    }
    conn.execute(
        "ALTER TABLE tool_events
         ADD COLUMN origin TEXT NOT NULL DEFAULT 'agent'
         CHECK(origin IN ('agent', 'user', 'system', 'recommender_internal'))",
        [],
    )
    .context("failed to migrate tool_events.origin")?;
    Ok(())
}

fn to_json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).context("failed to serialize recommender value")
}

fn to_enum_text<T: Serialize>(value: &T) -> Result<String> {
    let value = serde_json::to_value(value).context("failed to serialize recommender enum")?;
    value
        .as_str()
        .map(ToOwned::to_owned)
        .context("serialized enum was not a string")
}

fn from_json<T: DeserializeOwned>(value: &str) -> Result<T> {
    serde_json::from_str(value).context("failed to deserialize recommender value")
}

fn enum_from_text<T: DeserializeOwned>(value: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(value.to_string()))
        .context("failed to deserialize recommender enum")
}

fn parse_ts(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .with_context(|| format!("invalid recommender timestamp: {value}"))
}

fn limit_to_i64(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&Row<'_>) -> rusqlite::Result<Result<T>>>,
) -> Result<Vec<T>> {
    let mut values = Vec::new();
    for row in rows {
        values.push(row??);
    }
    Ok(values)
}

fn map_raw_event_row(row: &Row<'_>) -> rusqlite::Result<Result<RawToolEvent>> {
    let ts: String = row.get(3)?;
    let input_json: String = row.get(5)?;
    let status: String = row.get(7)?;
    let duration_ms: i64 = row.get(8)?;
    let origin: String = row.get(9)?;
    Ok((|| {
        Ok(RawToolEvent {
            id: row.get(0)?,
            session_id: row.get(1)?,
            turn_id: row.get(2)?,
            ts: parse_ts(&ts)?,
            tool_name: row.get(4)?,
            input_summary: from_json(&input_json)?,
            output_summary: row.get(6)?,
            status: enum_from_text::<ToolEventStatus>(&status)?,
            duration_ms: u64::try_from(duration_ms).unwrap_or(0),
            origin: enum_from_text::<ToolEventOrigin>(&origin)?,
        })
    })())
}

fn map_trajectory_event_row(row: &Row<'_>) -> rusqlite::Result<Result<TrajectoryEvent>> {
    let kind: String = row.get(3)?;
    let entity_json: Option<String> = row.get(4)?;
    let line_range_json: Option<String> = row.get(6)?;
    let payload_json: String = row.get(8)?;
    Ok((|| {
        Ok(TrajectoryEvent {
            id: row.get(0)?,
            raw_event_id: row.get(1)?,
            session_id: row.get(2)?,
            kind: enum_from_text::<TrajectoryKind>(&kind)?,
            entity_ref: match entity_json {
                Some(value) => from_json::<Option<EntityRef>>(&value)?,
                None => None,
            },
            file_path: row.get(5)?,
            line_range: match line_range_json {
                Some(value) => from_json::<Option<LineRange>>(&value)?,
                None => None,
            },
            confidence: row.get(7)?,
            payload: from_json(&payload_json)?,
        })
    })())
}

fn map_edge_row(row: &Row<'_>) -> rusqlite::Result<Result<EdgeStats>> {
    let source_json: String = row.get(0)?;
    let target_json: String = row.get(1)?;
    let edge_type: String = row.get(2)?;
    let count: i64 = row.get(4)?;
    let last_seen_ts: String = row.get(6)?;
    Ok((|| {
        Ok(EdgeStats {
            source: from_json(&source_json)?,
            target: from_json(&target_json)?,
            edge_type: enum_from_text::<EvidenceSource>(&edge_type)?,
            weight: row.get(3)?,
            count: u32::try_from(count).unwrap_or(u32::MAX),
            reason: row.get(5)?,
            last_seen_ts: parse_ts(&last_seen_ts)?,
        })
    })())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;

    use super::*;
    use crate::cog_recommender::graph::EdgeStats;
    use crate::cog_recommender::types::{
        EntityRef, Evidence, EvidenceSource, RawToolEvent, Recommendation,
        RecommendationFeedbackKind, RecommendationStatus, StoredRecommendation, SuggestedAction,
        ToolEventOrigin, ToolEventStatus, TrajectoryEvent, TrajectoryKind,
    };

    #[test]
    fn sqlite_repository_records_raw_and_trajectory_events() {
        let repo = SqliteTrajectoryRepository::open_in_memory().expect("repo");
        let raw = RawToolEvent {
            id: "raw-1".into(),
            session_id: "session".into(),
            turn_id: "turn".into(),
            ts: Utc::now(),
            tool_name: "read_file".into(),
            input_summary: json!({"path": "src/lib.rs"}),
            output_summary: "ok".into(),
            status: ToolEventStatus::Success,
            duration_ms: 10,
            origin: ToolEventOrigin::Agent,
        };
        let trajectory = TrajectoryEvent {
            id: "trajectory-1".into(),
            raw_event_id: raw.id.clone(),
            session_id: raw.session_id.clone(),
            kind: TrajectoryKind::ReadEntity,
            entity_ref: None,
            file_path: Some("src/lib.rs".into()),
            line_range: None,
            payload: raw.input_summary.clone(),
            confidence: 0.5,
        };

        repo.record_raw_event(&raw).expect("raw insert");
        repo.record_trajectory_event(&trajectory)
            .expect("trajectory insert");

        assert_eq!(repo.count_rows("tool_events").unwrap(), 1);
        assert_eq!(repo.count_rows("trajectory_events").unwrap(), 1);
    }

    #[test]
    fn sqlite_repository_upserts_trajectory_edges() {
        let repo = SqliteTrajectoryRepository::open_in_memory().expect("repo");
        let edge = EdgeStats {
            source: EntityRef::new("A"),
            target: EntityRef::new("B"),
            edge_type: EvidenceSource::ReadBeforeEdit,
            weight: 0.2,
            count: 1,
            reason: "read before edit".into(),
            last_seen_ts: Utc::now(),
        };

        repo.upsert_trajectory_edge(&edge).expect("edge insert");
        let mut updated = edge.clone();
        updated.weight = 0.4;
        updated.count = 2;
        repo.upsert_trajectory_edge(&updated).expect("edge update");

        assert_eq!(repo.count_rows("trajectory_edges").unwrap(), 1);
        assert_eq!(
            repo.get_edge_count("A", "B", "read_before_edit").unwrap(),
            Some(2)
        );
    }

    #[test]
    fn sqlite_repository_persists_recommendations_feedback_and_config() {
        let repo = SqliteTrajectoryRepository::open_in_memory().expect("repo");
        let now = Utc::now();
        let entity = EntityRef::new("inventory::api::get_stock");
        let stored = StoredRecommendation {
            id: "recommendation-1".into(),
            session_id: "session".into(),
            turn_id: "turn".into(),
            trigger_event_ids: vec!["event-1".into()],
            recommendation: Recommendation {
                entity: entity.clone(),
                score: 0.8,
                evidence: vec![Evidence::new(
                    EvidenceSource::CogImpact,
                    entity,
                    0.8,
                    "affected caller",
                )],
                suggested_action: SuggestedAction::Read,
                display_text: "Read affected caller".into(),
            },
            status: RecommendationStatus::Pending,
            created_at: now,
            last_triggered_at: now,
            exposed_at: None,
            expires_at: now + chrono::Duration::minutes(15),
            trigger_tool_index: 1,
            exposed_turn_index: None,
        };
        repo.upsert_recommendation(&stored).expect("recommendation");
        repo.record_recommendation_feedback(&RecommendationFeedback {
            id: "feedback-1".into(),
            recommendation_id: stored.id.clone(),
            session_id: stored.session_id.clone(),
            turn_id: stored.turn_id.clone(),
            kind: RecommendationFeedbackKind::Exposed,
            event_id: None,
            created_at: now,
        })
        .expect("feedback");

        let mut config = RecommenderConfig::default();
        config.max_recommendations = 3;
        repo.save_runtime_config(&config).expect("save config");

        assert_eq!(repo.count_rows("recommendations").unwrap(), 1);
        assert_eq!(repo.count_rows("recommendation_evidence").unwrap(), 1);
        assert_eq!(repo.count_rows("recommendation_feedback").unwrap(), 1);
        assert_eq!(repo.load_runtime_config().unwrap().max_recommendations, 3);
    }
}
