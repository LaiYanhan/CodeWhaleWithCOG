use std::collections::HashMap;

use super::cog_adapter::CogAdapter;
use super::config::RecommenderConfig;
use super::graph::TrajectoryGraph;
use super::types::{
    Candidate, EntityRef, Evidence, EvidenceSource, Recommendation, SuggestedAction,
    TrajectoryEvent,
};

pub struct Recommender {
    config: RecommenderConfig,
}

impl Recommender {
    pub fn new(config: RecommenderConfig) -> Self {
        Self { config }
    }

    pub fn recommend(
        &self,
        trigger: &TrajectoryEvent,
        adapter: &impl CogAdapter,
        graph: &TrajectoryGraph,
    ) -> Vec<Recommendation> {
        let Some(current) = trigger.entity_ref.as_ref() else {
            return Vec::new();
        };

        let mut candidates = Vec::new();
        candidates.extend(self.candidates_from_evidence(trigger, adapter.impact(current)));
        candidates.extend(self.candidates_from_evidence(trigger, adapter.related(current)));
        candidates.extend(self.candidates_from_evidence(trigger, adapter.assertions(current)));
        candidates.extend(self.candidates_from_evidence(trigger, graph.evidence_for(current)));

        self.rank(candidates)
    }

    fn candidates_from_evidence(
        &self,
        trigger: &TrajectoryEvent,
        evidence: Vec<Evidence>,
    ) -> Vec<Candidate> {
        evidence
            .into_iter()
            .map(|evidence| Candidate {
                entity: evidence.target.clone(),
                trigger_event_id: trigger.id.clone(),
                evidence: vec![evidence],
            })
            .collect()
    }

    fn rank(&self, candidates: Vec<Candidate>) -> Vec<Recommendation> {
        let mut merged: HashMap<String, Candidate> = HashMap::new();
        for candidate in candidates {
            merged
                .entry(candidate.entity.qualified_name.clone())
                .and_modify(|existing| existing.evidence.extend(candidate.evidence.clone()))
                .or_insert(candidate);
        }

        let mut recommendations: Vec<Recommendation> = merged
            .into_values()
            .filter_map(|candidate| self.to_recommendation(candidate))
            .collect();
        recommendations.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.entity.qualified_name.cmp(&b.entity.qualified_name))
        });
        recommendations.truncate(self.config.max_recommendations);
        recommendations
    }

    fn to_recommendation(&self, candidate: Candidate) -> Option<Recommendation> {
        if candidate.evidence.is_empty() {
            return None;
        }
        let score: f64 = candidate
            .evidence
            .iter()
            .map(|evidence| self.weighted_score(evidence))
            .sum();
        if score < self.config.min_score {
            return None;
        }
        let display_text = format_display(&candidate.entity, &candidate.evidence);
        Some(Recommendation {
            entity: candidate.entity,
            score,
            evidence: candidate.evidence,
            suggested_action: SuggestedAction::Read,
            display_text,
        })
    }

    fn weighted_score(&self, evidence: &Evidence) -> f64 {
        let multiplier = match evidence.source {
            EvidenceSource::CogImpact | EvidenceSource::CogRelation => self.config.cog_graph_weight,
            EvidenceSource::CoAccess
            | EvidenceSource::ReadBeforeEdit
            | EvidenceSource::SearchToRead
            | EvidenceSource::EditToTest
            | EvidenceSource::CogWriteToEdit => self.config.trajectory_weight,
            EvidenceSource::SearchToEdit => self.config.search_weight,
            EvidenceSource::ErrorToEdit => self.config.error_weight,
            EvidenceSource::Rule => self.config.risk_weight,
            EvidenceSource::Assertion => self.config.risk_weight,
        };
        evidence.weight * multiplier
    }
}

fn format_display(entity: &EntityRef, evidence: &[Evidence]) -> String {
    let reasons: Vec<&str> = evidence
        .iter()
        .map(|evidence| evidence.reason.as_str())
        .filter(|reason| !reason.is_empty())
        .take(3)
        .collect();
    if reasons.is_empty() {
        format!("Consider {}", entity.qualified_name)
    } else {
        format!("Consider {}: {}", entity.qualified_name, reasons.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cog_recommender::types::{EntityRef, EvidenceSource};

    #[test]
    fn rank_merges_duplicate_candidates() {
        let target = EntityRef::new("target");
        let candidate_a = Candidate {
            entity: target.clone(),
            trigger_event_id: "evt".into(),
            evidence: vec![Evidence::new(
                EvidenceSource::CogImpact,
                target.clone(),
                0.4,
                "impact",
            )],
        };
        let candidate_b = Candidate {
            entity: target.clone(),
            trigger_event_id: "evt".into(),
            evidence: vec![Evidence::new(
                EvidenceSource::CoAccess,
                target,
                0.3,
                "co-access",
            )],
        };

        let ranked =
            Recommender::new(RecommenderConfig::default()).rank(vec![candidate_a, candidate_b]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].evidence.len(), 2);
    }
}
