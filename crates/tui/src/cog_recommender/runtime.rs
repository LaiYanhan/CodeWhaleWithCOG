use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;

use chrono::{Duration, Utc};
use rusqlite::Connection;
use serde_json::json;
use uuid::Uuid;

use super::cog_adapter::{CliCogAdapter, CogAdapter};
use super::collector::RawEventCollector;
use super::config::RecommenderConfig;
use super::feedback::render_repository_recommendations;
use super::graph::{TrajectoryGraph, TrajectoryGraphUpdater};
use super::recommendation_summary::RecommendationSummaryStore;
use super::recommender::Recommender;
use super::resolver::resolve_event_entity;
use super::storage::{
    DEFAULT_RECOMMENDER_DB_NAME, SqliteTrajectoryRepository, TrajectoryRepository,
};
use super::types::{
    EntityChange, EntityChangeKind, EntityKind, EntityRef, Evidence, EvidenceSource,
    Recommendation, RecommendationFeedback, RecommendationFeedbackKind, RecommendationInjection,
    RecommendationStatus, StoredRecommendation, SuggestedAction, ToolEventStatus, TrajectoryEvent,
    TrajectoryKind,
};

const FEEDBACK_TOOL_WINDOW: u64 = 10;
const FEEDBACK_TURN_WINDOW: u64 = 2;
const FEEDBACK_MINUTES: i64 = 15;

#[derive(Debug, Default)]
pub struct PendingRecommendationQueue {
    records: Vec<StoredRecommendation>,
    injections_by_turn: HashMap<(String, String), usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRecommendationContext {
    pub injection_id: String,
    pub text: String,
}

impl PendingRecommendationQueue {
    pub fn enqueue(
        &mut self,
        session_id: &str,
        turn_id: &str,
        turn_index: u64,
        tool_index: u64,
        trigger_event_id: &str,
        recommendations: Vec<Recommendation>,
        repository: Option<&SqliteTrajectoryRepository>,
    ) {
        let now = Utc::now();
        for recommendation in dedupe_recommendations(recommendations) {
            let existing = self.records.iter_mut().find(|record| {
                record.session_id == session_id
                    && record.recommendation.entity.qualified_name
                        == recommendation.entity.qualified_name
                    && record.recommendation.suggested_action == recommendation.suggested_action
                    && matches!(
                        record.status,
                        RecommendationStatus::Pending | RecommendationStatus::Exposed
                    )
            });
            let record = if let Some(record) = existing {
                if !record
                    .trigger_event_ids
                    .iter()
                    .any(|id| id == trigger_event_id)
                {
                    record.trigger_event_ids.push(trigger_event_id.to_string());
                }
                record.recommendation.score = record.recommendation.score.max(recommendation.score);
                merge_evidence(&mut record.recommendation.evidence, recommendation.evidence);
                record.last_triggered_at = now;
                record.expires_at = now + Duration::minutes(FEEDBACK_MINUTES);
                record
            } else {
                self.records.push(StoredRecommendation {
                    id: Uuid::new_v4().to_string(),
                    session_id: session_id.to_string(),
                    turn_id: turn_id.to_string(),
                    trigger_event_ids: vec![trigger_event_id.to_string()],
                    recommendation,
                    status: RecommendationStatus::Pending,
                    created_at: now,
                    last_triggered_at: now,
                    exposed_at: None,
                    expires_at: now + Duration::minutes(FEEDBACK_MINUTES),
                    trigger_tool_index: tool_index,
                    exposed_turn_index: None,
                });
                self.records.last_mut().expect("record just inserted")
            };
            let _ = turn_index;
            persist(repository, record);
        }
    }

