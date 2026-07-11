use std::cmp::Ordering;
use std::collections::HashMap;

use super::RecommendationAlgorithm;
use crate::cog_recommender::recommendation_context::{RecommendationContext, entity_key};
use crate::cog_recommender::types::{
    Candidate, EntityRef, Evidence, EvidenceSource, Recommendation, SuggestedAction, TrajectoryKind,
};

#[derive(Debug, Clone, Default)]
pub struct WeightedV1Algorithm;

impl RecommendationAlgorithm for WeightedV1Algorithm {
    fn recommend(
        &self,
        context: &RecommendationContext,
        candidates: Vec<Candidate>,
    ) -> Vec<Recommendation> {
        let mut recommendations = merge_candidates(candidates)
            .into_iter()
            .filter_map(|candidate| to_recommendation(context, candidate))
            .collect::<Vec<_>>();
        recommendations.sort_by(compare_recommendations);
        recommendations.truncate(context.config.max_recommendations);
        recommendations
    }
}

fn merge_candidates(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut merged: HashMap<String, Candidate> = HashMap::new();
    for candidate in candidates {
        let key = candidate_key(&candidate);
        merged
            .entry(key)
            .and_modify(|existing| existing.evidence.extend(candidate.evidence.clone()))
            .or_insert(candidate);
    }

    merged
        .into_values()
        .map(|mut candidate| {
            candidate.evidence = compact_evidence(candidate.evidence);
            candidate
        })
        .collect()
}

fn candidate_key(candidate: &Candidate) -> String {
    candidate.entity.cog_entity_id.clone().unwrap_or_else(|| {
        if !candidate.entity.qualified_name.is_empty() {
            candidate.entity.qualified_name.clone()
        } else {
            candidate.entity.file_path.clone().unwrap_or_default()
        }
    })
}

fn compact_evidence(evidence: Vec<Evidence>) -> Vec<Evidence> {
    let mut grouped: HashMap<EvidenceSource, Vec<Evidence>> = HashMap::new();
    for item in evidence {
        grouped.entry(item.source).or_default().push(item);
    }

    let mut compacted = Vec::new();
    for mut group in grouped.into_values() {
        group.sort_by(|a, b| {
            b.weight
                .partial_cmp(&a.weight)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.reason.cmp(&b.reason))
        });
        compacted.extend(group.into_iter().take(3));
    }
    compacted.sort_by(|a, b| {
        evidence_priority(a.source)
            .cmp(&evidence_priority(b.source))
            .then_with(|| b.weight.partial_cmp(&a.weight).unwrap_or(Ordering::Equal))
    });
    compacted
}

fn to_recommendation(
    context: &RecommendationContext,
    candidate: Candidate,
) -> Option<Recommendation> {
    if candidate.evidence.is_empty() || display_reasons(&candidate.evidence).is_empty() {
        return None;
    }

    let score = score_candidate(context, &candidate);
    if score < context.config.min_score {
        return None;
    }

    let suggested_action = suggested_action(context, &candidate.evidence);
    let display_text = format_display(suggested_action, &candidate.entity, &candidate.evidence);
    Some(Recommendation {
        entity: candidate.entity,
        score,
        evidence: candidate.evidence,
        suggested_action,
        display_text,
    })
}

fn score_candidate(context: &RecommendationContext, candidate: &Candidate) -> f64 {
    let cog_graph_score = capped_sum(&candidate.evidence, |source| {
        matches!(
            source,
            EvidenceSource::CogImpact
                | EvidenceSource::CogRelation
                | EvidenceSource::EntityAdded
                | EvidenceSource::EntityDeleted
                | EvidenceSource::Assertion
        )
    });
    let trajectory_score = capped_sum(&candidate.evidence, |source| {
        matches!(
            source,
            EvidenceSource::CoAccess
                | EvidenceSource::ReadBeforeEdit
                | EvidenceSource::EditToTest
                | EvidenceSource::CogWriteToEdit
        )
    });
    let error_score = capped_sum(&candidate.evidence, |source| {
        source == EvidenceSource::ErrorToEdit
    });
    let search_score = capped_sum(&candidate.evidence, |source| {
        matches!(
            source,
            EvidenceSource::SearchToRead | EvidenceSource::SearchToEdit
        )
    });
    let risk_score = capped_sum(&candidate.evidence, |source| {
        matches!(source, EvidenceSource::Rule | EvidenceSource::Assertion)
    });

    let confidence_bonus = if candidate.entity.confidence >= 0.7 {
        1.0
    } else {
        0.0
    };
    let already_seen_penalty = already_seen_penalty(context, &candidate.entity);
    let self_target_penalty = self_target_penalty(context, &candidate.entity);
    let low_confidence_penalty = if candidate.entity.confidence < 0.4 {
        1.0
    } else {
        0.0
    };

    (context.config.cog_graph_weight * cog_graph_score
        + context.config.trajectory_weight * trajectory_score
        + context.config.error_weight * error_score
        + context.config.search_weight * search_score
        + context.config.risk_weight * risk_score
        + 0.05 * confidence_bonus
        - context.config.already_seen_penalty * already_seen_penalty
        - 0.20 * self_target_penalty
        - 0.10 * low_confidence_penalty)
        .clamp(0.0, 1.0)
}

