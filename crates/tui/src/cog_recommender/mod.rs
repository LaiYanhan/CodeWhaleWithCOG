//! COG-backed trajectory recommendation system.
//!
//! This module is intentionally kept separate from the standalone `cog/` CLI
//! tree. It observes CodeWhale tool activity, maps it onto COG entities, and
//! ranks recommendations from COG graph evidence plus historical trajectories.

#![allow(dead_code)]

pub mod algorithms;
pub mod candidate_sources;
pub mod cog_adapter;
pub mod collector;
pub mod config;
pub mod feedback;
pub mod graph;
pub mod normalizer;
pub mod recommendation_context;
pub mod recommendation_summary;
pub mod recommender;
pub mod resolver;
pub mod runtime;
pub mod storage;
pub mod types;
pub mod visualization;
pub mod visualization_web;

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;

    use super::cog_adapter::StaticCogAdapter;
    use super::config::RecommenderConfig;
    use super::graph::TrajectoryGraph;
    use super::normalizer::normalize_tool_event;
    use super::recommender::Recommender;
    use super::resolver::resolve_event_entity;
    use super::types::{
        EntityKind, EntityRef, Evidence, EvidenceSource, RawToolEvent, ToolEventOrigin,
        ToolEventStatus,
    };

    #[test]
    fn edit_event_recommendations_merge_cog_and_trajectory_evidence() {
        let service = EntityRef::new("inventory::service::InventoryService::get_stock")
            .with_kind(EntityKind::Method)
            .with_file_path("src/inventory/service.py")
            .with_confidence(0.9);
        let api = EntityRef::new("inventory::api::get_stock_status")
            .with_kind(EntityKind::Function)
            .with_file_path("src/inventory/api.py")
            .with_confidence(0.9);
        let model = EntityRef::new("inventory::models::StockItem")
            .with_kind(EntityKind::Type)
            .with_file_path("src/inventory/models.py")
            .with_confidence(0.8);

        let adapter = StaticCogAdapter::new()
            .with_file("src/inventory/service.py", service.clone())
            .with_impact(
                "inventory::service::InventoryService::get_stock",
                Evidence::new(
                    EvidenceSource::CogImpact,
                    api.clone(),
                    0.4,
                    "api calls the edited service method",
                ),
            );

        let raw = RawToolEvent {
            id: "evt_edit".into(),
            session_id: "s1".into(),
            turn_id: "t1".into(),
            ts: Utc::now(),
            tool_name: "apply_patch".into(),
            input_summary: json!({"path": "src/inventory/service.py"}),
            output_summary: "patched".into(),
            status: ToolEventStatus::Success,
            duration_ms: 12,
            origin: ToolEventOrigin::Agent,
        };

        let mut events = normalize_tool_event(&raw);
        let event = events.pop().expect("edit event");
        let resolved = resolve_event_entity(event, &adapter);

        let mut graph = TrajectoryGraph::default();
        graph.observe_edge(
            &service,
            &model,
            EvidenceSource::CoAccess,
            0.25,
            "historically co-accessed with edited service",
        );

        let recommender = Recommender::new(RecommenderConfig::default());
        let recommendations = recommender.recommend(&resolved, &adapter, &graph);

        assert_eq!(recommendations.len(), 2);
        assert_eq!(
            recommendations[0].entity.qualified_name,
            "inventory::api::get_stock_status"
        );
        assert!(
            recommendations
                .iter()
                .any(|r| r.entity.qualified_name == "inventory::models::StockItem")
        );
        assert!(
            recommendations[0]
                .evidence
                .iter()
                .any(|e| e.source == EvidenceSource::CogImpact)
        );
    }
}
