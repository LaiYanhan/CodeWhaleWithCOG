use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use super::types::{EntityKind, EntityRef, Evidence, EvidenceSource, SyncStatus};

pub trait CogAdapter {
    fn ensure_synced(&self, workspace: &Path) -> SyncStatus;
    fn resolve_file(&self, path: &str) -> Vec<EntityRef>;
    fn resolve_location(&self, path: &str, _line: u32) -> Option<EntityRef> {
        self.resolve_file(path).into_iter().next()
    }
    fn impact(&self, entity: &EntityRef) -> Vec<Evidence>;
    fn related(&self, entity: &EntityRef) -> Vec<Evidence>;
    fn assertions(&self, entity: &EntityRef) -> Vec<Evidence>;
}

#[derive(Debug, Clone)]
pub struct CliCogAdapter {
    workspace: PathBuf,
    cog_binary: String,
}

impl CliCogAdapter {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            cog_binary: "cog".to_string(),
        }
    }

    pub fn with_binary(mut self, cog_binary: impl Into<String>) -> Self {
        self.cog_binary = cog_binary.into();
        self
    }

    fn run_cog(&self, args: &[&str]) -> Result<Value, String> {
        let output = Command::new(&self.cog_binary)
            .current_dir(&self.workspace)
            .arg("--output")
            .arg("json")
            .args(args)
            .output()
            .map_err(|err| format!("failed to execute {}: {err}", self.cog_binary))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return Err(if stderr.is_empty() { stdout } else { stderr });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        serde_json::from_str(&stdout).map_err(|err| format!("invalid COG JSON output: {err}"))
    }

    fn run_cog_or_null(&self, args: &[&str]) -> Value {
        self.run_cog(args).unwrap_or(Value::Null)
    }
}

impl CogAdapter for CliCogAdapter {
    fn ensure_synced(&self, workspace: &Path) -> SyncStatus {
        let has_db = workspace.join(".cog").join("cog.db").exists();
        let args: Vec<&str> = if has_db {
            vec!["sync"]
        } else {
            vec!["sync", "--init"]
        };

        match self.run_cog(&args) {
            Ok(_) if has_db => SyncStatus::Synced,
            Ok(_) => SyncStatus::Initialized,
            Err(err) => SyncStatus::Degraded(err),
        }
    }

    fn resolve_file(&self, path: &str) -> Vec<EntityRef> {
        let output = self.run_cog_or_null(&["index", "--verbose"]);
        parse_index_entities(&output)
            .into_iter()
            .filter(|entity| entity_matches_path(entity, path))
            .collect()
    }

    fn impact(&self, entity: &EntityRef) -> Vec<Evidence> {
        let output = self.run_cog_or_null(&["impact", &entity.qualified_name]);
        output
            .get("downstream_entities")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(entity_from_value)
            .map(|target| {
                Evidence::new(
                    EvidenceSource::CogImpact,
                    target,
                    0.75,
                    "COG impact reports this entity as downstream of the current entity",
                )
                .with_payload(json!({ "source": entity.qualified_name }))
            })
            .collect()
    }

    fn related(&self, entity: &EntityRef) -> Vec<Evidence> {
        let output = self.run_cog_or_null(&["query", &entity.qualified_name, "--relations"]);
        output
            .get("related")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|related| {
                let target = related.get("entity").and_then(entity_from_value)?;
                let kind = related
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("related");
                let direction = related
                    .get("direction")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                Some(
                    Evidence::new(
                        EvidenceSource::CogRelation,
                        target,
                        0.6,
                        format!("COG relation {kind} ({direction}) links this entity"),
                    )
                    .with_payload(json!({
                        "source": entity.qualified_name,
                        "kind": kind,
                        "direction": direction,
                    })),
                )
            })
            .collect()
    }

    fn assertions(&self, entity: &EntityRef) -> Vec<Evidence> {
        let output = self.run_cog_or_null(&["query", &entity.qualified_name]);
        let target = output
            .get("entity")
            .and_then(entity_from_value)
            .unwrap_or_else(|| entity.clone());
        output
            .get("assertions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(assertion_from_tuple_or_object)
            .filter(|assertion| {
                assertion
                    .get("status")
                    .and_then(Value::as_str)
                    .is_none_or(|status| status == "active")
            })
            .map(|assertion| {
                let claim = assertion
                    .get("claim")
                    .and_then(Value::as_str)
                    .unwrap_or("COG assertion");
                Evidence::new(EvidenceSource::Assertion, target.clone(), 0.7, claim)
                    .with_payload(assertion.clone())
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default)]
pub struct StaticCogAdapter {
    files: HashMap<String, Vec<EntityRef>>,
    impact: HashMap<String, Vec<Evidence>>,
    related: HashMap<String, Vec<Evidence>>,
    assertions: HashMap<String, Vec<Evidence>>,
}

impl StaticCogAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_file(mut self, path: impl Into<String>, entity: EntityRef) -> Self {
        self.files.entry(path.into()).or_default().push(entity);
        self
    }

    pub fn with_impact(mut self, entity_name: impl Into<String>, evidence: Evidence) -> Self {
        self.impact
            .entry(entity_name.into())
            .or_default()
            .push(evidence);
        self
    }
}

trait EvidencePayloadExt {
    fn with_payload(self, payload: Value) -> Self;
}

impl EvidencePayloadExt for Evidence {
    fn with_payload(mut self, payload: Value) -> Self {
        self.payload = payload;
        self
    }
}

fn parse_index_entities(value: &Value) -> Vec<EntityRef> {
    value
        .get("entities")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            if let Some(entity) = entry.as_array().and_then(|pair| pair.first()) {
                entity_from_value(entity)
            } else {
                entity_from_value(entry)
            }
        })
        .collect()
}

