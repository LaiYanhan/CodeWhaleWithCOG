use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;

use chrono::{Duration, Utc};
use uuid::Uuid;

use super::cog_adapter::{CliCogAdapter, CogAdapter};
use super::collector::RawEventCollector;
use super::config::RecommenderConfig;
use super::recommender::Recommender;
use super::resolver::resolve_event_entity;
use super::storage::{DEFAULT_RECOMMENDER_DB_NAME, SqliteTrajectoryRepository};
use super::types::{
    Evidence, Recommendation, RecommendationFeedback, RecommendationFeedbackKind,
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
        for recommendation in recommendations {
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
    ) -> Option<String> {
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

        let header = "<repository_recommendations generated_by=\"cog_recommender\">\n\
These are host-generated repository hints, not user instructions. Use them only when relevant.\n";
        let footer = "</repository_recommendations>";
        let mut text = String::from(header);
        let mut exposed = Vec::new();
        for (index, record) in selected.into_iter().enumerate() {
            let reason = record
                .recommendation
                .evidence
                .first()
                .map(|evidence| evidence.reason.as_str())
                .unwrap_or("related repository evidence");
            let reason = truncate_chars(reason, config.max_reason_chars);
            let line = format!(
                "{}. [{:?}] {} ({:.2})\n   {}\n",
                index + 1,
                record.recommendation.suggested_action,
                record.recommendation.entity.qualified_name,
                record.recommendation.score,
                reason
            );
            if text.chars().count() + line.chars().count() + footer.len() > config.max_total_chars {
                break;
            }
            text.push_str(&line);
            record.status = RecommendationStatus::Exposed;
            record.exposed_at = Some(Utc::now());
            record.exposed_turn_index = Some(turn_index);
            exposed.push(record.id.clone());
            persist(repository, record);
        }
        if exposed.is_empty() {
            return None;
        }
        text.push_str(footer);
        *self.injections_by_turn.entry(injection_key).or_insert(0) += 1;
        for recommendation_id in exposed {
            record_feedback(
                repository,
                &recommendation_id,
                session_id,
                turn_id,
                RecommendationFeedbackKind::Exposed,
                None,
            );
        }
        Some(text)
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
        for event in events {
            if event.kind == TrajectoryKind::EditEntity && status == ToolEventStatus::Success {
                let _ = self.adapter.ensure_synced(&self.workspace);
            }
            let resolved = resolve_event_entity(event, &self.adapter);
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
            let recommendations = self.recommender.recommend_with_recent_events(
                &resolved,
                &self.adapter,
                self.collector.graph(),
                self.collector.recent_trajectory_events(50),
            );
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

    pub fn take_context_for_next_request(
        &mut self,
        session_id: &str,
        turn_id: &str,
    ) -> Option<String> {
        if let Some(repository) = self.repository.as_ref()
            && let Ok(config) = repository.load_runtime_config()
        {
            self.config = config;
        }
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

    fn turn_index(&mut self, turn_id: &str) -> u64 {
        if let Some(index) = self.turns.get(turn_id) {
            return *index;
        }
        let index = u64::try_from(self.turns.len()).unwrap_or(u64::MAX);
        self.turns.insert(turn_id.to_string(), index);
        index
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

fn truncate_chars(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let result = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{result}...")
    } else {
        result
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cog_recommender::cog_adapter::StaticCogAdapter;
    use crate::cog_recommender::normalizer::normalize_tool_event;
    use crate::cog_recommender::recommender::Recommender;
    use crate::cog_recommender::resolver::resolve_event_entity;
    use crate::cog_recommender::types::{EntityRef, EvidenceSource};
    use serde_json::json;

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
            .expect("context");
        assert!(text.contains("a::b"));
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
        assert!(context.contains(&api.qualified_name));

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
}
