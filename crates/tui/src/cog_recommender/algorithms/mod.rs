pub mod weighted_v1;

use super::recommendation_context::RecommendationContext;
use super::types::{Candidate, Recommendation};

pub trait RecommendationAlgorithm {
    fn recommend(
        &self,
        context: &RecommendationContext,
        candidates: Vec<Candidate>,
    ) -> Vec<Recommendation>;
}
