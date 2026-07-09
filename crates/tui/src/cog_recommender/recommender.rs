use super::algorithms::RecommendationAlgorithm;
use super::algorithms::weighted_v1::WeightedV1Algorithm;
use super::candidate_sources::CandidateSources;
use super::cog_adapter::CogAdapter;
use super::config::RecommenderConfig;
use super::graph::TrajectoryGraph;
use super::recommendation_context::RecommendationContext;
use super::types::{Evidence, Recommendation, TrajectoryEvent};

pub struct Recommender {
    config: RecommenderConfig,
    algorithm: WeightedV1Algorithm,
}

impl Recommender {
    pub fn new(config: RecommenderConfig) -> Self {
        Self {
            config,
            algorithm: WeightedV1Algorithm,
        }
    }

    pub fn recommend(
        &self,
        trigger: &TrajectoryEvent,
        adapter: &impl CogAdapter,
        graph: &TrajectoryGraph,
    ) -> Vec<Recommendation> {
        self.recommend_with_recent_events(trigger, adapter, graph, Vec::new())
    }

    pub fn recommend_with_recent_events(
        &self,
        trigger: &TrajectoryEvent,
        adapter: &impl CogAdapter,
        graph: &TrajectoryGraph,
        recent_events: Vec<TrajectoryEvent>,
    ) -> Vec<Recommendation> {
        let Some(current) = trigger.entity_ref.as_ref() else {
            return Vec::new();
        };

        let cog_evidence = collect_cog_evidence(current, adapter);
        let trajectory_evidence = graph.evidence_for(current);
        let context = RecommendationContext::new(trigger.clone(), self.config.clone())
            .with_recent_events(recent_events)
            .with_cog_evidence(cog_evidence.clone())
            .with_trajectory_evidence(trajectory_evidence.clone());
        let mut candidates = Vec::new();
        candidates.extend(CandidateSources::from_evidence(&trigger.id, cog_evidence));
        candidates.extend(CandidateSources::from_evidence(
            &trigger.id,
            trajectory_evidence,
        ));

        self.algorithm.recommend(&context, candidates)
    }
}

fn collect_cog_evidence(
    entity: &super::types::EntityRef,
    adapter: &impl CogAdapter,
) -> Vec<Evidence> {
    let mut evidence = Vec::new();
    evidence.extend(adapter.impact(entity));
    evidence.extend(adapter.related(entity));
    evidence.extend(adapter.assertions(entity));
    evidence
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cog_recommender::candidate_sources::CandidateSources;
    use crate::cog_recommender::cog_adapter::StaticCogAdapter;
    use crate::cog_recommender::graph::TrajectoryGraph;
    use crate::cog_recommender::recommendation_context::RecommendationContext;
    use crate::cog_recommender::types::{
        EntityKind, EntityRef, EvidenceSource, TrajectoryEvent, TrajectoryKind,
    };
    use serde_json::Value;

    #[test]
    fn rank_merges_duplicate_candidates() {
        let target = EntityRef::new("target");
        let mut candidates = CandidateSources::from_evidence(
            "evt",
            vec![Evidence::new(
                EvidenceSource::CogImpact,
                target.clone(),
                0.4,
                "impact",
            )],
        );
        candidates.extend(CandidateSources::from_evidence(
            "evt",
            vec![Evidence::new(
                EvidenceSource::CoAccess,
                target,
                0.3,
                "co-access",
            )],
        ));

        let ranked = WeightedV1Algorithm.recommend(
            &RecommendationContext::new(trigger(), RecommenderConfig::default()),
            candidates,
        );
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].evidence.len(), 2);
    }

    #[test]
    fn recommend_with_recent_events_penalizes_session_seen_entity() {
        let current = entity("current");
        let seen = entity("seen");
        let fresh = entity("fresh");
        let trigger = event("trigger", TrajectoryKind::ReadEntity, Some(current.clone()));
        let recent_seen = event("seen-event", TrajectoryKind::ReadEntity, Some(seen.clone()));
        let mut graph = TrajectoryGraph::default();
        graph.observe_edge(
            &current,
            &seen,
            EvidenceSource::CogImpact,
            1.0,
            "seen impact",
        );
        graph.observe_edge(
            &current,
            &fresh,
            EvidenceSource::CogImpact,
            1.0,
            "fresh impact",
        );

        let ranked = Recommender::new(RecommenderConfig::default()).recommend_with_recent_events(
            &trigger,
            &StaticCogAdapter::new(),
            &graph,
            vec![recent_seen],
        );

        assert_eq!(ranked[0].entity.qualified_name, "fresh");
    }

    fn trigger() -> TrajectoryEvent {
        event(
            "evt",
            TrajectoryKind::ReadEntity,
            Some(EntityRef::new("current")),
        )
    }

    fn event(id: &str, kind: TrajectoryKind, entity_ref: Option<EntityRef>) -> TrajectoryEvent {
        TrajectoryEvent {
            id: id.into(),
            raw_event_id: format!("raw-{id}"),
            session_id: "session".into(),
            kind,
            entity_ref,
            file_path: None,
            line_range: None,
            payload: Value::Null,
            confidence: 0.8,
        }
    }

    fn entity(name: &str) -> EntityRef {
        EntityRef::new(name)
            .with_kind(EntityKind::Function)
            .with_confidence(0.8)
    }
}