fn capped_sum(evidence: &[Evidence], predicate: impl Fn(EvidenceSource) -> bool) -> f64 {
    evidence
        .iter()
        .filter(|evidence| predicate(evidence.source))
        .map(|evidence| evidence.weight)
        .sum::<f64>()
        .min(1.0)
}

fn already_seen_penalty(context: &RecommendationContext, entity: &EntityRef) -> f64 {
    let key = entity_key(entity);
    if context.current_turn_seen_entities.contains(&key)
        || context
            .current_turn_seen_entities
            .contains(&entity.qualified_name)
    {
        return 1.0;
    }
    if context.current_session_seen_entities.contains(&key)
        || context
            .current_session_seen_entities
            .contains(&entity.qualified_name)
    {
        return 0.5;
    }
    0.0
}

fn self_target_penalty(context: &RecommendationContext, entity: &EntityRef) -> f64 {
    context
        .current_entity
        .as_ref()
        .is_some_and(|current| entity_key(current) == entity_key(entity))
        .then_some(1.0)
        .unwrap_or(0.0)
}

fn suggested_action(context: &RecommendationContext, evidence: &[Evidence]) -> SuggestedAction {
    if context.trigger.kind == TrajectoryKind::ErrorSignal {
        return SuggestedAction::UpdateRelatedCode;
    }
    if context.trigger.kind == TrajectoryKind::TestEntity
        && context
            .recent_events
            .iter()
            .any(|event| event.kind == TrajectoryKind::EditEntity)
    {
        return SuggestedAction::Verify;
    }

    let strongest = evidence
        .iter()
        .max_by(|a, b| a.weight.partial_cmp(&b.weight).unwrap_or(Ordering::Equal))
        .map(|evidence| evidence.source);

    match (context.trigger.kind, strongest) {
        (
            TrajectoryKind::EditEntity,
            Some(
                EvidenceSource::CogImpact
                | EvidenceSource::CogRelation
                | EvidenceSource::EntityAdded
                | EvidenceSource::EntityDeleted,
            ),
        ) => SuggestedAction::InspectImpact,
        (TrajectoryKind::EditEntity, Some(EvidenceSource::EditToTest | EvidenceSource::Rule)) => {
            SuggestedAction::RunTest
        }
        _ => SuggestedAction::Read,
    }
}

fn format_display(action: SuggestedAction, entity: &EntityRef, evidence: &[Evidence]) -> String {
    let action = action_text(action);
    let reasons = display_reasons(evidence);
    if reasons.is_empty() {
        format!("Consider {action} {}", entity.qualified_name)
    } else {
        format!(
            "Consider {action} {}: {}",
            entity.qualified_name,
            reasons.join("; ")
        )
    }
}

fn display_reasons(evidence: &[Evidence]) -> Vec<String> {
    let mut reasons = Vec::new();
    let mut used_sources = Vec::new();
    for item in evidence {
        if item.reason.trim().is_empty() || used_sources.contains(&item.source) {
            continue;
        }
        used_sources.push(item.source);
        reasons.push(format!("{:?}: {}", item.source, item.reason));
        if reasons.len() >= 3 {
            break;
        }
    }
    reasons
}

fn action_text(action: SuggestedAction) -> &'static str {
    match action {
        SuggestedAction::Read => "read",
        SuggestedAction::InspectImpact => "inspect impact of",
        SuggestedAction::RunTest => "run tests for",
        SuggestedAction::UpdateRelatedCode => "update related code for",
        SuggestedAction::Verify => "verify",
    }
}

fn evidence_priority(source: EvidenceSource) -> u8 {
    match source {
        EvidenceSource::CogImpact => 0,
        EvidenceSource::CogRelation => 1,
        EvidenceSource::EntityDeleted => 2,
        EvidenceSource::EntityAdded => 3,
        EvidenceSource::Assertion => 4,
        EvidenceSource::ErrorToEdit => 5,
        EvidenceSource::ReadBeforeEdit => 6,
        EvidenceSource::SearchToEdit => 7,
        EvidenceSource::SearchToRead => 8,
        EvidenceSource::EditToTest => 9,
        EvidenceSource::CogWriteToEdit => 10,
        EvidenceSource::CoAccess => 11,
        EvidenceSource::Rule => 12,
    }
}