    pub fn render_for_next_request(
        &mut self,
        session_id: &str,
        turn_id: &str,
        turn_index: u64,
        tool_index: u64,
        config: &RecommenderConfig,
        repository: Option<&SqliteTrajectoryRepository>,
    ) -> Option<RuntimeRecommendationContext> {
        if !config.enabled {
            return None;
        }
        self.expire(session_id, turn_index, tool_index, repository);
        let injection_key = (session_id.to_string(), turn_id.to_string());
        if self
            .injections_by_turn
            .get(&injection_key)
            .copied()
            .unwrap_or(0)
            >= config.max_injections_per_turn
        {
            return None;
        }

        let mut selected = self
            .records
            .iter_mut()
            .filter(|record| {
                record.session_id == session_id
                    && record.status == RecommendationStatus::Pending
                    && record.recommendation.score >= config.min_injection_score
            })
            .collect::<Vec<_>>();
        selected.sort_by(|left, right| {
            right
                .recommendation
                .score
                .partial_cmp(&left.recommendation.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        selected.truncate(config.max_recommendations);
        if selected.is_empty() {
            return None;
        }

        let rendered =
            render_repository_recommendations(selected.iter().map(|record| &**record), config)?;
        let exposed_ids = rendered.recommendation_ids.clone();
        for record in selected
            .iter_mut()
            .filter(|record| exposed_ids.iter().any(|id| id == &record.id))
        {
            record.status = RecommendationStatus::Exposed;
            record.exposed_at = Some(Utc::now());
            record.exposed_turn_index = Some(turn_index);
            persist(repository, record);
        }
        *self.injections_by_turn.entry(injection_key).or_insert(0) += 1;
        for recommendation_id in exposed_ids {
            record_feedback(
                repository,
                &recommendation_id,
                session_id,
                turn_id,
                RecommendationFeedbackKind::Exposed,
                None,
            );
        }
        let injection_id = record_injection(
            repository,
            session_id,
            turn_id,
            rendered.text.clone(),
            rendered.recommendation_ids,
        );
        Some(RuntimeRecommendationContext {
            injection_id,
            text: rendered.text,
        })
    }

    pub fn observe(
        &mut self,
        event: &TrajectoryEvent,
        turn_id: &str,
        turn_index: u64,
        tool_index: u64,
        repository: Option<&SqliteTrajectoryRepository>,
    ) {
        self.expire(&event.session_id, turn_index, tool_index, repository);
        let entity_name = event
            .entity_ref
            .as_ref()
            .map(|entity| entity.qualified_name.as_str());
        for record in self.records.iter_mut().filter(|record| {
            record.session_id == event.session_id
                && record.status == RecommendationStatus::Exposed
                && record
                    .exposed_turn_index
                    .is_some_and(|index| turn_index.saturating_sub(index) <= FEEDBACK_TURN_WINDOW)
                && tool_index.saturating_sub(record.trigger_tool_index) <= FEEDBACK_TOOL_WINDOW
        }) {
            let kind = match event.kind {
                TrajectoryKind::ReadEntity
                    if entity_name
                        == Some(record.recommendation.entity.qualified_name.as_str()) =>
                {
                    Some(RecommendationFeedbackKind::ReadAfterRecommendation)
                }
                TrajectoryKind::EditEntity
                    if entity_name
                        == Some(record.recommendation.entity.qualified_name.as_str()) =>
                {
                    Some(RecommendationFeedbackKind::EditAfterRecommendation)
                }
                TrajectoryKind::TestEntity
                    if record.recommendation.suggested_action == SuggestedAction::RunTest =>
                {
                    Some(RecommendationFeedbackKind::ValidatedAfterRecommendation)
                }
                _ => None,
            };
            if let Some(kind) = kind {
                record.status = RecommendationStatus::Completed;
                persist(repository, record);
                record_feedback(
                    repository,
                    &record.id,
                    &event.session_id,
                    turn_id,
                    kind,
                    Some(&event.id),
                );
            }
        }
    }

    fn expire(
        &mut self,
        session_id: &str,
        turn_index: u64,
        tool_index: u64,
        repository: Option<&SqliteTrajectoryRepository>,
    ) {
        let now = Utc::now();
        for record in self.records.iter_mut().filter(|record| {
            record.session_id == session_id
                && matches!(
                    record.status,
                    RecommendationStatus::Pending | RecommendationStatus::Exposed
                )
                && (now >= record.expires_at
                    || tool_index.saturating_sub(record.trigger_tool_index) > FEEDBACK_TOOL_WINDOW
                    || record.exposed_turn_index.is_some_and(|index| {
                        turn_index.saturating_sub(index) > FEEDBACK_TURN_WINDOW
                    }))
        }) {
            let was_exposed = record.status == RecommendationStatus::Exposed;
            record.status = RecommendationStatus::Expired;
            persist(repository, record);
            if was_exposed {
                record_feedback(
                    repository,
                    &record.id,
                    &record.session_id,
                    &record.turn_id,
                    RecommendationFeedbackKind::NoObservedAction,
                    None,
                );
            }
        }
    }

    #[cfg(test)]
    pub fn records(&self) -> &[StoredRecommendation] {
        &self.records
    }
}

pub struct RuntimeRecommendationLoop {
    collector: RawEventCollector,
    adapter: CliCogAdapter,
    recommender: Recommender,
    repository: Option<SqliteTrajectoryRepository>,
    queue: PendingRecommendationQueue,
    resolved_graph: TrajectoryGraph,
    resolved_graph_updater: TrajectoryGraphUpdater,
    resolved_recent_events: Vec<TrajectoryEvent>,
    summary_store: RecommendationSummaryStore,
    config: RecommenderConfig,
    tool_index: u64,
    turns: HashMap<String, u64>,
    workspace: PathBuf,
}

impl RuntimeRecommendationLoop {
    pub fn open(workspace: &Path) -> Self {
        let db_path = workspace.join(".cog").join(DEFAULT_RECOMMENDER_DB_NAME);
        let repository = SqliteTrajectoryRepository::open(&db_path).ok();
        let config = repository
            .as_ref()
            .and_then(|repo| repo.load_runtime_config().ok())
            .unwrap_or_default();
        Self {
            collector: RawEventCollector::open_at_workspace(workspace).unwrap_or_default(),
            adapter: CliCogAdapter::new(workspace),
            recommender: Recommender::new(config.clone()),
            repository,
            queue: PendingRecommendationQueue::default(),
            resolved_graph: TrajectoryGraph::default(),
            resolved_graph_updater: TrajectoryGraphUpdater::default(),
            resolved_recent_events: Vec::new(),
            summary_store: RecommendationSummaryStore::new(workspace),
            config,
            tool_index: 0,
            turns: HashMap::new(),
            workspace: workspace.to_path_buf(),
        }
    }

    pub fn record_tool_completed(
        &mut self,
        tool_call_id: &str,
        session_id: &str,
        turn_id: &str,
        tool_name: &str,
        input: serde_json::Value,
        output_summary: String,
        status: ToolEventStatus,
    ) {
        self.reload_config();
        self.tool_index = self.tool_index.saturating_add(1);
        let turn_index = self.turn_index(turn_id);
        self.collector.record_tool_call_started(
            tool_call_id,
            session_id,
            turn_id,
            tool_name,
            input,
        );
        let raw = self.collector.record_tool_call_completed(
            tool_call_id,
            session_id,
            turn_id,
            tool_name,
            output_summary,
            status,
        );
        let events = self
            .collector
            .recent_trajectory_events(16)
            .into_iter()
            .filter(|event| event.raw_event_id == raw.id)
            .collect::<Vec<_>>();
        let mut summary_fallback_used = false;
        for event in events {
            let mut change_recommendations = Vec::new();
            if event.kind == TrajectoryKind::EditEntity && status == ToolEventStatus::Success {
                let before_sync = load_cog_snapshot(&self.workspace);
                let _ = self.adapter.ensure_synced(&self.workspace);
                let after_sync = load_cog_snapshot(&self.workspace);
                change_recommendations = self.record_entity_changes(
                    session_id,
                    turn_id,
                    &raw.id,
                    before_sync.as_ref(),
                    after_sync.as_ref(),
                );
            }
            let resolved = resolve_event_entity(event, &self.adapter);
            for edge in self
                .resolved_graph_updater
                .observe(&mut self.resolved_graph, resolved.clone())
            {
                if let Some(repository) = self.repository.as_ref() {
                    let _ = repository.upsert_trajectory_edge(&edge);
                }
            }
            self.resolved_recent_events.push(resolved.clone());
            if self.resolved_recent_events.len() > 50 {
                let excess = self.resolved_recent_events.len().saturating_sub(50);
                self.resolved_recent_events.drain(0..excess);
            }
            self.queue.observe(
                &resolved,
                turn_id,
                turn_index,
                self.tool_index,
                self.repository.as_ref(),
            );
            if resolved.kind == TrajectoryKind::TestEntity && status == ToolEventStatus::Success {
                continue;
            }
            let mut recommendations = self.recommender.recommend_with_recent_events(
                &resolved,
                &self.adapter,
                &self.resolved_graph,
                self.resolved_recent_events.clone(),
            );
            recommendations.extend(change_recommendations);
            recommendations = dedupe_recommendations(recommendations);
            if recommendations.is_empty() && !summary_fallback_used {
                recommendations = self.summary_recommendations();
                recommendations = dedupe_recommendations(recommendations);
                summary_fallback_used = true;
            }
            if recommendations.is_empty() {
                continue;
            }
            self.queue.enqueue(
                session_id,
                turn_id,
                turn_index,
                self.tool_index,
                &resolved.id,
                recommendations,
                self.repository.as_ref(),
            );
        }
    }

    fn record_entity_changes(
        &self,
        session_id: &str,
        turn_id: &str,
        raw_event_id: &str,
        before: Option<&CogSnapshot>,
        after: Option<&CogSnapshot>,
    ) -> Vec<Recommendation> {
        let Some(before) = before else {
            return Vec::new();
        };
        let Some(after) = after else {
            return Vec::new();
        };
        let now = Utc::now();
        let mut recommendations = Vec::new();
        for (name, entity) in after.entities.iter() {
            if before.entities.contains_key(name) {
                continue;
            }
            let impacted = after
                .reverse_dependencies
                .get(name)
                .cloned()
                .unwrap_or_default();
            let change = EntityChange {
                id: Uuid::new_v4().to_string(),
                session_id: session_id.to_string(),
                turn_id: turn_id.to_string(),
                raw_event_id: raw_event_id.to_string(),
                ts: now,
                kind: EntityChangeKind::Added,
                entity: entity.clone(),
                impacted_entities: impacted.clone(),
            };
            if let Some(repository) = self.repository.as_ref() {
                let _ = repository.record_entity_change(&change);
            }
            recommendations.push(change_recommendation(
                entity.clone(),
                EvidenceSource::EntityAdded,
                0.78,
                "new code entity appeared after this edit; inspect integration and callers",
                name,
            ));
            for impacted_entity in impacted {
                recommendations.push(change_recommendation(
                    impacted_entity,
                    EvidenceSource::EntityAdded,
                    0.68,
                    "new code entity may affect this dependent entity",
                    name,
                ));
            }
        }
        for (name, entity) in before.entities.iter() {
            if after.entities.contains_key(name) {
                continue;
            }
            let impacted = before
                .reverse_dependencies
                .get(name)
                .cloned()
                .unwrap_or_default();
            let change = EntityChange {
                id: Uuid::new_v4().to_string(),
                session_id: session_id.to_string(),
                turn_id: turn_id.to_string(),
                raw_event_id: raw_event_id.to_string(),
                ts: now,
                kind: EntityChangeKind::Deleted,
                entity: entity.clone(),
                impacted_entities: impacted.clone(),
            };
            if let Some(repository) = self.repository.as_ref() {
                let _ = repository.record_entity_change(&change);
            }
            for impacted_entity in impacted {
                recommendations.push(change_recommendation(
                    impacted_entity,
                    EvidenceSource::EntityDeleted,
                    0.92,
                    "deleted code entity had dependents before sync; verify broken references",
                    name,
                ));
            }
        }
        recommendations
    }

    fn summary_recommendations(&self) -> Vec<Recommendation> {
        self.summary_store
            .load_summary(
                super::visualization::VisualizationScope::Session,
                self.config.max_recommendations.max(1),
            )
            .records
            .into_iter()
            .filter(|record| record.server_score >= self.config.min_score)
            .map(|record| Recommendation {
                entity: record.entity.clone(),
                score: record.server_score,
                evidence: record
                    .evidence
                    .into_iter()
                    .map(|item| Evidence {
                        source: item.source,
                        target: record.entity.clone(),
                        weight: item.weight,
                        reason: item.reason,
                        payload: item.payload,
                    })
                    .collect(),
                suggested_action: record.suggested_action,
                display_text: format!("Review {}", record.entity.qualified_name),
            })
            .collect()
    }

    pub fn take_context_for_next_request(
        &mut self,
        session_id: &str,
        turn_id: &str,
    ) -> Option<RuntimeRecommendationContext> {
        self.reload_config();
        let turn_index = self.turn_index(turn_id);
        self.queue.render_for_next_request(
            session_id,
            turn_id,
            turn_index,
            self.tool_index,
            &self.config,
            self.repository.as_ref(),
        )
    }

    pub fn record_injection_context_excerpt(
        &self,
        injection_id: &str,
        request_context_excerpt: &str,
    ) {
        if let Some(repository) = self.repository.as_ref() {
            let _ = repository.update_recommendation_injection_context_excerpt(
                injection_id,
                request_context_excerpt,
            );
        }
    }

    fn turn_index(&mut self, turn_id: &str) -> u64 {
        if let Some(index) = self.turns.get(turn_id) {
            return *index;
        }
        let index = u64::try_from(self.turns.len()).unwrap_or(u64::MAX);
        self.turns.insert(turn_id.to_string(), index);
        index
    }

    fn reload_config(&mut self) {
        if let Some(repository) = self.repository.as_ref()
            && let Ok(config) = repository.load_runtime_config()
        {
            self.config = config.clone();
            self.recommender = Recommender::new(config);
        }
    }

    #[cfg(test)]
    pub fn queue(&self) -> &PendingRecommendationQueue {
        &self.queue
    }
}

fn merge_evidence(existing: &mut Vec<Evidence>, incoming: Vec<Evidence>) {
    let mut seen = existing
        .iter()
        .map(|evidence| {
            (
                evidence.source,
                evidence.target.qualified_name.clone(),
                evidence.reason.clone(),
            )
        })
        .collect::<HashSet<_>>();
    for evidence in incoming {
        let key = (
            evidence.source,
            evidence.target.qualified_name.clone(),
            evidence.reason.clone(),
        );
        if seen.insert(key) {
            existing.push(evidence);
        }
    }
    let mut per_source = HashMap::new();
    existing.retain(|evidence| {
        let count = per_source.entry(evidence.source).or_insert(0usize);
        *count += 1;
        *count <= 3
    });
}

fn dedupe_recommendations(recommendations: Vec<Recommendation>) -> Vec<Recommendation> {
    let mut by_key: HashMap<(String, SuggestedAction), Recommendation> = HashMap::new();
    for recommendation in recommendations {
        let key = (
            recommendation
                .entity
                .cog_entity_id
                .clone()
                .unwrap_or_else(|| recommendation.entity.qualified_name.clone()),
            recommendation.suggested_action,
        );
        match by_key.get_mut(&key) {
            Some(existing) => {
                existing.score = existing.score.max(recommendation.score);
                if recommendation.display_text.len() > existing.display_text.len() {
                    existing.display_text = recommendation.display_text.clone();
                }
                merge_evidence(&mut existing.evidence, recommendation.evidence);
            }
            None => {
                by_key.insert(key, recommendation);
            }
        }
    }
    let mut recommendations = by_key.into_values().collect::<Vec<_>>();
    recommendations.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.entity.qualified_name.cmp(&right.entity.qualified_name))
    });
    recommendations
}

fn persist(repository: Option<&SqliteTrajectoryRepository>, recommendation: &StoredRecommendation) {
    if let Some(repository) = repository {
        let _ = repository.upsert_recommendation(recommendation);
    }
}

fn record_feedback(
    repository: Option<&SqliteTrajectoryRepository>,
    recommendation_id: &str,
    session_id: &str,
    turn_id: &str,
    kind: RecommendationFeedbackKind,
    event_id: Option<&str>,
) {
    if let Some(repository) = repository {
        let _ = repository.record_recommendation_feedback(&RecommendationFeedback {
            id: Uuid::new_v4().to_string(),
            recommendation_id: recommendation_id.to_string(),
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            kind,
            event_id: event_id.map(ToOwned::to_owned),
            created_at: Utc::now(),
        });
    }
}

fn record_injection(
    repository: Option<&SqliteTrajectoryRepository>,
    session_id: &str,
    turn_id: &str,
    context_text: String,
    recommendation_ids: Vec<String>,
) -> String {
    let id = Uuid::new_v4().to_string();
    if let Some(repository) = repository {
        let _ = repository.record_recommendation_injection(&RecommendationInjection {
            id: id.clone(),
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            created_at: Utc::now(),
            context_text,
            request_context_excerpt: None,
            recommendation_ids,
        });
    }
    id
}

#[derive(Debug, Clone, Default)]
struct CogSnapshot {
    entities: HashMap<String, EntityRef>,
    reverse_dependencies: HashMap<String, Vec<EntityRef>>,
}

fn load_cog_snapshot(workspace: &Path) -> Option<CogSnapshot> {
    let path = workspace.join(".cog").join("cog.db");
    let conn = Connection::open(path).ok()?;
    let mut entity_stmt = conn
        .prepare("SELECT id, qualified_name, kind FROM entities ORDER BY qualified_name")
        .ok()?;
    let rows = entity_stmt
        .query_map([], |row| {
            let kind_text: String = row.get(2)?;
            let qualified_name: String = row.get(1)?;
            Ok(EntityRef {
                cog_entity_id: Some(row.get(0)?),
                qualified_name,
                kind: entity_kind_from_cog(&kind_text),
                file_path: None,
                confidence: 0.85,
            })
        })
        .ok()?;
    let entities = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .ok()?
        .into_iter()
        .map(|entity| (entity.qualified_name.clone(), entity))
        .collect::<HashMap<_, _>>();
    let mut reverse_dependencies: HashMap<String, Vec<EntityRef>> = HashMap::new();
    let mut relation_stmt = conn
        .prepare(
            "SELECT source.qualified_name, target.qualified_name
             FROM entity_relations rel
             JOIN entities source ON source.id = rel.from_entity
             JOIN entities target ON target.id = rel.to_entity
             WHERE rel.kind IN ('calls', 'uses')",
        )
        .ok()?;
    let relation_rows = relation_stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .ok()?;
    for row in relation_rows {
        let Ok((source, target)) = row else {
            continue;
        };
        if let Some(source_entity) = entities.get(&source) {
            reverse_dependencies
                .entry(target)
                .or_default()
                .push(source_entity.clone());
        }
    }
    Some(CogSnapshot {
        entities,
        reverse_dependencies,
    })
}

fn change_recommendation(
    target: EntityRef,
    source: EvidenceSource,
    score: f64,
    reason: &str,
    changed_entity: &str,
) -> Recommendation {
    let evidence = Evidence {
        source,
        target: target.clone(),
        weight: score,
        reason: reason.to_string(),
        payload: json!({ "changed_entity": changed_entity }),
    };
    Recommendation {
        entity: target.clone(),
        score,
        evidence: vec![evidence],
        suggested_action: SuggestedAction::UpdateRelatedCode,
        display_text: format!(
            "Verify {} after structural graph change",
            target.qualified_name
        ),
    }
}

fn entity_kind_from_cog(value: &str) -> EntityKind {
    match value.to_ascii_lowercase().as_str() {
        "module" => EntityKind::Module,
        "function" => EntityKind::Function,
        "type" | "class" | "interface" | "struct" | "enum" => EntityKind::Type,
        "method" => EntityKind::Method,
        "file" => EntityKind::File,
        _ => EntityKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cog_recommender::cog_adapter::StaticCogAdapter;
    use crate::cog_recommender::graph::TrajectoryGraph;
    use crate::cog_recommender::normalizer::normalize_tool_event;
    use crate::cog_recommender::recommender::Recommender;
    use crate::cog_recommender::resolver::resolve_event_entity;
    use crate::cog_recommender::storage::TrajectoryRepository;
    use crate::cog_recommender::types::{EntityKind, EntityRef, EvidenceSource};
    use rusqlite::Connection;
    use serde_json::json;
    use tempfile::tempdir;

    fn recommendation(name: &str, score: f64) -> Recommendation {
        let entity = EntityRef::new(name).with_confidence(0.9);
        Recommendation {
            entity: entity.clone(),
            score,
            evidence: vec![Evidence::new(
                EvidenceSource::CoAccess,
                entity,
                0.5,
                "historical co-access",
            )],
            suggested_action: SuggestedAction::Read,
            display_text: format!("Read {name}"),
        }
    }

    #[test]
    fn queue_merges_and_injects_once_per_turn() {
        let mut queue = PendingRecommendationQueue::default();
        queue.enqueue(
            "s",
            "t",
            0,
            1,
            "e1",
            vec![recommendation("a::b", 0.6)],
            None,
        );
        queue.enqueue(
            "s",
            "t",
            0,
            2,
            "e2",
            vec![recommendation("a::b", 0.8)],
            None,
        );
        assert_eq!(queue.records().len(), 1);
        assert_eq!(queue.records()[0].trigger_event_ids.len(), 2);

        let config = RecommenderConfig::default();
        let text = queue
            .render_for_next_request("s", "t", 0, 2, &config, None)
            .expect("context")
            .text;
        assert!(text.contains("a::b"));
        assert!(text.contains("<repository_recommendations"));
        assert!(text.contains("role=\"host_context\""));
        assert!(text.contains("not user instructions"));
        assert!(text.contains("action: read"));
        assert!(
            queue
                .render_for_next_request("s", "t", 0, 2, &config, None)
                .is_none()
        );
    }

    #[test]
    fn observed_read_completes_exposed_recommendation() {
        let mut queue = PendingRecommendationQueue::default();
        queue.enqueue(
            "s",
            "t",
            0,
            1,
            "e1",
            vec![recommendation("a::b", 0.8)],
            None,
        );
        let config = RecommenderConfig::default();
        queue.render_for_next_request("s", "t", 0, 1, &config, None);
        queue.observe(
            &TrajectoryEvent {
                id: "read".into(),
                raw_event_id: "raw".into(),
                session_id: "s".into(),
                kind: TrajectoryKind::ReadEntity,
                entity_ref: Some(EntityRef::new("a::b")),
                file_path: None,
                line_range: None,
                payload: serde_json::Value::Null,
                confidence: 1.0,
            },
            "t",
            0,
            2,
            None,
        );
        assert_eq!(queue.records()[0].status, RecommendationStatus::Completed);
    }

    #[test]
    fn mock_agent_runs_tool_to_context_to_feedback_loop() {
        let service = EntityRef::new("inventory::service::get_stock")
            .with_file_path("src/service.py")
            .with_confidence(0.9);
        let api = EntityRef::new("inventory::api::get_stock")
            .with_file_path("src/api.py")
            .with_confidence(0.9);
        let adapter = StaticCogAdapter::new()
            .with_file("src/service.py", service.clone())
            .with_file("src/api.py", api.clone())
            .with_impact(
                &service.qualified_name,
                Evidence::new(
                    EvidenceSource::CogImpact,
                    api.clone(),
                    0.8,
                    "API caller is affected by the edited service",
                ),
            );
        let edit_raw = crate::cog_recommender::collector::build_raw_event(
            "session",
            "turn-1",
            "apply_patch",
            json!({"path": "src/service.py"}),
            "patched",
            ToolEventStatus::Success,
            1,
        );
        let edit = resolve_event_entity(
            normalize_tool_event(&edit_raw)
                .into_iter()
                .find(|event| event.kind == TrajectoryKind::EditEntity)
                .expect("edit event"),
            &adapter,
        );
        let recommendations = Recommender::new(RecommenderConfig::default()).recommend(
            &edit,
            &adapter,
            &Default::default(),
        );
        let mut queue = PendingRecommendationQueue::default();
        queue.enqueue("session", "turn-1", 0, 1, &edit.id, recommendations, None);
        let context = queue
            .render_for_next_request(
                "session",
                "turn-1",
                0,
                1,
                &RecommenderConfig::default(),
                None,
            )
            .expect("injected context");
        assert!(context.text.contains(&api.qualified_name));
        assert!(context.text.contains("evidence: cog_impact"));
        assert!(context.text.contains("Prefer reading or verifying"));

        let read_raw = crate::cog_recommender::collector::build_raw_event(
            "session",
            "turn-1",
            "read_file",
            json!({"path": "src/api.py"}),
            "contents",
            ToolEventStatus::Success,
            1,
        );
        let read = resolve_event_entity(
            normalize_tool_event(&read_raw)
                .into_iter()
                .find(|event| event.kind == TrajectoryKind::ReadEntity)
                .expect("read event"),
            &adapter,
        );
        queue.observe(&read, "turn-1", 0, 2, None);
        assert_eq!(queue.records()[0].status, RecommendationStatus::Completed);
    }

    #[test]
    fn queue_exposes_only_records_that_fit_render_budget() {
        let mut queue = PendingRecommendationQueue::default();
        queue.enqueue(
            "s",
            "t",
            0,
            1,
            "e1",
            vec![recommendation("short::target", 0.9)],
            None,
        );
        queue.enqueue(
            "s",
            "t",
            0,
            2,
            "e2",
            vec![recommendation(
                "very::long::target::that::does::not::fit",
                0.8,
            )],
            None,
        );
        let mut config = RecommenderConfig::default();
        config.max_total_chars = 420;

        let text = queue
            .render_for_next_request("s", "t", 0, 2, &config, None)
            .expect("context")
            .text;

        assert!(text.contains("short::target"));
        assert!(!text.contains("very::long::target"));
        assert_eq!(queue.records()[0].status, RecommendationStatus::Exposed);
        assert_eq!(queue.records()[1].status, RecommendationStatus::Pending);
    }

    #[test]
    fn queue_persists_injected_context_snapshot() {
        let repo = SqliteTrajectoryRepository::open_in_memory().expect("repo");
        let mut queue = PendingRecommendationQueue::default();
        queue.enqueue(
            "s",
            "t",
            0,
            1,
            "e1",
            vec![recommendation("a::b", 0.9)],
            Some(&repo),
        );

        let text = queue
            .render_for_next_request("s", "t", 0, 1, &RecommenderConfig::default(), Some(&repo))
            .expect("context");
        let injections = repo
            .list_recent_recommendation_injections(
                crate::cog_recommender::visualization::VisualizationScope::Session,
                10,
            )
            .expect("injections");

        assert_eq!(injections.len(), 1);
        assert_eq!(injections[0].id, text.injection_id);
        assert_eq!(injections[0].context_text, text.text);
        assert!(injections[0].request_context_excerpt.is_none());
        assert_eq!(injections[0].recommendation_ids.len(), 1);
    }

    #[test]
    fn runtime_materializes_summary_recommendations_when_online_candidates_are_empty() {
        let workspace = tempdir().expect("workspace");
        let cog_dir = workspace.path().join(".cog");
        std::fs::create_dir_all(&cog_dir).expect("cog dir");
        let conn = Connection::open(cog_dir.join("cog.db")).expect("cog db");
        conn.execute(
            "CREATE TABLE entities (
                id TEXT PRIMARY KEY,
                qualified_name TEXT NOT NULL,
                kind TEXT NOT NULL
            )",
            [],
        )
        .expect("entities table");
        conn.execute(
            "INSERT INTO entities (id, qualified_name, kind) VALUES (?1, ?2, ?3)",
            rusqlite::params!["module-main", "main", "module"],
        )
        .expect("entity");

        let repo = SqliteTrajectoryRepository::open(&cog_dir.join(DEFAULT_RECOMMENDER_DB_NAME))
            .expect("repo");
        let mut graph = TrajectoryGraph::default();
        let source = EntityRef::new("search:main").with_kind(EntityKind::Unknown);
        let target = EntityRef::new("main.py")
            .with_kind(EntityKind::File)
            .with_file_path("main.py")
            .with_confidence(0.4);
        let edge = graph.observe_edge(&source, &target, EvidenceSource::CoAccess, 0.8, "co access");
        repo.upsert_trajectory_edge(&edge).expect("edge");

        let mut runtime = RuntimeRecommendationLoop::open(workspace.path());
        runtime.record_tool_completed(
            "tool-1",
            "session",
            "turn",
            "read_file",
            json!({"path": "unresolved.txt"}),
            "ok".to_string(),
            ToolEventStatus::Success,
        );

        assert_eq!(repo.count_rows("recommendations").unwrap(), 1);
    }
}
