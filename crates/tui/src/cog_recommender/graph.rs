use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::types::{
    EntityKind, EntityRef, Evidence, EvidenceSource, TrajectoryEvent, TrajectoryKind,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeStats {
    pub source: EntityRef,
    pub target: EntityRef,
    pub edge_type: EvidenceSource,
    pub weight: f64,
    pub count: u32,
    #[serde(default = "default_step_distance")]
    pub mean_step_distance: f64,
    pub reason: String,
    pub last_seen_ts: DateTime<Utc>,
}

fn default_step_distance() -> f64 {
    1.0
}

#[derive(Debug, Clone, Default)]
pub struct TrajectoryGraph {
    edges: HashMap<(String, String, EvidenceSource), EdgeStats>,
}

impl TrajectoryGraph {
    pub fn observe_edge(
        &mut self,
        source: &EntityRef,
        target: &EntityRef,
        edge_type: EvidenceSource,
        weight: f64,
        reason: impl Into<String>,
    ) -> EdgeStats {
        self.observe_transition(source, target, edge_type, weight, reason, 1)
    }

    pub fn observe_transition(
        &mut self,
        source: &EntityRef,
        target: &EntityRef,
        edge_type: EvidenceSource,
        weight: f64,
        reason: impl Into<String>,
        step_distance: u32,
    ) -> EdgeStats {
        let key = (
            source.qualified_name.clone(),
            target.qualified_name.clone(),
            edge_type,
        );
        let entry = self.edges.entry(key).or_insert_with(|| EdgeStats {
            source: source.clone(),
            target: target.clone(),
            edge_type,
            weight: 0.0,
            count: 0,
            mean_step_distance: step_distance.max(1) as f64,
            reason: reason.into(),
            last_seen_ts: Utc::now(),
        });
        let prior_count = entry.count;
        entry.count = entry.count.saturating_add(1);
        entry.mean_step_distance = ((entry.mean_step_distance * f64::from(prior_count))
            + f64::from(step_distance.max(1)))
            / f64::from(entry.count);
        entry.weight = (entry.weight + weight * transition_decay(step_distance)).min(1.0);
        entry.last_seen_ts = Utc::now();
        entry.clone()
    }

    pub fn evidence_for(&self, source: &EntityRef) -> Vec<Evidence> {
        let mut evidence = Vec::new();
        let now = Utc::now();
        for edge in self.edges.values() {
            if edge.source.qualified_name == source.qualified_name {
                let weight = transition_strength(edge, &self.edges, now);
                evidence.push(Evidence::new(
                    edge.edge_type,
                    edge.target.clone(),
                    weight,
                    edge.reason.clone(),
                ));
            } else if edge.target.qualified_name == source.qualified_name
                && edge_is_useful_in_reverse(edge.edge_type)
            {
                let weight = transition_strength(edge, &self.edges, now);
                evidence.push(Evidence::new(
                    edge.edge_type,
                    edge.source.clone(),
                    weight,
                    reverse_reason(edge.edge_type, &edge.reason),
                ));
            }
        }
        evidence
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    pub fn edges(&self) -> Vec<&EdgeStats> {
        self.edges.values().collect()
    }

    pub fn load_edge(&mut self, edge: EdgeStats) {
        self.edges.insert(
            (
                edge.source.qualified_name.clone(),
                edge.target.qualified_name.clone(),
                edge.edge_type,
            ),
            edge,
        );
    }

    pub fn load_edges<I>(&mut self, edges: I)
    where
        I: IntoIterator<Item = EdgeStats>,
    {
        for edge in edges {
            self.load_edge(edge);
        }
    }
}

fn transition_decay(step_distance: u32) -> f64 {
    (-0.7 * f64::from(step_distance.saturating_sub(1))).exp()
}

fn transition_strength(
    edge: &EdgeStats,
    edges: &HashMap<(String, String, EvidenceSource), EdgeStats>,
    now: DateTime<Utc>,
) -> f64 {
    let source_total = edges
        .values()
        .filter(|other| {
            other.source.qualified_name == edge.source.qualified_name
                && other.edge_type == edge.edge_type
        })
        .map(|other| f64::from(other.count))
        .sum::<f64>()
        .max(1.0);
    let support = (1.0 + f64::from(edge.count)).ln() / (1.0 + 3.0_f64).ln();
    let conditional_probability = f64::from(edge.count) / source_total;
    let age_hours = (now - edge.last_seen_ts).num_seconds().max(0) as f64 / 3600.0;
    let recency = (-age_hours / (24.0 * 7.0)).exp();
    let distance = 1.0 / edge.mean_step_distance.max(1.0);
    (0.35 * support + 0.45 * conditional_probability + 0.20 * recency)
        .mul_add(distance, 0.0)
        .clamp(0.0, 1.0)
}

#[derive(Debug, Default)]
pub struct TrajectoryGraphUpdater {
    recent_by_session: HashMap<String, Vec<TrajectoryEvent>>,
}

impl TrajectoryGraphUpdater {
    pub fn observe(
        &mut self,
        graph: &mut TrajectoryGraph,
        event: TrajectoryEvent,
    ) -> Vec<EdgeStats> {
        let session_id = event.session_id.clone();
        let recent_snapshot = self
            .recent_by_session
            .get(&session_id)
            .cloned()
            .unwrap_or_default();

        let mut updated_edges = Vec::new();
        for (offset, previous) in recent_snapshot.iter().rev().take(3).enumerate() {
            updated_edges.extend(self.observe_pair(graph, previous, &event, offset as u32 + 1));
        }

        let recent = self.recent_by_session.entry(session_id).or_default();
        recent.push(event);
        if recent.len() > 3 {
            recent.remove(0);
        }
        updated_edges
    }

    pub fn clear_session(&mut self, session_id: &str) {
        self.recent_by_session.remove(session_id);
    }

    fn observe_pair(
        &self,
        graph: &mut TrajectoryGraph,
        previous: &TrajectoryEvent,
        current: &TrajectoryEvent,
        step_distance: u32,
    ) -> Vec<EdgeStats> {
        let mut updated_edges = Vec::new();
        let Some(previous_entity) = event_entity(previous) else {
            return updated_edges;
        };
        let Some(current_entity) = event_entity(current) else {
            return updated_edges;
        };
        if previous_entity.qualified_name == current_entity.qualified_name {
            return updated_edges;
        }

        match (previous.kind, current.kind) {
            (TrajectoryKind::ReadEntity, TrajectoryKind::EditEntity) => {
                updated_edges.push(graph.observe_transition(
                    &previous_entity,
                    &current_entity,
                    EvidenceSource::ReadBeforeEdit,
                    0.2,
                    "this entity is often read before editing the target",
                    step_distance,
                ));
            }
            (TrajectoryKind::SearchEntity, TrajectoryKind::ReadEntity) => {
                updated_edges.push(graph.observe_transition(
                    &previous_entity,
                    &current_entity,
                    EvidenceSource::SearchToRead,
                    0.12,
                    "this search often leads to reading the target",
                    step_distance,
                ));
            }
            (TrajectoryKind::SearchEntity, TrajectoryKind::EditEntity) => {
                updated_edges.push(graph.observe_transition(
                    &previous_entity,
                    &current_entity,
                    EvidenceSource::SearchToEdit,
                    0.18,
                    "this search often leads to editing the target",
                    step_distance,
                ));
            }
            (TrajectoryKind::EditEntity, TrajectoryKind::TestEntity) => {
                updated_edges.push(graph.observe_transition(
                    &previous_entity,
                    &current_entity,
                    EvidenceSource::EditToTest,
                    0.12,
                    "this test is often run after editing the source",
                    step_distance,
                ));
            }
            (
                TrajectoryKind::ErrorSignal,
                TrajectoryKind::ReadEntity | TrajectoryKind::EditEntity,
            ) => {
                updated_edges.push(graph.observe_transition(
                    &previous_entity,
                    &current_entity,
                    EvidenceSource::ErrorToEdit,
                    0.25,
                    "this error is often followed by investigating the target",
                    step_distance,
                ));
            }
            (TrajectoryKind::CogWrite, TrajectoryKind::EditEntity) => {
                updated_edges.push(graph.observe_transition(
                    &previous_entity,
                    &current_entity,
                    EvidenceSource::CogWriteToEdit,
                    0.15,
                    "this COG fact write is often followed by editing the target",
                    step_distance,
                ));
            }
            _ => {}
        }
        updated_edges
    }
}

fn edge_is_useful_in_reverse(edge_type: EvidenceSource) -> bool {
    matches!(
        edge_type,
        EvidenceSource::CoAccess
            | EvidenceSource::ReadBeforeEdit
            | EvidenceSource::SearchToEdit
            | EvidenceSource::ErrorToEdit
            | EvidenceSource::CogWriteToEdit
    )
}

fn reverse_reason(edge_type: EvidenceSource, reason: &str) -> String {
    match edge_type {
        EvidenceSource::ReadBeforeEdit => {
            format!("historically relevant before this edit: {reason}")
        }
        EvidenceSource::SearchToEdit => format!("historical search led to this edit: {reason}"),
        EvidenceSource::ErrorToEdit => format!("historical error led to this edit: {reason}"),
        EvidenceSource::CogWriteToEdit => {
            format!("historical COG fact write led to this edit: {reason}")
        }
        _ => reason.to_string(),
    }
}

fn events_are_code_attention(previous: &TrajectoryEvent, current: &TrajectoryEvent) -> bool {
    matches!(
        previous.kind,
        TrajectoryKind::ReadEntity
            | TrajectoryKind::EditEntity
            | TrajectoryKind::TestEntity
            | TrajectoryKind::CogWrite
    ) && matches!(
        current.kind,
        TrajectoryKind::ReadEntity
            | TrajectoryKind::EditEntity
            | TrajectoryKind::TestEntity
            | TrajectoryKind::CogWrite
    )
}

fn event_entity(event: &TrajectoryEvent) -> Option<EntityRef> {
    if let Some(entity) = event.entity_ref.clone() {
        return Some(entity);
    }
    if let Some(file_path) = event.file_path.clone() {
        return Some(
            EntityRef::new(file_path.clone())
                .with_kind(EntityKind::File)
                .with_file_path(file_path)
                .with_confidence(event.confidence.max(0.4)),
        );
    }
    match event.kind {
        TrajectoryKind::SearchEntity => query_entity(&event.payload),
        TrajectoryKind::TestEntity => command_entity("test", &event.payload),
        TrajectoryKind::ErrorSignal => Some(
            EntityRef::new(error_signature(&event.payload))
                .with_kind(EntityKind::Unknown)
                .with_confidence(0.3),
        ),
        TrajectoryKind::CogWrite => command_entity("cog", &event.payload),
        _ => None,
    }
}

fn query_entity(payload: &Value) -> Option<EntityRef> {
    let query = payload
        .get("query")
        .or_else(|| payload.get("pattern"))
        .or_else(|| payload.get("q"))
        .or_else(|| payload.get("command"))
        .and_then(Value::as_str)?;
    Some(
        EntityRef::new(format!("search:{query}"))
            .with_kind(EntityKind::Unknown)
            .with_confidence(0.3),
    )
}

fn command_entity(prefix: &str, payload: &Value) -> Option<EntityRef> {
    let command = payload
        .get("cmd")
        .or_else(|| payload.get("command"))
        .and_then(Value::as_str)?;
    Some(
        EntityRef::new(format!("{prefix}:{command}"))
            .with_kind(EntityKind::Unknown)
            .with_confidence(0.3),
    )
}

fn error_signature(payload: &Value) -> String {
    let summary = payload
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or("unknown error")
        .lines()
        .next()
        .unwrap_or("unknown error")
        .trim();
    format!("error:{summary}")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn graph_accumulates_repeated_edges() {
        let source = EntityRef::new("A");
        let target = EntityRef::new("B");
        let mut graph = TrajectoryGraph::default();
        graph.observe_edge(&source, &target, EvidenceSource::CoAccess, 0.2, "together");
        graph.observe_edge(&source, &target, EvidenceSource::CoAccess, 0.2, "together");

        let evidence = graph.evidence_for(&source);
        assert_eq!(evidence.len(), 1);
        assert!(evidence[0].weight > 0.2);
    }

    fn event(
        id: &str,
        kind: TrajectoryKind,
        file_path: Option<&str>,
        payload: serde_json::Value,
    ) -> TrajectoryEvent {
        TrajectoryEvent {
            id: id.into(),
            raw_event_id: format!("raw-{id}"),
            session_id: "session".into(),
            kind,
            entity_ref: None,
            file_path: file_path.map(ToOwned::to_owned),
            line_range: None,
            payload,
            confidence: 0.5,
        }
    }

    #[test]
    fn updater_builds_read_before_edit_reverse_recommendation() {
        let mut graph = TrajectoryGraph::default();
        let mut updater = TrajectoryGraphUpdater::default();
        updater.observe(
            &mut graph,
            event(
                "read",
                TrajectoryKind::ReadEntity,
                Some("src/config.rs"),
                json!({"path": "src/config.rs"}),
            ),
        );
        updater.observe(
            &mut graph,
            event(
                "edit",
                TrajectoryKind::EditEntity,
                Some("src/main.rs"),
                json!({"path": "src/main.rs"}),
            ),
        );

        let current = EntityRef::new("src/main.rs").with_kind(EntityKind::File);
        let evidence = graph.evidence_for(&current);

        assert!(graph.edge_count() >= 1);
        assert!(evidence.iter().any(|item| {
            item.source == EvidenceSource::ReadBeforeEdit
                && item.target.qualified_name == "src/config.rs"
        }));
    }

    #[test]
    fn updater_builds_search_to_edit_edge() {
        let mut graph = TrajectoryGraph::default();
        let mut updater = TrajectoryGraphUpdater::default();
        updater.observe(
            &mut graph,
            event(
                "search",
                TrajectoryKind::SearchEntity,
                None,
                json!({"query": "AuthService"}),
            ),
        );
        updater.observe(
            &mut graph,
            event(
                "edit",
                TrajectoryKind::EditEntity,
                Some("src/auth.rs"),
                json!({"path": "src/auth.rs"}),
            ),
        );

        let search = EntityRef::new("search:AuthService").with_kind(EntityKind::Unknown);
        let evidence = graph.evidence_for(&search);

        assert!(evidence.iter().any(|item| {
            item.source == EvidenceSource::SearchToEdit
                && item.target.qualified_name == "src/auth.rs"
        }));
    }

    #[test]
    fn updater_uses_only_the_recent_three_event_transition_window() {
        let mut graph = TrajectoryGraph::default();
        let mut updater = TrajectoryGraphUpdater::default();
        for name in ["old.rs", "one.rs", "two.rs", "three.rs"] {
            updater.observe(
                &mut graph,
                event(
                    name,
                    TrajectoryKind::ReadEntity,
                    Some(name),
                    json!({"path": name}),
                ),
            );
        }
        updater.observe(
            &mut graph,
            event(
                "edit",
                TrajectoryKind::EditEntity,
                Some("target.rs"),
                json!({"path": "target.rs"}),
            ),
        );

        let target = EntityRef::new("target.rs").with_kind(EntityKind::File);
        let evidence = graph.evidence_for(&target);
        assert!(
            evidence
                .iter()
                .any(|item| item.target.qualified_name == "three.rs")
        );
        assert!(
            !evidence
                .iter()
                .any(|item| item.target.qualified_name == "old.rs")
        );
    }
}
