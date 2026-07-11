use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEventStatus {
    Success,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEventOrigin {
    Agent,
    User,
    System,
    RecommenderInternal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawToolEvent {
    pub id: String,
    pub session_id: String,
    pub turn_id: String,
    pub ts: DateTime<Utc>,
    pub tool_name: String,
    pub input_summary: Value,
    pub output_summary: String,
    pub status: ToolEventStatus,
    pub duration_ms: u64,
    pub origin: ToolEventOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrajectoryKind {
    ReadEntity,
    SearchEntity,
    EditEntity,
    TestEntity,
    ErrorSignal,
    CogQuery,
    CogWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Module,
    Function,
    Type,
    Method,
    File,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityRef {
    pub cog_entity_id: Option<String>,
    pub qualified_name: String,
    pub kind: EntityKind,
    pub file_path: Option<String>,
    pub confidence: f64,
}

impl EntityRef {
    pub fn new(qualified_name: impl Into<String>) -> Self {
        Self {
            cog_entity_id: None,
            qualified_name: qualified_name.into(),
            kind: EntityKind::Unknown,
            file_path: None,
            confidence: 0.0,
        }
    }

    pub fn with_kind(mut self, kind: EntityKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn with_file_path(mut self, file_path: impl Into<String>) -> Self {
        self.file_path = Some(file_path.into());
        self
    }

    pub fn with_confidence(mut self, confidence: f64) -> Self {
        self.confidence = confidence;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryEvent {
    pub id: String,
    pub raw_event_id: String,
    pub session_id: String,
    pub kind: TrajectoryKind,
    pub entity_ref: Option<EntityRef>,
    pub file_path: Option<String>,
    pub line_range: Option<LineRange>,
    pub payload: Value,
    pub confidence: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    CogImpact,
    CogRelation,
    CoAccess,
    ReadBeforeEdit,
    SearchToRead,
    SearchToEdit,
    EditToTest,
    ErrorToEdit,
    CogWriteToEdit,
    Rule,
    Assertion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub source: EvidenceSource,
    pub target: EntityRef,
    pub weight: f64,
    pub reason: String,
    pub payload: Value,
}

impl Evidence {
    pub fn new(
        source: EvidenceSource,
        target: EntityRef,
        weight: f64,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            source,
            target,
            weight,
            reason: reason.into(),
            payload: Value::Null,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub entity: EntityRef,
    pub trigger_event_id: String,
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestedAction {
    Read,
    InspectImpact,
    RunTest,
    UpdateRelatedCode,
    Verify,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recommendation {
    pub entity: EntityRef,
    pub score: f64,
    pub evidence: Vec<Evidence>,
    pub suggested_action: SuggestedAction,
    pub display_text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendationStatus {
    Pending,
    Exposed,
    Completed,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendationFeedbackKind {
    Exposed,
    ReadAfterRecommendation,
    EditAfterRecommendation,
    ValidatedAfterRecommendation,
    NoObservedAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredRecommendation {
    pub id: String,
    pub session_id: String,
    pub turn_id: String,
    pub trigger_event_ids: Vec<String>,
    pub recommendation: Recommendation,
    pub status: RecommendationStatus,
    pub created_at: DateTime<Utc>,
    pub last_triggered_at: DateTime<Utc>,
    pub exposed_at: Option<DateTime<Utc>>,
    pub expires_at: DateTime<Utc>,
    pub trigger_tool_index: u64,
    pub exposed_turn_index: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecommendationFeedback {
    pub id: String,
    pub recommendation_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub kind: RecommendationFeedbackKind,
    pub event_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecommendationInjection {
    pub id: String,
    pub session_id: String,
    pub turn_id: String,
    pub created_at: DateTime<Utc>,
    pub context_text: String,
    pub recommendation_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    Synced,
    Initialized,
    Degraded(String),
}
