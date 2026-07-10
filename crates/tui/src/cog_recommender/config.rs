use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecommenderConfig {
    pub enabled: bool,
    pub cog_graph_weight: f64,
    pub trajectory_weight: f64,
    pub error_weight: f64,
    pub search_weight: f64,
    pub risk_weight: f64,
    pub already_seen_penalty: f64,
    pub max_recommendations: usize,
    pub min_score: f64,
    pub min_injection_score: f64,
    pub max_total_chars: usize,
    pub max_reason_chars: usize,
    pub max_injections_per_turn: usize,
}

impl Default for RecommenderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cog_graph_weight: 0.35,
            trajectory_weight: 0.25,
            error_weight: 0.20,
            search_weight: 0.10,
            risk_weight: 0.10,
            already_seen_penalty: 0.15,
            max_recommendations: 5,
            min_score: 0.05,
            min_injection_score: 0.20,
            max_total_chars: 1200,
            max_reason_chars: 180,
            max_injections_per_turn: 1,
        }
    }
}
