use serde_json::{Value, json};

use super::types::{
    RawToolEvent, ToolEventOrigin, ToolEventStatus, TrajectoryEvent, TrajectoryKind,
};

pub fn normalize_tool_event(raw: &RawToolEvent) -> Vec<TrajectoryEvent> {
    if !is_behavior_origin(raw.origin) {
        return Vec::new();
    }

    let mut events = Vec::new();
    let kind = classify_event(raw);
    if let Some(kind) = kind {
        events.push(TrajectoryEvent {
            id: format!("{}:{}", raw.id, kind_suffix(kind)),
            raw_event_id: raw.id.clone(),
            session_id: raw.session_id.clone(),
            kind,
            entity_ref: None,
            file_path: extract_path(&raw.input_summary),
            line_range: None,
            payload: raw.input_summary.clone(),
            confidence: 0.0,
        });
    }

    if raw.status == ToolEventStatus::Error && kind != Some(TrajectoryKind::ErrorSignal) {
        events.push(TrajectoryEvent {
            id: format!("{}:error", raw.id),
            raw_event_id: raw.id.clone(),
            session_id: raw.session_id.clone(),
            kind: TrajectoryKind::ErrorSignal,
            entity_ref: None,
            file_path: extract_path(&raw.input_summary),
            line_range: None,
            payload: json!({ "summary": raw.output_summary }),
            confidence: 0.0,
        });
    }

    events
}

fn is_behavior_origin(origin: ToolEventOrigin) -> bool {
    matches!(origin, ToolEventOrigin::Agent | ToolEventOrigin::User)
}

fn classify_event(raw: &RawToolEvent) -> Option<TrajectoryKind> {
    let name = raw.tool_name.as_str();
    if is_cog_command(raw) {
        return Some(if is_cog_write(raw) {
            TrajectoryKind::CogWrite
        } else {
            TrajectoryKind::CogQuery
        });
    }
    if matches!(name, "read_file" | "handle_read") {
        return Some(TrajectoryKind::ReadEntity);
    }
    if matches!(name, "file_search" | "grep_files" | "project_map") || name.contains("search") {
        return Some(TrajectoryKind::SearchEntity);
    }
    if matches!(
        name,
        "apply_patch" | "edit_file" | "write_file" | "fim_edit"
    ) {
        return Some(TrajectoryKind::EditEntity);
    }
    if name.contains("shell") || name == "exec" {
        let command = command_text(&raw.input_summary);
        if looks_like_test_command(&command) {
            return Some(TrajectoryKind::TestEntity);
        }
    }
    if raw.status == ToolEventStatus::Error {
        return Some(TrajectoryKind::ErrorSignal);
    }
    None
}

fn is_cog_command(raw: &RawToolEvent) -> bool {
    raw.tool_name == "cog"
        || command_text(&raw.input_summary)
            .trim_start()
            .starts_with("cog ")
}

fn is_cog_write(raw: &RawToolEvent) -> bool {
    let command = command_text(&raw.input_summary);
    ["assert", "depend", "retract"]
        .iter()
        .any(|verb| command.contains(&format!("cog {verb}")))
}

fn looks_like_test_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.contains("cargo test")
        || lower.contains("pytest")
        || lower.contains("npm test")
        || lower.contains("pnpm test")
        || lower.contains("yarn test")
        || lower.contains("go test")
}

fn command_text(value: &Value) -> String {
    value
        .get("cmd")
        .or_else(|| value.get("command"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub fn extract_path(value: &Value) -> Option<String> {
    value
        .get("path")
        .or_else(|| value.get("file_path"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn kind_suffix(kind: TrajectoryKind) -> &'static str {
    match kind {
        TrajectoryKind::ReadEntity => "read",
        TrajectoryKind::SearchEntity => "search",
        TrajectoryKind::EditEntity => "edit",
        TrajectoryKind::TestEntity => "test",
        TrajectoryKind::ErrorSignal => "error",
        TrajectoryKind::CogQuery => "cog-query",
        TrajectoryKind::CogWrite => "cog-write",
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;

    use super::*;

    fn raw(tool_name: &str, input: Value) -> RawToolEvent {
        RawToolEvent {
            id: "evt".into(),
            session_id: "s".into(),
            turn_id: "t".into(),
            ts: Utc::now(),
            tool_name: tool_name.into(),
            input_summary: input,
            output_summary: String::new(),
            status: ToolEventStatus::Success,
            duration_ms: 1,
            origin: ToolEventOrigin::Agent,
        }
    }

    #[test]
    fn normalizes_patch_as_edit_entity() {
        let events = normalize_tool_event(&raw("apply_patch", json!({"path": "src/lib.rs"})));
        assert_eq!(events[0].kind, TrajectoryKind::EditEntity);
        assert_eq!(events[0].file_path.as_deref(), Some("src/lib.rs"));
    }

    #[test]
    fn normalizes_shell_test_as_test_entity() {
        let events = normalize_tool_event(&raw("exec_shell", json!({"cmd": "cargo test"})));
        assert_eq!(events[0].kind, TrajectoryKind::TestEntity);
    }

    #[test]
    fn normalizes_cog_assert_as_write() {
        let events = normalize_tool_event(&raw(
            "exec_shell",
            json!({"cmd": "cog assert foo --kind invariant"}),
        ));
        assert_eq!(events[0].kind, TrajectoryKind::CogWrite);
    }

    #[test]
    fn ignores_recommender_internal_events() {
        let mut event = raw("exec_shell", json!({"cmd": "cog impact foo"}));
        event.origin = ToolEventOrigin::RecommenderInternal;

        let events = normalize_tool_event(&event);

        assert!(events.is_empty());
    }

    #[test]
    fn ignores_successful_non_behavior_tools() {
        for tool in ["checklist_write", "git_diff", "exec_shell"] {
            let events = normalize_tool_event(&raw(tool, json!({"cmd": "python helper.py"})));
            assert!(events.is_empty(), "{tool} must not become read_entity");
        }
    }
}
