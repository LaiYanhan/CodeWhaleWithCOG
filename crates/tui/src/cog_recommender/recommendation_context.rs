use std::collections::HashSet;

use super::config::RecommenderConfig;
use super::types::{EntityRef, Evidence, TrajectoryEvent, TrajectoryKind};

#[derive(Debug, Clone)]
pub struct RecommendationContext {
    pub trigger: TrajectoryEvent,
    pub current_entity: Option<EntityRef>,
    pub recent_events: Vec<TrajectoryEvent>,
    pub current_turn_seen_entities: HashSet<String>,
    pub current_session_seen_entities: HashSet<String>,
    pub trajectory_evidence: Vec<Evidence>,
    pub cog_evidence: Vec<Evidence>,
    pub rule_evidence: Vec<Evidence>,
    pub config: RecommenderConfig,
}

impl RecommendationContext {
    pub fn new(trigger: TrajectoryEvent, config: RecommenderConfig) -> Self {
        let current_entity = trigger.entity_ref.clone();
        Self {
            trigger,
            current_entity,
            recent_events: Vec::new(),
            current_turn_seen_entities: HashSet::new(),
            current_session_seen_entities: HashSet::new(),
            trajectory_evidence: Vec::new(),
            cog_evidence: Vec::new(),
            rule_evidence: Vec::new(),
            config,
        }
    }

    pub fn with_recent_events(mut self, recent_events: Vec<TrajectoryEvent>) -> Self {
        self.current_turn_seen_entities = seen_entities_for_turn(&self.trigger, &recent_events);
        self.current_session_seen_entities = seen_entities_for_session(&recent_events);
        self.recent_events = recent_events;
        self
    }

    pub fn with_trajectory_evidence(mut self, evidence: Vec<Evidence>) -> Self {
        self.trajectory_evidence = evidence;
        self
    }

    pub fn with_cog_evidence(mut self, evidence: Vec<Evidence>) -> Self {
        self.cog_evidence = evidence;
        self
    }

    pub fn with_rule_evidence(mut self, evidence: Vec<Evidence>) -> Self {
        self.rule_evidence = evidence;
        self
    }
}

fn seen_entities_for_turn(
    trigger: &TrajectoryEvent,
    recent_events: &[TrajectoryEvent],
) -> HashSet<String> {
    recent_events
        .iter()
        .filter(|event| event.session_id == trigger.session_id)
        .filter(|event| event_is_seen_attention(event))
        .filter(|event| same_turn(event, trigger))
        .filter_map(event_entity_key)
        .collect()
}

fn seen_entities_for_session(recent_events: &[TrajectoryEvent]) -> HashSet<String> {
    recent_events
        .iter()
        .filter(|event| event_is_seen_attention(event))
        .filter_map(event_entity_key)
        .collect()
}

fn same_turn(left: &TrajectoryEvent, right: &TrajectoryEvent) -> bool {
    left.raw_event_id == right.raw_event_id
}

fn event_is_seen_attention(event: &TrajectoryEvent) -> bool {
    matches!(
        event.kind,
        TrajectoryKind::ReadEntity
            | TrajectoryKind::EditEntity
            | TrajectoryKind::TestEntity
            | TrajectoryKind::CogWrite
    )
}

fn event_entity_key(event: &TrajectoryEvent) -> Option<String> {
    event
        .entity_ref
        .as_ref()
        .map(entity_key)
        .or_else(|| event.file_path.clone())
}

pub fn entity_key(entity: &EntityRef) -> String {
    entity
        .cog_entity_id
        .clone()
        .unwrap_or_else(|| entity.qualified_name.clone())
}
