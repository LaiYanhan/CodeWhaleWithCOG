//! Native COG graph tools exposed to the Agent tool catalog.

use std::process::Command;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::cog_recommender::cog_adapter::resolve_cog_binary;

use super::spec::{
    ApprovalRequirement, ToolCapability, ToolContext, ToolError, ToolResult, ToolSpec,
};

pub struct CogTool;

#[async_trait]
impl ToolSpec for CogTool {
    fn name(&self) -> &'static str {
        "cog"
    }

    fn description(&self) -> &'static str {
        "Query the repository COG graph, inspect impact and assertions, synchronize COG, or create/retract a grounded assertion. Read actions are automatic; graph and assertion mutations require approval."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type":"string", "enum":["next","query","impact","sync","assert","retract"]},
                "entity": {"type":"string", "description":"COG qualified entity name for query, impact, or assert."},
                "relations": {"type":"boolean", "description":"For query, include relations."},
                "kind": {"type":"string", "description":"Assertion kind for assert (for example invariant or correction)."},
                "claim": {"type":"string", "description":"Natural-language assertion claim."},
                "grounds": {"type":"string", "description":"Evidence supporting an assertion."},
                "depends_on": {"type":"string"},
                "replace": {"type":"string"},
                "force": {"type":"boolean"},
                "assertion_id": {"type":"string", "description":"Assertion id for retract."},
                "reason": {"type":"string", "description":"Retraction reason."}
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    fn capabilities(&self) -> Vec<ToolCapability> {
        vec![ToolCapability::ReadOnly, ToolCapability::WritesFiles]
    }

    fn approval_requirement_for(&self, input: &Value) -> ApprovalRequirement {
        match input.get("action").and_then(Value::as_str) {
            Some("next" | "query" | "impact") => ApprovalRequirement::Auto,
            _ => ApprovalRequirement::Suggest,
        }
    }

    fn is_read_only_for(&self, input: &Value) -> bool {
        matches!(
            input.get("action").and_then(Value::as_str),
            Some("next" | "query" | "impact")
        )
    }

    fn supports_parallel_for(&self, input: &Value) -> bool {
        self.is_read_only_for(input)
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<ToolResult, ToolError> {
        let action = required(input.get("action"), "action")?;
        let args = match action {
            "next" => vec!["next".into()],
            "query" => {
                let entity = required(input.get("entity"), "entity")?;
                let mut args = vec!["query".to_string(), entity.to_string()];
                if input
                    .get("relations")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    args.push("--relations".into());
                }
                args
            }
            "impact" => vec![
                "impact".into(),
                required(input.get("entity"), "entity")?.to_string(),
            ],
            "sync" => vec!["sync".into()],
            "assert" => {
                let mut args = vec![
                    "assert".into(),
                    required(input.get("entity"), "entity")?.to_string(),
                    "--kind".into(),
                    required(input.get("kind"), "kind")?.to_string(),
                    "--claim".into(),
                    required(input.get("claim"), "claim")?.to_string(),
                    "--grounds".into(),
                    required(input.get("grounds"), "grounds")?.to_string(),
                ];
                optional_arg(&mut args, "--depends-on", input.get("depends_on"));
                optional_arg(&mut args, "--replace", input.get("replace"));
                if input.get("force").and_then(Value::as_bool).unwrap_or(false) {
                    args.push("--force".into());
                }
                args
            }
            "retract" => vec![
                "retract".into(),
                required(input.get("assertion_id"), "assertion_id")?.to_string(),
                "--reason".into(),
                required(input.get("reason"), "reason")?.to_string(),
            ],
            other => {
                return Err(ToolError::invalid_input(format!(
                    "unsupported COG action: {other}"
                )));
            }
        };
        let binary = resolve_cog_binary();
        let output = Command::new(&binary)
            .current_dir(&context.workspace)
            .arg("--output")
            .arg("json")
            .args(&args)
            .output()
            .map_err(|err| {
                ToolError::execution_failed(format!(
                    "failed to execute {}: {err}",
                    binary.display()
                ))
            })?;
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(ToolError::execution_failed(if message.is_empty() {
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            } else {
                message
            }));
        }
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let payload =
            serde_json::from_str::<Value>(&stdout).unwrap_or_else(|_| json!({"text": stdout}));
        ToolResult::json(&json!({"action": action, "result": payload}))
            .map_err(|err| ToolError::execution_failed(err.to_string()))
    }
}

fn required<'a>(value: Option<&'a Value>, field: &str) -> Result<&'a str, ToolError> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ToolError::missing_field(field))
}

fn optional_arg(args: &mut Vec<String>, flag: &str, value: Option<&Value>) {
    if let Some(value) = value
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        args.push(flag.into());
        args.push(value.into());
    }
}