fn assertion_from_tuple_or_object(value: &Value) -> Option<&Value> {
    value
        .as_array()
        .and_then(|pair| pair.first())
        .or_else(|| value.as_object().map(|_| value))
}

fn entity_from_value(value: &Value) -> Option<EntityRef> {
    let qualified_name = value.get("qualified_name")?.as_str()?.to_string();
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .map(entity_kind_from_cog)
        .unwrap_or(EntityKind::Unknown);
    Some(EntityRef {
        cog_entity_id: value
            .get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        qualified_name,
        kind,
        file_path: None,
        confidence: 0.8,
    })
}

fn entity_kind_from_cog(kind: &str) -> EntityKind {
    match kind {
        "module" => EntityKind::Module,
        "function" => EntityKind::Function,
        "type" => EntityKind::Type,
        "method" => EntityKind::Method,
        "field" | "unknown" => EntityKind::Unknown,
        _ => EntityKind::Unknown,
    }
}

fn entity_matches_path(entity: &EntityRef, path: &str) -> bool {
    let normalized_path = path
        .replace('\\', "/")
        .replace(".rs", "")
        .replace(".py", "");
    let normalized_entity = entity.qualified_name.replace("::", "/");
    normalized_entity.contains(&normalized_path) || normalized_path.contains(&normalized_entity)
}

impl CogAdapter for StaticCogAdapter {
    fn ensure_synced(&self, _workspace: &Path) -> SyncStatus {
        SyncStatus::Synced
    }

    fn resolve_file(&self, path: &str) -> Vec<EntityRef> {
        self.files.get(path).cloned().unwrap_or_default()
    }

    fn impact(&self, entity: &EntityRef) -> Vec<Evidence> {
        self.impact
            .get(&entity.qualified_name)
            .cloned()
            .unwrap_or_default()
    }

    fn related(&self, entity: &EntityRef) -> Vec<Evidence> {
        self.related
            .get(&entity.qualified_name)
            .cloned()
            .unwrap_or_default()
    }

    fn assertions(&self, entity: &EntityRef) -> Vec<Evidence> {
        self.assertions
            .get(&entity.qualified_name)
            .cloned()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_index_entities_from_cog_json_tuple_shape() {
        let output = json!({
            "entities": [
                [{
                    "id": "entity-1",
                    "qualified_name": "src::auth::login",
                    "kind": "function",
                    "origin": "scan",
                    "metrics": {},
                    "created_at": "2026-07-04T00:00:00Z"
                }, 0]
            ]
        });

        let entities = parse_index_entities(&output);

        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].qualified_name, "src::auth::login");
        assert_eq!(entities[0].kind, EntityKind::Function);
        assert_eq!(entities[0].cog_entity_id.as_deref(), Some("entity-1"));
    }

    #[test]
    fn parses_related_evidence_from_query_json() {
        let adapter = StaticCogAdapter::new();
        assert_eq!(adapter.ensure_synced(Path::new(".")), SyncStatus::Synced);

        let related = json!({
            "entity": {"id": "a", "qualified_name": "A", "kind": "module"},
            "related": [{
                "entity": {"id": "b", "qualified_name": "B", "kind": "type"},
                "kind": "Uses",
                "direction": "Outgoing"
            }]
        });

        let evidence: Vec<_> = related
            .get("related")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(|item| item.get("entity").and_then(entity_from_value))
            .collect();

        assert_eq!(evidence[0].qualified_name, "B");
        assert_eq!(evidence[0].kind, EntityKind::Type);
    }
}
