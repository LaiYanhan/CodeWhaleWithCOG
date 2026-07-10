use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::json;

use super::config::RecommenderConfig;
use super::graph::EdgeStats;
use super::storage::{
    DEFAULT_RECOMMENDER_DB_NAME, SqliteTrajectoryRepository, TrajectoryRepository,
};
use super::types::{EntityKind, EntityRef, EvidenceSource, SuggestedAction};
use super::visualization::VisualizationScope;

#[derive(Debug, Clone, Serialize)]
pub struct RecommendationSummary {
    pub scope: VisualizationScope,
    pub generated_at: DateTime<Utc>,
    pub default_weights: RecommendationWeights,
    pub records: Vec<RecommendationSummaryRecord>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct RecommendationWeights {
    pub cog_graph: f64,
    pub trajectory: f64,
    pub error: f64,
    pub search: f64,
    pub risk: f64,
    pub confidence_bonus: f64,
    pub already_seen_penalty: f64,
    pub self_target_penalty: f64,
    pub low_confidence_penalty: f64,
}

impl RecommendationWeights {
    pub fn from_config(config: &RecommenderConfig) -> Self {
        Self {
            cog_graph: config.cog_graph_weight,
            trajectory: config.trajectory_weight,
            error: config.error_weight,
            search: config.search_weight,
            risk: config.risk_weight,
            confidence_bonus: 0.05,
            already_seen_penalty: config.already_seen_penalty,
            self_target_penalty: 0.20,
            low_confidence_penalty: 0.10,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RecommendationSummaryRecord {
    pub entity: EntityRef,
    pub suggested_action: SuggestedAction,
    pub server_score: f64,
    pub score_parts: RecommendationScoreParts,
    pub evidence: Vec<RecommendationEvidenceSummary>,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct RecommendationScoreParts {
    pub cog_graph: f64,
    pub trajectory: f64,
    pub error: f64,
    pub search: f64,
    pub risk: f64,
    pub confidence_bonus: f64,
    pub already_seen_penalty: f64,
    pub self_target_penalty: f64,
    pub low_confidence_penalty: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecommendationEvidenceSummary {
    pub source: EvidenceSource,
    pub weight: f64,
    pub reason: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct RecommendationSummaryStore {
    workspace: PathBuf,
}

impl RecommendationSummaryStore {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
        }
    }

    pub fn load_summary(&self, scope: VisualizationScope, limit: usize) -> RecommendationSummary {
        let mut warnings = Vec::new();
        let weights = RecommendationWeights::from_config(&RecommenderConfig::default());
        let edges = match self.load_edges() {
            Ok(edges) => edges,
            Err(err) => {
                warnings.push(format!("failed to load trajectory edges: {err}"));
                Vec::new()
            }
        };
        let cog_entities = match self.load_cog_entities() {
            Ok(entities) => entities,
            Err(err) => {
                warnings.push(format!(
                    "failed to load COG entities for recommendation projection: {err}"
                ));
                Vec::new()
            }
        };

        let mut records = records_from_edges(edges, weights, &cog_entities, &mut warnings);
        records.sort_by(|left, right| {
            right
                .server_score
                .partial_cmp(&left.server_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.entity.qualified_name.cmp(&right.entity.qualified_name))
        });
        records.truncate(limit);
        if records.is_empty() {
            warnings.push("no recommendation records available".to_string());
        }
        aggregate_projection_warnings(&mut warnings);
        warnings.sort();
        warnings.dedup();

        RecommendationSummary {
            scope,
            generated_at: Utc::now(),
            default_weights: weights,
            records,
            warnings,
        }
    }

    fn load_edges(&self) -> anyhow::Result<Vec<EdgeStats>> {
        let path = self
            .workspace
            .join(".cog")
            .join(DEFAULT_RECOMMENDER_DB_NAME);
        if !path.exists() {
            anyhow::bail!("{} does not exist", path.display());
        }
        let repo = SqliteTrajectoryRepository::open(&path)?;
        repo.list_trajectory_edges()
    }

    fn load_cog_entities(&self) -> anyhow::Result<Vec<EntityRef>> {
        let path = self.workspace.join(".cog").join("cog.db");
        if !path.exists() {
            anyhow::bail!("{} does not exist", path.display());
        }
        load_cog_entities_from_db(&path)
    }
}

fn aggregate_projection_warnings(warnings: &mut Vec<String>) {
    const PREFIX: &str = "trajectory target '";
    let mut targets = warnings
        .iter()
        .filter(|warning| warning.starts_with(PREFIX))
        .filter_map(|warning| warning.strip_prefix(PREFIX))
        .filter_map(|warning| warning.split('\'').next())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    targets.sort();
    targets.dedup();
    if targets.is_empty() {
        return;
    }
    warnings.retain(|warning| !warning.starts_with(PREFIX));
    let examples = targets
        .iter()
        .take(4)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    warnings.push(format!(
        "{} stale or temporary trajectory target(s) were skipped because they are absent from the current COG graph. Examples: {}",
        targets.len(),
        examples
    ));
}

fn records_from_edges(
    edges: Vec<EdgeStats>,
    weights: RecommendationWeights,
    cog_entities: &[EntityRef],
    warnings: &mut Vec<String>,
) -> Vec<RecommendationSummaryRecord> {
    let mut grouped: HashMap<String, (EntityRef, Vec<RecommendationEvidenceSummary>)> =
        HashMap::new();
    for edge in edges {
        let targets = recommendation_targets_for_edge(&edge, cog_entities, warnings);
        for target in targets {
            let key = target
                .cog_entity_id
                .clone()
                .unwrap_or_else(|| target.qualified_name.clone());
            let evidence_weight = if target.qualified_name == edge.target.qualified_name {
                edge.weight
            } else {
                (edge.weight * 0.8).min(1.0)
            };
            grouped
                .entry(key)
                .or_insert_with(|| (target.clone(), Vec::new()))
                .1
                .push(RecommendationEvidenceSummary {
                    source: edge.edge_type,
                    weight: evidence_weight,
                    reason: projected_reason(&edge, &target),
                    payload: json!({
                        "source_entity": edge.source.qualified_name,
                        "original_target_entity": edge.target.qualified_name,
                        "projected_entity": target.qualified_name,
                        "projected_from_file": target.qualified_name != edge.target.qualified_name,
                        "count": edge.count,
                        "last_seen_ts": edge.last_seen_ts,
                    }),
                });
        }
    }

    grouped
        .into_values()
        .filter_map(|(entity, mut evidence)| {
            evidence.sort_by(|left, right| {
                right
                    .weight
                    .partial_cmp(&left.weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| format!("{:?}", left.source).cmp(&format!("{:?}", right.source)))
            });
            let score_parts = score_parts(&entity, &evidence);
            let server_score = score_from_parts(score_parts, weights);
            if server_score <= 0.0 || evidence.iter().all(|item| item.reason.trim().is_empty()) {
                return None;
            }
            Some(RecommendationSummaryRecord {
                suggested_action: suggested_action(&evidence),
                entity,
                server_score,
                score_parts,
                evidence,
            })
        })
        .collect()
}

fn load_cog_entities_from_db(path: &Path) -> anyhow::Result<Vec<EntityRef>> {
    let conn = Connection::open(path)?;
    let mut stmt =
        conn.prepare("SELECT id, qualified_name, kind FROM entities ORDER BY qualified_name")?;
    let rows = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let name: String = row.get(1)?;
        let kind_text: String = row.get(2)?;
        Ok(EntityRef {
            cog_entity_id: Some(id),
            qualified_name: name,
            kind: entity_kind_from_cog(&kind_text),
            file_path: None,
            confidence: 0.85,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn recommendation_targets_for_edge(
    edge: &EdgeStats,
    cog_entities: &[EntityRef],
    warnings: &mut Vec<String>,
) -> Vec<EntityRef> {
    if is_recommendable_entity(&edge.target) {
        return vec![edge.target.clone()];
    }
    let projected = project_observed_target_to_cog_entities(&edge.target, cog_entities);
    if projected.is_empty() && !cog_entities.is_empty() {
        warnings.push(format!(
            "trajectory target '{}' could not be projected to COG code entities",
            edge.target.qualified_name
        ));
    }
    projected
}

fn project_observed_target_to_cog_entities(
    target: &EntityRef,
    cog_entities: &[EntityRef],
) -> Vec<EntityRef> {
    let Some(path_like) = target
        .file_path
        .as_deref()
        .or_else(|| path_like_value(&target.qualified_name))
    else {
        return Vec::new();
    };
    let normalized_path = normalize_entity_like_path(path_like);
    let file_stem = Path::new(path_like)
        .file_stem()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());
    let mut candidates = cog_entities
        .iter()
        .filter(|entity| is_recommendable_entity(entity))
        .filter(|entity| {
            entity_matches_path(
                &normalize_entity_name(&entity.qualified_name),
                &normalized_path,
                path_like,
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.qualified_name.cmp(&right.qualified_name));
    candidates.dedup_by(|left, right| left.qualified_name == right.qualified_name);
    if candidates.len() > 25 {
        let module_candidates = candidates
            .iter()
            .filter(|entity| entity.kind == EntityKind::Module)
            .filter(|entity| {
                file_stem.as_deref().is_some_and(|stem| {
                    normalize_entity_name(&entity.qualified_name)
                        .rsplit("::")
                        .next()
                        == Some(stem)
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        if module_candidates.len() == 1 {
            candidates = module_candidates;
        } else {
            return Vec::new();
        }
    }
    candidates
        .into_iter()
        .map(|entity| EntityRef {
            file_path: target.file_path.clone(),
            confidence: entity.confidence.max(target.confidence).max(0.55),
            ..entity
        })
        .collect()
}

fn projected_reason(edge: &EdgeStats, target: &EntityRef) -> String {
    if target.qualified_name == edge.target.qualified_name {
        return edge.reason.clone();
    }
    format!(
        "{}; projected from observed target '{}'",
        edge.reason, edge.target.qualified_name
    )
}

fn is_recommendable_entity(entity: &EntityRef) -> bool {
    matches!(
        entity.kind,
        EntityKind::Function | EntityKind::Method | EntityKind::Type | EntityKind::Module
    ) && !path_like_value(&entity.qualified_name).is_some()
}

fn path_like_value(value: &str) -> Option<&str> {
    if value.contains('/') || value.contains('\\') {
        return Some(value);
    }
    let lower = value.to_ascii_lowercase();
    if [
        ".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".go", ".java", ".cpp", ".c", ".h",
    ]
    .iter()
    .any(|suffix| lower.ends_with(suffix))
    {
        return Some(value);
    }
    None
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

fn normalize_entity_like_path(value: &str) -> String {
    value
        .trim()
        .trim_start_matches("./")
        .replace('\\', "/")
        .replace(['/', '.', '-'], "::")
        .to_ascii_lowercase()
}

fn normalize_entity_name(value: &str) -> String {
    value
        .replace(['/', '\\', '.', '-'], "::")
        .to_ascii_lowercase()
}

fn entity_matches_path(
    normalized_entity: &str,
    normalized_path: &str,
    original_path: &str,
) -> bool {
    if normalized_entity.contains(normalized_path) || normalized_path.contains(normalized_entity) {
        return true;
    }
    let Some(stem) = Path::new(original_path)
        .file_stem()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
    else {
        return false;
    };
    normalized_entity.split("::").any(|part| part == stem)
}

fn score_parts(
    entity: &EntityRef,
    evidence: &[RecommendationEvidenceSummary],
) -> RecommendationScoreParts {
    RecommendationScoreParts {
        cog_graph: capped_sum(evidence, |source| {
            matches!(
                source,
                EvidenceSource::CogImpact | EvidenceSource::CogRelation | EvidenceSource::Assertion
            )
        }),
        trajectory: capped_sum(evidence, |source| {
            matches!(
                source,
                EvidenceSource::CoAccess
                    | EvidenceSource::ReadBeforeEdit
                    | EvidenceSource::EditToTest
                    | EvidenceSource::CogWriteToEdit
            )
        }),
        error: capped_sum(evidence, |source| source == EvidenceSource::ErrorToEdit),
        search: capped_sum(evidence, |source| {
            matches!(
                source,
                EvidenceSource::SearchToRead | EvidenceSource::SearchToEdit
            )
        }),
        risk: capped_sum(evidence, |source| {
            matches!(source, EvidenceSource::Rule | EvidenceSource::Assertion)
        }),
        confidence_bonus: if entity.confidence >= 0.7 { 1.0 } else { 0.0 },
        low_confidence_penalty: if entity.confidence < 0.4 { 1.0 } else { 0.0 },
        ..RecommendationScoreParts::default()
    }
}

fn capped_sum(
    evidence: &[RecommendationEvidenceSummary],
    predicate: impl Fn(EvidenceSource) -> bool,
) -> f64 {
    evidence
        .iter()
        .filter(|item| predicate(item.source))
        .map(|item| item.weight)
        .sum::<f64>()
        .min(1.0)
}

fn score_from_parts(parts: RecommendationScoreParts, weights: RecommendationWeights) -> f64 {
    (weights.cog_graph * parts.cog_graph
        + weights.trajectory * parts.trajectory
        + weights.error * parts.error
        + weights.search * parts.search
        + weights.risk * parts.risk
        + weights.confidence_bonus * parts.confidence_bonus
        - weights.already_seen_penalty * parts.already_seen_penalty
        - weights.self_target_penalty * parts.self_target_penalty
        - weights.low_confidence_penalty * parts.low_confidence_penalty)
        .clamp(0.0, 1.0)
}

fn suggested_action(evidence: &[RecommendationEvidenceSummary]) -> SuggestedAction {
    if evidence
        .iter()
        .any(|item| item.source == EvidenceSource::ErrorToEdit)
    {
        return SuggestedAction::UpdateRelatedCode;
    }
    if evidence
        .iter()
        .any(|item| item.source == EvidenceSource::EditToTest)
    {
        return SuggestedAction::RunTest;
    }
    if evidence.iter().any(|item| {
        matches!(
            item.source,
            EvidenceSource::CogImpact | EvidenceSource::CogRelation
        )
    }) {
        return SuggestedAction::InspectImpact;
    }
    SuggestedAction::Read
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::cog_recommender::graph::TrajectoryGraph;
    use crate::cog_recommender::storage::TrajectoryRepository;
    use crate::cog_recommender::types::{EntityKind, EntityRef};

    #[test]
    fn summary_returns_empty_records_and_warning_without_db() {
        let workspace = tempdir().expect("workspace");

        let summary = RecommendationSummaryStore::new(workspace.path())
            .load_summary(VisualizationScope::Session, 100);

        assert!(summary.records.is_empty());
        assert!(!summary.warnings.is_empty());
        assert_eq!(summary.default_weights.cog_graph, 0.35);
    }

    #[test]
    fn summary_groups_edges_into_score_parts() {
        let workspace = tempdir().expect("workspace");
        let db_path = workspace
            .path()
            .join(".cog")
            .join(DEFAULT_RECOMMENDER_DB_NAME);
        let repo = SqliteTrajectoryRepository::open(&db_path).expect("repo");
        let source = entity("src::service");
        let target = entity("src::api");
        let mut graph = TrajectoryGraph::default();
        let edge = graph.observe_edge(
            &source,
            &target,
            EvidenceSource::ReadBeforeEdit,
            0.5,
            "read before edit",
        );
        repo.upsert_trajectory_edge(&edge).expect("edge");

        let summary = RecommendationSummaryStore::new(workspace.path())
            .load_summary(VisualizationScope::Session, 100);

        assert_eq!(summary.records.len(), 1);
        assert_eq!(summary.records[0].entity.qualified_name, "src::api");
        assert_eq!(summary.records[0].score_parts.trajectory, 0.5);
        assert!(summary.records[0].server_score > 0.0);
    }

    #[test]
    fn summary_projects_file_edge_targets_to_cog_code_entities() {
        let workspace = tempdir().expect("workspace");
        write_cog_entities(
            workspace.path(),
            &[
                ("entity-1", "auth::AuthService::login", "method"),
                ("entity-2", "auth::AuthState", "type"),
                ("file-1", "src/auth.rs", "file"),
            ],
        );
        let db_path = workspace
            .path()
            .join(".cog")
            .join(DEFAULT_RECOMMENDER_DB_NAME);
        let repo = SqliteTrajectoryRepository::open(&db_path).expect("repo");
        let source = entity("search:AuthService");
        let target = EntityRef::new("src/auth.rs")
            .with_kind(EntityKind::File)
            .with_file_path("src/auth.rs")
            .with_confidence(0.4);
        let mut graph = TrajectoryGraph::default();
        let edge = graph.observe_edge(
            &source,
            &target,
            EvidenceSource::SearchToEdit,
            0.5,
            "search often leads to editing auth",
        );
        repo.upsert_trajectory_edge(&edge).expect("edge");

        let summary = RecommendationSummaryStore::new(workspace.path())
            .load_summary(VisualizationScope::Session, 100);
        let names = summary
            .records
            .iter()
            .map(|record| record.entity.qualified_name.as_str())
            .collect::<Vec<_>>();

        assert!(names.contains(&"auth::AuthService::login"));
        assert!(names.contains(&"auth::AuthState"));
        assert!(!names.contains(&"src/auth.rs"));
        assert!(
            summary
                .records
                .iter()
                .all(|record| record.entity.kind != EntityKind::File)
        );
        assert!(summary.records.iter().any(|record| {
            record.evidence.iter().any(|evidence| {
                evidence
                    .payload
                    .get("original_target_entity")
                    .and_then(serde_json::Value::as_str)
                    == Some("src/auth.rs")
            })
        }));
    }

    #[test]
    fn summary_does_not_emit_file_records_without_projection() {
        let workspace = tempdir().expect("workspace");
        write_cog_entities(
            workspace.path(),
            &[("entity-1", "billing::charge", "function")],
        );
        let db_path = workspace
            .path()
            .join(".cog")
            .join(DEFAULT_RECOMMENDER_DB_NAME);
        let repo = SqliteTrajectoryRepository::open(&db_path).expect("repo");
        let source = entity("search:AuthService");
        let target = EntityRef::new("src/auth.rs")
            .with_kind(EntityKind::File)
            .with_file_path("src/auth.rs")
            .with_confidence(0.4);
        let mut graph = TrajectoryGraph::default();
        let edge = graph.observe_edge(
            &source,
            &target,
            EvidenceSource::SearchToEdit,
            0.5,
            "search often leads to editing auth",
        );
        repo.upsert_trajectory_edge(&edge).expect("edge");

        let summary = RecommendationSummaryStore::new(workspace.path())
            .load_summary(VisualizationScope::Session, 100);

        assert!(summary.records.is_empty());
        assert!(
            summary
                .warnings
                .iter()
                .any(|warning| { warning.contains("stale or temporary trajectory target") })
        );
    }

    #[test]
    fn summary_projects_absolute_path_for_large_file_to_exact_module_once() {
        let workspace = tempdir().expect("workspace");
        let mut rows = vec![("module-main", "main", "module")];
        let owned = (0..30)
            .map(|index| {
                (
                    format!("method-{index}"),
                    format!("main::Window::handler_{index}"),
                    "method".to_string(),
                )
            })
            .collect::<Vec<_>>();
        let owned_refs = owned
            .iter()
            .map(|(id, name, kind)| (id.as_str(), name.as_str(), kind.as_str()))
            .collect::<Vec<_>>();
        rows.extend(owned_refs);
        write_cog_entities(workspace.path(), &rows);

        let db_path = workspace
            .path()
            .join(".cog")
            .join(DEFAULT_RECOMMENDER_DB_NAME);
        let repo = SqliteTrajectoryRepository::open(&db_path).expect("repo");
        let source = entity("search:main");
        let target = EntityRef::new(r"D:\project\game_translator\main.py")
            .with_kind(EntityKind::File)
            .with_file_path(r"D:\project\game_translator\main.py")
            .with_confidence(0.4);
        let mut graph = TrajectoryGraph::default();
        for edge_type in [EvidenceSource::SearchToEdit, EvidenceSource::CoAccess] {
            let edge = graph.observe_edge(&source, &target, edge_type, 0.5, "observed main.py");
            repo.upsert_trajectory_edge(&edge).expect("edge");
        }

        let summary = RecommendationSummaryStore::new(workspace.path())
            .load_summary(VisualizationScope::Session, 100);

        assert_eq!(summary.records.len(), 1);
        assert_eq!(summary.records[0].entity.qualified_name, "main");
        assert!(
            summary
                .warnings
                .iter()
                .all(|warning| !warning.contains("could not be projected"))
        );
    }

    #[test]
    fn summary_aggregates_unprojectable_targets_into_one_warning() {
        let mut warnings = vec![
            "trajectory target '_a.py' could not be projected to COG code entities".to_string(),
            "trajectory target '_a.py' could not be projected to COG code entities".to_string(),
            "trajectory target '_b.py' could not be projected to COG code entities".to_string(),
        ];
        aggregate_projection_warnings(&mut warnings);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("2 stale or temporary"));
        assert!(warnings[0].contains("_a.py"));
        assert!(warnings[0].contains("_b.py"));
    }

    fn entity(name: &str) -> EntityRef {
        EntityRef::new(name)
            .with_kind(EntityKind::Function)
            .with_confidence(0.8)
    }

    fn write_cog_entities(workspace: &std::path::Path, rows: &[(&str, &str, &str)]) {
        let cog_dir = workspace.join(".cog");
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
        for (id, name, kind) in rows {
            conn.execute(
                "INSERT INTO entities (id, qualified_name, kind) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, name, kind],
            )
            .expect("insert entity");
        }
    }
}