fn compare_recommendations(left: &Recommendation, right: &Recommendation) -> Ordering {
    right
        .score
        .partial_cmp(&left.score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| left.entity.qualified_name.cmp(&right.entity.qualified_name))
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::cog_recommender::config::RecommenderConfig;
    use crate::cog_recommender::types::{EntityKind, TrajectoryEvent};

    #[test]
    fn merge_duplicate_candidates() {
        let target = entity("target", 0.8);
        let ranked = algorithm().recommend(
            &context(TrajectoryKind::ReadEntity, Some(entity("current", 0.8))),
            vec![
                candidate(&target, EvidenceSource::CogImpact, 0.7, "impact"),
                candidate(&target, EvidenceSource::CoAccess, 0.3, "co-access"),
            ],
        );

        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].evidence.len(), 2);
    }

    #[test]
    fn rank_cog_impact_above_co_access() {
        let cog = entity("cog_target", 0.8);
        let co_access = entity("co_access_target", 0.8);

        let ranked = algorithm().recommend(
            &context(TrajectoryKind::EditEntity, Some(entity("current", 0.8))),
            vec![
                candidate(&co_access, EvidenceSource::CoAccess, 1.0, "co-access"),
                candidate(&cog, EvidenceSource::CogImpact, 1.0, "impact"),
            ],
        );

        assert_eq!(ranked[0].entity.qualified_name, "cog_target");
    }

    #[test]
    fn penalize_seen_entity() {
        let seen = entity("seen", 0.8);
        let fresh = entity("fresh", 0.8);
        let mut context = context(TrajectoryKind::ReadEntity, Some(entity("current", 0.8)));
        context
            .current_turn_seen_entities
            .insert(seen.qualified_name.clone());

        let ranked = algorithm().recommend(
            &context,
            vec![
                candidate(&seen, EvidenceSource::CogImpact, 1.0, "seen impact"),
                candidate(&fresh, EvidenceSource::CogImpact, 1.0, "fresh impact"),
            ],
        );

        assert_eq!(ranked[0].entity.qualified_name, "fresh");
    }

    #[test]
    fn suppress_self_recommendation() {
        let current = entity("current", 0.8);
        let other = entity("other", 0.8);

        let ranked = algorithm().recommend(
            &context(TrajectoryKind::EditEntity, Some(current.clone())),
            vec![
                candidate(&current, EvidenceSource::CogImpact, 1.0, "self"),
                candidate(&other, EvidenceSource::CogImpact, 0.8, "other"),
            ],
        );

        assert_eq!(ranked[0].entity.qualified_name, "other");
    }

    #[test]
    fn recommend_from_search_to_edit() {
        let target = entity("search_target", 0.8);

        let ranked = algorithm().recommend(
            &context(TrajectoryKind::SearchEntity, None),
            vec![candidate(
                &target,
                EvidenceSource::SearchToEdit,
                1.0,
                "search led to edit",
            )],
        );

        assert_eq!(ranked[0].entity.qualified_name, "search_target");
    }

    #[test]
    fn recommend_from_error_to_edit() {
        let target = entity("error_target", 0.8);

        let ranked = algorithm().recommend(
            &context(TrajectoryKind::ErrorSignal, None),
            vec![candidate(
                &target,
                EvidenceSource::ErrorToEdit,
                1.0,
                "error led to edit",
            )],
        );

        assert_eq!(
            ranked[0].suggested_action,
            SuggestedAction::UpdateRelatedCode
        );
    }

    #[test]
    fn preserve_explainable_reasons() {
        let target = entity("target", 0.8);

        let ranked = algorithm().recommend(
            &context(TrajectoryKind::ReadEntity, Some(entity("current", 0.8))),
            vec![candidate(&target, EvidenceSource::CogImpact, 1.0, "")],
        );

        assert!(ranked.is_empty());
    }

    fn algorithm() -> WeightedV1Algorithm {
        WeightedV1Algorithm
    }

    fn context(kind: TrajectoryKind, current_entity: Option<EntityRef>) -> RecommendationContext {
        let trigger = TrajectoryEvent {
            id: "event".into(),
            raw_event_id: "raw".into(),
            session_id: "session".into(),
            kind,
            entity_ref: current_entity,
            file_path: None,
            line_range: None,
            payload: Value::Null,
            confidence: 0.8,
        };
        RecommendationContext::new(trigger, RecommenderConfig::default())
    }

    fn entity(name: &str, confidence: f64) -> EntityRef {
        EntityRef::new(name)
            .with_kind(EntityKind::Function)
            .with_confidence(confidence)
    }

    fn candidate(
        target: &EntityRef,
        source: EvidenceSource,
        weight: f64,
        reason: &str,
    ) -> Candidate {
        Candidate {
            entity: target.clone(),
            trigger_event_id: "event".into(),
            evidence: vec![Evidence::new(source, target.clone(), weight, reason)],
        }
    }
}
