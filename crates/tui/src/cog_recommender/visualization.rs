use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use anyhow::{Context, Result};
use regex::Regex;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::storage::{DEFAULT_RECOMMENDER_DB_NAME, SqliteTrajectoryRepository};
use super::types::{EntityChange, EntityChangeKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VisualizationScope {
    Turn,
    Session,
}

impl Default for VisualizationScope {
    fn default() -> Self {
        Self::Turn
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VisualizationGraph {
    pub entities: Vec<VisualizationEntity>,
    pub relations: Vec<VisualizationRelation>,
    pub modified_entities: Vec<String>,
    pub added_entities: Vec<String>,
    pub deleted_entities: Vec<String>,
    pub impacted_entities: Vec<String>,
    pub tool_chain: Vec<ToolChainNode>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VisualizationEntity {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub kind: String,
    pub file_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VisualizationRelation {
    pub id: String,
    pub source: String,
    pub target: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolChainNode {
    pub id: String,
    pub tool_name: String,
    pub kind: String,
    pub target: Option<String>,
    pub status: String,
    pub ts: String,
    pub input_summary: Value,
    pub output_summary: String,
}

#[derive(Debug, Clone)]
struct TrajectoryRecord {
    raw_event_id: String,
    turn_id: String,
    session_id: String,
    kind: String,
    entity_id: Option<String>,
    entity_name: Option<String>,
    file_path: Option<String>,
    payload: Value,
    raw_input: Value,
    raw_output: String,
}

#[derive(Debug, Clone)]
struct RawEventRecord {
    id: String,
    session_id: String,
    turn_id: String,
    ts: String,
    tool_name: String,
    input_summary: Value,
    output_summary: String,
    status: String,
}

#[derive(Debug, Clone)]
pub struct VisualizationStore {
    workspace: PathBuf,
}

impl VisualizationStore {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
        }
    }

    pub fn load_graph(
        &self,
        scope: VisualizationScope,
        include_contains: bool,
        limit: usize,
    ) -> VisualizationGraph {
        let mut warnings = Vec::new();
        let (entities, relations) = match self.load_cog_graph(include_contains) {
            Ok(graph) => graph,
            Err(err) => {
                warnings.push(format!("failed to load COG graph: {err}"));
                (Vec::new(), Vec::new())
            }
        };

        let raw_events = match self.load_recent_raw_events(limit) {
            Ok(events) => events,
            Err(err) => {
                warnings.push(format!("failed to load tool events: {err}"));
                Vec::new()
            }
        };
        let trajectory_events = match self.load_recent_trajectory_events(limit) {
            Ok(events) => events,
            Err(err) => {
                warnings.push(format!("failed to load trajectory events: {err}"));
                Vec::new()
            }
        };
        let entity_changes =
            match self.load_recent_entity_changes(VisualizationScope::Session, limit) {
                Ok(changes) => changes,
                Err(err) => {
                    warnings.push(format!("failed to load entity changes: {err}"));
                    Vec::new()
                }
            };

        let latest_turn = raw_events.first().map(|event| event.turn_id.clone());
        let latest_session = raw_events.first().map(|event| event.session_id.clone());
        let entity_changes = entity_changes
            .into_iter()
            .filter(|change| match scope {
                VisualizationScope::Turn => latest_turn
                    .as_deref()
                    .is_none_or(|turn| change.turn_id == turn),
                VisualizationScope::Session => latest_session
                    .as_deref()
                    .is_none_or(|session| change.session_id == session),
            })
            .collect::<Vec<_>>();
        let mut entities = entities;
        add_deleted_change_entities(&mut entities, &entity_changes);
        let modified_entities = modified_entities_for_scope(
            scope,
            latest_turn.as_deref(),
            latest_session.as_deref(),
            &trajectory_events,
            &entities,
        );
        let added_entities = changed_entities(&entity_changes, &entities, EntityChangeKind::Added);
        let deleted_entities =
            changed_entities(&entity_changes, &entities, EntityChangeKind::Deleted);
        let impacted_entities = impacted_entities(
            &modified_entities,
            &added_entities,
            &deleted_entities,
            &relations,
            &entity_changes,
            &entities,
        );
        let tool_chain = tool_chain_for_scope(
            scope,
            latest_turn.as_deref(),
            latest_session.as_deref(),
            raw_events,
            &trajectory_events,
        );

        VisualizationGraph {
            entities,
            relations,
            modified_entities,
            added_entities,
            deleted_entities,
            impacted_entities,
            tool_chain,
            warnings,
        }
    }

    fn cog_db_path(&self) -> PathBuf {
        self.workspace.join(".cog").join("cog.db")
    }

    fn recommender_db_path(&self) -> PathBuf {
        self.workspace
            .join(".cog")
            .join(DEFAULT_RECOMMENDER_DB_NAME)
    }

    fn load_cog_graph(
        &self,
        include_contains: bool,
    ) -> Result<(Vec<VisualizationEntity>, Vec<VisualizationRelation>)> {
        let path = self.cog_db_path();
        if !path.exists() {
            anyhow::bail!("{} does not exist", path.display());
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let mut entity_stmt =
            conn.prepare("SELECT id, qualified_name, kind FROM entities ORDER BY qualified_name")?;
        let entities = entity_stmt
            .query_map([], |row| {
                let name: String = row.get(1)?;
                Ok(VisualizationEntity {
                    id: row.get(0)?,
                    display_name: display_name(&name),
                    name,
                    kind: row.get(2)?,
                    file_path: None,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let relation_sql = if include_contains {
            "SELECT id, from_entity, to_entity, kind FROM entity_relations ORDER BY from_entity, to_entity"
        } else {
            "SELECT id, from_entity, to_entity, kind FROM entity_relations WHERE kind IN ('calls', 'uses') ORDER BY from_entity, to_entity"
        };
        let mut relation_stmt = conn.prepare(relation_sql)?;
        let relations = relation_stmt
            .query_map([], |row| {
                Ok(VisualizationRelation {
                    id: row.get(0)?,
                    source: row.get(1)?,
                    target: row.get(2)?,
                    kind: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok((entities, relations))
    }

    fn load_recent_raw_events(&self, limit: usize) -> Result<Vec<RawEventRecord>> {
        let path = self.recommender_db_path();
        if !path.exists() {
            anyhow::bail!("{} does not exist", path.display());
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, turn_id, ts, tool_name, input_json,
                    output_summary, status
             FROM tool_events
             ORDER BY ts DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit_to_i64(limit)], |row| {
            let input_json: String = row.get(5)?;
            Ok(RawEventRecord {
                id: row.get(0)?,
                session_id: row.get(1)?,
                turn_id: row.get(2)?,
                ts: row.get(3)?,
                tool_name: row.get(4)?,
                input_summary: serde_json::from_str(&input_json).unwrap_or(Value::Null),
                output_summary: row.get(6)?,
                status: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn load_recent_trajectory_events(&self, limit: usize) -> Result<Vec<TrajectoryRecord>> {
        let path = self.recommender_db_path();
        if !path.exists() {
            anyhow::bail!("{} does not exist", path.display());
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let mut stmt = conn.prepare(
            "SELECT te.raw_event_id, re.turn_id, re.session_id, te.kind, te.entity_id,
                    te.entity_json, te.file_path, te.payload_json, re.input_json, re.output_summary
             FROM trajectory_events te
             JOIN tool_events re ON re.id = te.raw_event_id
             ORDER BY te.rowid DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit_to_i64(limit)], map_trajectory_record)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn load_recent_entity_changes(
        &self,
        scope: VisualizationScope,
        limit: usize,
    ) -> Result<Vec<EntityChange>> {
        let path = self.recommender_db_path();
        if !path.exists() {
            anyhow::bail!("{} does not exist", path.display());
        }
        let repo = SqliteTrajectoryRepository::open(&path)?;
        repo.list_recent_entity_changes(scope, limit)
    }
}

fn map_trajectory_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<TrajectoryRecord> {
    let entity_json: Option<String> = row.get(5)?;
    let payload_json: String = row.get(7)?;
    let raw_input_json: String = row.get(8)?;
    let entity_value = entity_json
        .as_deref()
        .and_then(|value| serde_json::from_str::<Value>(value).ok())
        .flatten_null();
    let entity_name = entity_value
        .as_ref()
        .and_then(|value| value.get("qualified_name"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let entity_id_from_json = entity_value
        .as_ref()
        .and_then(|value| value.get("cog_entity_id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    Ok(TrajectoryRecord {
        raw_event_id: row.get(0)?,
        turn_id: row.get(1)?,
        session_id: row.get(2)?,
        kind: row.get(3)?,
        entity_id: row.get::<_, Option<String>>(4)?.or(entity_id_from_json),
        entity_name,
        file_path: row.get(6)?,
        payload: serde_json::from_str(&payload_json).unwrap_or(Value::Null),
        raw_input: serde_json::from_str(&raw_input_json).unwrap_or(Value::Null),
        raw_output: row.get(9)?,
    })
}

fn modified_entities_for_scope(
    scope: VisualizationScope,
    latest_turn: Option<&str>,
    latest_session: Option<&str>,
    trajectory_events: &[TrajectoryRecord],
    entities: &[VisualizationEntity],
) -> Vec<String> {
    let mut modified = HashSet::new();
    for event in trajectory_events {
        if event.kind != "edit_entity" {
            continue;
        }
        if !event_in_scope(scope, latest_turn, latest_session, event) {
            continue;
        }
        for id in resolve_event_entity_ids(event, entities) {
            modified.insert(id);
        }
    }
    sorted_ids(modified)
}

fn add_deleted_change_entities(entities: &mut Vec<VisualizationEntity>, changes: &[EntityChange]) {
    let existing = entities
        .iter()
        .map(|entity| entity.id.clone())
        .collect::<HashSet<_>>();
    let mut added = HashSet::new();
    for change in changes
        .iter()
        .filter(|change| change.kind == EntityChangeKind::Deleted)
    {
        let id = deleted_entity_id(&change.entity.qualified_name);
        if existing.contains(&id) || !added.insert(id.clone()) {
            continue;
        }
        entities.push(VisualizationEntity {
            id,
            name: change.entity.qualified_name.clone(),
            display_name: display_name(&change.entity.qualified_name),
            kind: "deleted".to_string(),
            file_path: change.entity.file_path.clone(),
        });
    }
}

fn changed_entities(
    changes: &[EntityChange],
    entities: &[VisualizationEntity],
    kind: EntityChangeKind,
) -> Vec<String> {
    let mut ids = HashSet::new();
    for change in changes.iter().filter(|change| change.kind == kind) {
        match kind {
            EntityChangeKind::Added => {
                if let Some(id) = current_entity_id(&change.entity, entities) {
                    ids.insert(id);
                }
            }
            EntityChangeKind::Deleted => {
                ids.insert(deleted_entity_id(&change.entity.qualified_name));
            }
        }
    }
    sorted_ids(ids)
}

fn impacted_entities(
    modified: &[String],
    added: &[String],
    deleted: &[String],
    relations: &[VisualizationRelation],
    changes: &[EntityChange],
    entities: &[VisualizationEntity],
) -> Vec<String> {
    let mut source_set: HashSet<String> = modified.iter().cloned().collect();
    source_set.extend(added.iter().cloned());
    source_set.extend(deleted.iter().cloned());
    let mut reverse_dependencies: HashMap<String, Vec<String>> = HashMap::new();
    for relation in relations {
        if matches!(relation.kind.as_str(), "calls" | "uses") {
            reverse_dependencies
                .entry(relation.target.clone())
                .or_default()
                .push(relation.source.clone());
        }
    }

    let mut impacted = HashSet::new();
    let mut queue: VecDeque<String> = modified
        .iter()
        .chain(added.iter())
        .chain(deleted.iter())
        .cloned()
        .collect();
    while let Some(entity_id) = queue.pop_front() {
        if let Some(dependents) = reverse_dependencies.get(&entity_id) {
            for dependent in dependents {
                if impacted.insert(dependent.clone()) {
                    queue.push_back(dependent.clone());
                }
            }
        }
        if impacted.len() > 500 {
            break;
        }
    }
    for change in changes {
        for impacted_entity in &change.impacted_entities {
            if let Some(id) = current_entity_id(impacted_entity, entities) {
                impacted.insert(id);
            }
        }
    }
    for id in &source_set {
        impacted.remove(id);
    }
    sorted_ids(impacted)
}

fn current_entity_id(
    entity: &super::types::EntityRef,
    entities: &[VisualizationEntity],
) -> Option<String> {
    if let Some(id) = &entity.cog_entity_id
        && entities.iter().any(|candidate| candidate.id == *id)
    {
        return Some(id.clone());
    }
    entities
        .iter()
        .find(|candidate| candidate.name == entity.qualified_name)
        .map(|candidate| candidate.id.clone())
}

fn deleted_entity_id(qualified_name: &str) -> String {
    format!("deleted:{qualified_name}")
}

fn tool_chain_for_scope(
    scope: VisualizationScope,
    latest_turn: Option<&str>,
    latest_session: Option<&str>,
    mut raw_events: Vec<RawEventRecord>,
    trajectory_events: &[TrajectoryRecord],
) -> Vec<ToolChainNode> {
    raw_events.reverse();
    let kind_by_raw = trajectory_events
        .iter()
        .map(|event| (event.raw_event_id.clone(), event.kind.clone()))
        .collect::<HashMap<_, _>>();
    let target_by_raw = trajectory_events
        .iter()
        .filter_map(|event| {
            event
                .file_path
                .clone()
                .or_else(|| event.entity_name.clone())
                .map(|target| (event.raw_event_id.clone(), target))
        })
        .collect::<HashMap<_, _>>();

    raw_events
        .into_iter()
        .filter(|event| match scope {
            VisualizationScope::Turn => latest_turn.is_none_or(|turn| event.turn_id == turn),
            VisualizationScope::Session => {
                latest_session.is_none_or(|session| event.session_id == session)
            }
        })
        .map(|event| ToolChainNode {
            id: event.id.clone(),
            tool_name: event.tool_name,
            kind: kind_by_raw
                .get(&event.id)
                .cloned()
                .unwrap_or_else(|| "raw_tool".to_string()),
            target: target_by_raw.get(&event.id).cloned(),
            status: event.status,
            ts: event.ts,
            input_summary: event.input_summary,
            output_summary: event.output_summary,
        })
        .collect()
}

fn event_in_scope(
    scope: VisualizationScope,
    latest_turn: Option<&str>,
    latest_session: Option<&str>,
    event: &TrajectoryRecord,
) -> bool {
    match scope {
        VisualizationScope::Turn => latest_turn.is_none_or(|turn| event.turn_id == turn),
        VisualizationScope::Session => {
            latest_session.is_none_or(|session| event.session_id == session)
        }
    }
}

fn resolve_event_entity_ids(
    event: &TrajectoryRecord,
    entities: &[VisualizationEntity],
) -> Vec<String> {
    if let Some(id) = &event.entity_id {
        return vec![id.clone()];
    }
    if let Some(name) = &event.entity_name
        && let Some(entity) = entities.iter().find(|entity| entity.name == *name)
    {
        return vec![entity.id.clone()];
    }
    event.file_path.as_deref().map_or_else(Vec::new, |path| {
        match_file_to_entities(path, event, entities)
    })
}

fn match_file_to_entities(
    path: &str,
    event: &TrajectoryRecord,
    entities: &[VisualizationEntity],
) -> Vec<String> {
    let normalized_path = normalize_entity_like_path(path);
    let candidates = entities
        .iter()
        .filter(|entity| {
            let normalized_entity = normalize_entity_name(&entity.name);
            entity_matches_path(&normalized_entity, &normalized_path, path)
        })
        .collect::<Vec<_>>();

    if candidates.is_empty() {
        return Vec::new();
    }

    let symbol_hints = extract_symbol_hints_from_record(event);
    if !symbol_hints.is_empty() {
        let matched = candidates
            .iter()
            .filter(|entity| {
                let short = entity_short_name(&entity.name);
                let full = normalize_entity_name(&entity.name);
                symbol_hints.contains(&short)
                    || symbol_hints.iter().any(|hint| full.ends_with(hint))
            })
            .map(|entity| entity.id.clone())
            .collect::<HashSet<_>>();
        if !matched.is_empty() {
            return sorted_ids(matched);
        }
    }

    let concrete = candidates
        .iter()
        .filter(|entity| matches!(entity.kind.as_str(), "function" | "method" | "type"))
        .map(|entity| entity.id.clone())
        .collect::<HashSet<_>>();
    if !concrete.is_empty() && concrete.len() <= 25 {
        return sorted_ids(concrete);
    }

    candidates
        .into_iter()
        .map(|entity| (normalize_entity_name(&entity.name).len(), entity.id.clone()))
        .max_by_key(|(score, _)| *score)
        .map(|(_, id)| vec![id])
        .unwrap_or_default()
}

fn extract_symbol_hints(payload: &Value) -> HashSet<String> {
    extract_symbol_hints_from_text(&payload.to_string())
}

fn extract_symbol_hints_from_record(event: &TrajectoryRecord) -> HashSet<String> {
    let mut hints = extract_symbol_hints(&event.payload);
    hints.extend(extract_symbol_hints(&event.raw_input));
    hints.extend(extract_symbol_hints_from_text(&event.raw_output));
    hints
}

fn extract_symbol_hints_from_text(text: &str) -> HashSet<String> {
    let mut hints = HashSet::new();
    if let Ok(qualified) =
        Regex::new(r"\b[A-Za-z_$][A-Za-z0-9_$]*(?:::[A-Za-z_$][A-Za-z0-9_$]*)+\b")
    {
        for capture in qualified.captures_iter(text) {
            if let Some(value) = capture.get(0) {
                hints.insert(normalize_entity_name(value.as_str()));
                if let Some(last) = value.as_str().rsplit("::").next() {
                    hints.insert(normalize_symbol(last));
                }
            }
        }
    }
    let Ok(symbol) = Regex::new(
        r"(?m)\b(?:function|def|class|interface|type|const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)|^\s*(?:async\s+)?([A-Za-z_$][A-Za-z0-9_$]*)\s*\(",
    ) else {
        return hints;
    };
    hints.extend(
        symbol
            .captures_iter(text)
            .filter_map(|capture| capture.get(1).or_else(|| capture.get(2)))
            .map(|name| normalize_symbol(name.as_str())),
    );
    hints
}

fn entity_matches_path(
    normalized_entity: &str,
    normalized_path: &str,
    original_path: &str,
) -> bool {
    if normalized_entity.contains(normalized_path) || normalized_path.contains(normalized_entity) {
        return true;
    }
    let stem = std::path::Path::new(original_path)
        .file_stem()
        .and_then(|value| value.to_str())
        .map(normalize_symbol);
    if let Some(stem) = stem
        && normalized_entity.split("::").any(|part| part == stem)
    {
        return true;
    }
    false
}

fn normalize_entity_like_path(value: &str) -> String {
    value
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_end_matches(".rs")
        .trim_end_matches(".py")
        .trim_end_matches(".ts")
        .trim_end_matches(".tsx")
        .trim_end_matches(".js")
        .trim_end_matches(".jsx")
        .replace('/', "::")
        .to_ascii_lowercase()
}

fn normalize_entity_name(value: &str) -> String {
    value.replace('/', "::").to_ascii_lowercase()
}

fn normalize_symbol(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn entity_short_name(qualified_name: &str) -> String {
    normalize_symbol(
        qualified_name
            .rsplit("::")
            .next()
            .filter(|value| !value.is_empty())
            .unwrap_or(qualified_name),
    )
}

fn display_name(qualified_name: &str) -> String {
    compact_label(
        qualified_name
            .rsplit("::")
            .next()
            .filter(|value| !value.is_empty())
            .unwrap_or(qualified_name),
    )
}

fn compact_label(value: &str) -> String {
    const MAX_LABEL_CHARS: usize = 13;
    let count = value.chars().count();
    if count <= MAX_LABEL_CHARS {
        return value.to_string();
    }
    let head = value
        .chars()
        .take(MAX_LABEL_CHARS.saturating_sub(3))
        .collect::<String>();
    format!("{head}...")
}

fn sorted_ids(values: HashSet<String>) -> Vec<String> {
    let mut values = values.into_iter().collect::<Vec<_>>();
    values.sort();
    values
}

fn limit_to_i64(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

trait NullFlatten {
    fn flatten_null(self) -> Option<Value>;
}

impl NullFlatten for Option<Value> {
    fn flatten_null(self) -> Option<Value> {
        self.filter(|value| !value.is_null())
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use rusqlite::params;
    use std::path::Path;
    use tempfile::tempdir;

    use super::*;
    use crate::cog_recommender::collector::build_raw_event;
    use crate::cog_recommender::storage::{SqliteTrajectoryRepository, TrajectoryRepository};
    use crate::cog_recommender::types::{
        EntityChange, EntityChangeKind, EntityKind, EntityRef, ToolEventStatus, TrajectoryEvent,
        TrajectoryKind,
    };

    #[test]
    fn graph_defaults_to_calls_and_uses_and_marks_modified_and_impacted() {
        let workspace = tempdir().expect("workspace");
        create_cog_db(
            workspace.path(),
            &[
                ("a", "src::api", "module"),
                ("b", "src::service", "module"),
                ("c", "src::root", "module"),
            ],
            &[("r1", "a", "b", "calls"), ("r2", "c", "b", "contains")],
        );
        create_recommender_db(workspace.path(), "src/service.rs", "turn-1", "session-1");

        let graph = VisualizationStore::new(workspace.path()).load_graph(
            VisualizationScope::Turn,
            false,
            50,
        );

        assert_eq!(graph.relations.len(), 1);
        assert_eq!(graph.modified_entities, vec!["b"]);
        assert_eq!(graph.impacted_entities, vec!["a"]);
        assert_eq!(graph.tool_chain.len(), 1);
        assert!(graph.warnings.is_empty());
    }

    #[test]
    fn graph_can_include_contains() {
        let workspace = tempdir().expect("workspace");
        create_cog_db(
            workspace.path(),
            &[("a", "a", "module"), ("b", "b", "module")],
            &[("r1", "a", "b", "contains")],
        );

        let graph = VisualizationStore::new(workspace.path()).load_graph(
            VisualizationScope::Turn,
            true,
            50,
        );

        assert_eq!(graph.relations.len(), 1);
        assert_eq!(graph.relations[0].kind, "contains");
    }

    #[test]
    fn graph_marks_added_and_deleted_entities_from_sync_diff() {
        let workspace = tempdir().expect("workspace");
        create_cog_db(
            workspace.path(),
            &[("a", "api", "module"), ("new", "new_feature", "module")],
            &[("r1", "a", "new", "uses")],
        );
        create_recommender_db(workspace.path(), "new_feature.py", "turn-1", "session-1");
        let repo = SqliteTrajectoryRepository::open(
            &workspace
                .path()
                .join(".cog")
                .join(super::DEFAULT_RECOMMENDER_DB_NAME),
        )
        .expect("repo");
        repo.record_entity_change(&EntityChange {
            id: "change-added".into(),
            session_id: "session-1".into(),
            turn_id: "turn-1".into(),
            raw_event_id: "raw".into(),
            ts: Utc::now(),
            kind: EntityChangeKind::Added,
            entity: EntityRef {
                cog_entity_id: Some("new".into()),
                qualified_name: "new_feature".into(),
                kind: EntityKind::Module,
                file_path: None,
                confidence: 0.9,
            },
            impacted_entities: vec![EntityRef {
                cog_entity_id: Some("a".into()),
                qualified_name: "api".into(),
                kind: EntityKind::Module,
                file_path: None,
                confidence: 0.9,
            }],
        })
        .expect("change");
        repo.record_entity_change(&EntityChange {
            id: "change-deleted".into(),
            session_id: "session-1".into(),
            turn_id: "turn-1".into(),
            raw_event_id: "raw".into(),
            ts: Utc::now(),
            kind: EntityChangeKind::Deleted,
            entity: EntityRef {
                cog_entity_id: Some("old".into()),
                qualified_name: "old_feature".into(),
                kind: EntityKind::Module,
                file_path: None,
                confidence: 0.9,
            },
            impacted_entities: vec![EntityRef {
                cog_entity_id: Some("a".into()),
                qualified_name: "api".into(),
                kind: EntityKind::Module,
                file_path: None,
                confidence: 0.9,
            }],
        })
        .expect("change");

        let graph = VisualizationStore::new(workspace.path()).load_graph(
            VisualizationScope::Turn,
            false,
            50,
        );

        assert_eq!(graph.added_entities, vec!["new"]);
        assert_eq!(graph.deleted_entities, vec!["deleted:old_feature"]);
        assert!(
            graph
                .entities
                .iter()
                .any(|entity| entity.id == "deleted:old_feature")
        );
        assert!(graph.impacted_entities.contains(&"a".to_string()));
    }

    #[test]
    fn edit_payload_symbol_hint_resolves_function_entity_in_file() {
        let entities = vec![
            VisualizationEntity {
                id: "file".into(),
                name: "src::service".into(),
                display_name: "service".into(),
                kind: "module".into(),
                file_path: None,
            },
            VisualizationEntity {
                id: "func".into(),
                name: "src::service::calculateTotal".into(),
                display_name: "calculateTotal".into(),
                kind: "function".into(),
                file_path: None,
            },
        ];
        let event = TrajectoryRecord {
            raw_event_id: "raw".into(),
            turn_id: "turn".into(),
            session_id: "session".into(),
            kind: "edit_entity".into(),
            entity_id: None,
            entity_name: None,
            file_path: Some("src/service.ts".into()),
            payload: serde_json::json!({
                "path": "src/service.ts",
                "patch": "export function calculateTotal(items) { return items.length; }"
            }),
            raw_input: Value::Null,
            raw_output: String::new(),
        };

        let ids = resolve_event_entity_ids(&event, &entities);

        assert_eq!(ids, vec!["func"]);
    }

    #[test]
    fn raw_input_qualified_symbol_hint_resolves_nested_entity_in_file() {
        let entities = vec![
            VisualizationEntity {
                id: "file".into(),
                name: "screen_capture".into(),
                display_name: "screen_capture".into(),
                kind: "module".into(),
                file_path: None,
            },
            VisualizationEntity {
                id: "method".into(),
                name: "screen_capture::RegionSelector::_setup_window".into(),
                display_name: "_setup_win...".into(),
                kind: "method".into(),
                file_path: None,
            },
            VisualizationEntity {
                id: "other".into(),
                name: "screen_capture::RegionSelector::_draw_overlay".into(),
                display_name: "_draw_ove...".into(),
                kind: "method".into(),
                file_path: None,
            },
        ];
        let event = TrajectoryRecord {
            raw_event_id: "raw".into(),
            turn_id: "turn".into(),
            session_id: "session".into(),
            kind: "edit_entity".into(),
            entity_id: None,
            entity_name: None,
            file_path: Some("screen_capture.py".into()),
            payload: serde_json::json!({ "path": "screen_capture.py" }),
            raw_input: serde_json::json!({
                "command": "update screen_capture::RegionSelector::_setup_window comments"
            }),
            raw_output: String::new(),
        };

        let ids = resolve_event_entity_ids(&event, &entities);

        assert_eq!(ids, vec!["method"]);
    }

    #[test]
    fn display_name_uses_last_qualified_segment() {
        assert_eq!(
            display_name("packages::core::src::TaskManager::createTask"),
            "createTask"
        );
    }

    #[test]
    fn display_name_compacts_long_entity_names() {
        assert_eq!(
            display_name("prompt_builder::build_batch_prompt"),
            "build_batc..."
        );
    }

    fn create_cog_db(
        workspace: &Path,
        entities: &[(&str, &str, &str)],
        relations: &[(&str, &str, &str, &str)],
    ) {
        let cog_dir = workspace.join(".cog");
        std::fs::create_dir_all(&cog_dir).expect("cog dir");
        let conn = Connection::open(cog_dir.join("cog.db")).expect("cog db");
        conn.execute_batch(
            "CREATE TABLE entities (
                id TEXT PRIMARY KEY,
                qualified_name TEXT UNIQUE NOT NULL,
                kind TEXT NOT NULL,
                origin TEXT NOT NULL DEFAULT 'manual',
                metrics_json TEXT,
                created_at TEXT NOT NULL
             );
             CREATE TABLE entity_relations (
                id TEXT PRIMARY KEY,
                from_entity TEXT NOT NULL,
                to_entity TEXT NOT NULL,
                kind TEXT NOT NULL
             );",
        )
        .expect("schema");
        for (id, name, kind) in entities {
            conn.execute(
                "INSERT INTO entities (id, qualified_name, kind, origin, created_at)
                 VALUES (?1, ?2, ?3, 'scan', ?4)",
                params![id, name, kind, Utc::now().to_rfc3339()],
            )
            .expect("entity");
        }
        for (id, from, to, kind) in relations {
            conn.execute(
                "INSERT INTO entity_relations (id, from_entity, to_entity, kind)
                 VALUES (?1, ?2, ?3, ?4)",
                params![id, from, to, kind],
            )
            .expect("relation");
        }
    }

    fn create_recommender_db(workspace: &Path, file_path: &str, turn_id: &str, session_id: &str) {
        let repo = SqliteTrajectoryRepository::open(
            &workspace
                .join(".cog")
                .join(super::DEFAULT_RECOMMENDER_DB_NAME),
        )
        .expect("repo");
        let raw = build_raw_event(
            session_id,
            turn_id,
            "apply_patch",
            serde_json::json!({ "path": file_path }),
            "patched",
            ToolEventStatus::Success,
            10,
        );
        repo.record_raw_event(&raw).expect("raw");
        repo.record_trajectory_event(&TrajectoryEvent {
            id: "trajectory-1".into(),
            raw_event_id: raw.id,
            session_id: session_id.into(),
            kind: TrajectoryKind::EditEntity,
            entity_ref: None,
            file_path: Some(file_path.into()),
            line_range: None,
            payload: serde_json::json!({ "path": file_path }),
            confidence: 0.0,
        })
        .expect("trajectory");
    }
}
