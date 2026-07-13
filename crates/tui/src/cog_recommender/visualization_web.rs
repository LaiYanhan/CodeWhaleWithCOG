use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use super::config::RecommenderConfig;
use super::feedback::render_repository_recommendations;
use super::recommendation_summary::{RecommendationSummary, RecommendationSummaryStore};
use super::storage::{DEFAULT_RECOMMENDER_DB_NAME, SqliteTrajectoryRepository};
use super::types::RecommendationInjection;
use super::visualization::{VisualizationScope, VisualizationStore};

const DEFAULT_LIMIT: usize = 200;

#[derive(Debug, Clone)]
struct VisualizationState {
    store: VisualizationStore,
    recommendation_store: RecommendationSummaryStore,
    workspace: PathBuf,
}

#[derive(Debug, Deserialize)]
struct GraphQuery {
    #[serde(default)]
    scope: VisualizationScope,
    #[serde(default)]
    include_contains: bool,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct RecommendationSummaryQuery {
    #[serde(default)]
    scope: VisualizationScope,
    limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
struct RecommendationInjectionResponse {
    scope: VisualizationScope,
    generated_at: DateTime<Utc>,
    injections: Vec<RecommendationInjection>,
    preview: Option<RecommendationInjectionPreview>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RecommendationInjectionPreview {
    generated_at: DateTime<Utc>,
    source: &'static str,
    note: String,
    context_text: String,
    recommendation_ids: Vec<String>,
}

pub fn visualization_router(workspace: impl Into<PathBuf>) -> Router {
    let workspace = workspace.into();
    let state = VisualizationState {
        store: VisualizationStore::new(workspace.clone()),
        recommendation_store: RecommendationSummaryStore::new(workspace.clone()),
        workspace,
    };
    Router::new()
        .route("/", get(index))
        .route("/api/visualization/session-graph", get(session_graph))
        .route("/api/recommendations/summary", get(recommendation_summary))
        .route(
            "/api/recommendations/injections",
            get(recommendation_injections),
        )
        .route(
            "/api/recommendations/runtime-config",
            get(recommendation_runtime_config).put(update_recommendation_runtime_config),
        )
        .with_state(Arc::new(state))
}

pub async fn serve_visualization(workspace: impl Into<PathBuf>, addr: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind visualization service at {addr}"))?;
    let local_addr = listener
        .local_addr()
        .context("failed to read visualization service address")?;
    tracing::info!("COG visualization service listening at http://{local_addr}");
    axum::serve(listener, visualization_router(workspace))
        .await
        .context("visualization service failed")
}

pub fn spawn_visualization_from_env(workspace: impl Into<PathBuf>) {
    let Ok(port) = std::env::var("COG_RECOMMENDER_VIS_PORT") else {
        return;
    };
    let Ok(port) = port.parse::<u16>() else {
        tracing::warn!("invalid COG_RECOMMENDER_VIS_PORT value");
        return;
    };
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let workspace = workspace.into();
    tokio::spawn(async move {
        if let Err(err) = serve_visualization(workspace, addr).await {
            tracing::warn!("COG visualization service stopped: {err}");
        }
    });
}

async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Html(INDEX_HTML),
    )
}

async fn session_graph(
    State(state): State<Arc<VisualizationState>>,
    Query(query): Query<GraphQuery>,
) -> Json<super::visualization::VisualizationGraph> {
    let graph = state.store.load_graph(
        query.scope,
        query.include_contains,
        query.limit.unwrap_or(DEFAULT_LIMIT),
    );
    Json(graph)
}

async fn recommendation_summary(
    State(state): State<Arc<VisualizationState>>,
    Query(query): Query<RecommendationSummaryQuery>,
) -> Json<RecommendationSummary> {
    Json(
        state
            .recommendation_store
            .load_summary(query.scope, query.limit.unwrap_or(DEFAULT_LIMIT)),
    )
}

async fn recommendation_runtime_config(
    State(state): State<Arc<VisualizationState>>,
) -> Json<RecommenderConfig> {
    Json(load_runtime_config(&state.workspace))
}

async fn recommendation_injections(
    State(state): State<Arc<VisualizationState>>,
    Query(query): Query<RecommendationSummaryQuery>,
) -> Json<RecommendationInjectionResponse> {
    let path = state
        .workspace
        .join(".cog")
        .join(DEFAULT_RECOMMENDER_DB_NAME);
    let mut warnings = Vec::new();
    let mut preview = None;
    let injections = match SqliteTrajectoryRepository::open(&path) {
        Ok(repository) => {
            let injections = repository
                .list_recent_recommendation_injections(query.scope, query.limit.unwrap_or(20))
                .unwrap_or_else(|err| {
                    warnings.push(format!("failed to load recommendation injections: {err}"));
                    Vec::new()
                });
            if injections.is_empty() {
                preview =
                    build_recommendation_context_preview(&repository, query.scope, &mut warnings);
            }
            injections
        }
        Err(err) => {
            warnings.push(format!(
                "failed to open recommender db for injection snapshots: {err}"
            ));
            Vec::new()
        }
    };
    Json(RecommendationInjectionResponse {
        scope: query.scope,
        generated_at: Utc::now(),
        injections,
        preview,
        warnings,
    })
}

fn build_recommendation_context_preview(
    repository: &SqliteTrajectoryRepository,
    scope: VisualizationScope,
    warnings: &mut Vec<String>,
) -> Option<RecommendationInjectionPreview> {
    let config = repository.load_runtime_config().unwrap_or_else(|err| {
        warnings.push(format!(
            "failed to load recommendation runtime config for preview: {err}"
        ));
        RecommenderConfig::default()
    });
    if !config.enabled {
        warnings.push("recommendation runtime injection is disabled".to_string());
        return None;
    }
    let limit = config.max_recommendations.max(1).saturating_mul(3);
    let mut records = repository
        .list_recent_recommendations_for_context(scope, limit)
        .unwrap_or_else(|err| {
            warnings.push(format!(
                "failed to load pending recommendations for injection preview: {err}"
            ));
            Vec::new()
        });
    records.retain(|record| record.recommendation.score >= config.min_injection_score);
    records.sort_by(|left, right| {
        right
            .recommendation
            .score
            .partial_cmp(&left.recommendation.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    records.truncate(config.max_recommendations);
    if records.is_empty() {
        warnings.push(
            "no runtime pending recommendations found; recommendations table has no pending/exposed records that pass the current runtime config"
                .to_string(),
        );
        return None;
    }
    let rendered = render_repository_recommendations(records.iter(), &config);
    match rendered {
        Some(rendered) => Some(RecommendationInjectionPreview {
            generated_at: Utc::now(),
            source: "pending_recommendations",
            note: "Preview only: this text has not been inserted yet. It will be inserted before the next normal model request if the recommendations are still valid."
                .to_string(),
            context_text: rendered.text,
            recommendation_ids: rendered.recommendation_ids,
        }),
        None => {
            warnings.push(
                "pending recommendations exist, but none fit the current injection score or character budget"
                    .to_string(),
            );
            None
        }
    }
}

async fn update_recommendation_runtime_config(
    State(state): State<Arc<VisualizationState>>,
    Json(mut config): Json<RecommenderConfig>,
) -> Json<RecommenderConfig> {
    config.max_recommendations = config.max_recommendations.clamp(1, 10);
    config.max_total_chars = config.max_total_chars.clamp(200, 4000);
    config.max_reason_chars = config.max_reason_chars.clamp(40, 500);
    config.max_injections_per_turn = config.max_injections_per_turn.clamp(1, 3);
    config.min_injection_score = config.min_injection_score.clamp(0.0, 1.0);
    let path = state
        .workspace
        .join(".cog")
        .join(DEFAULT_RECOMMENDER_DB_NAME);
    if let Ok(repository) = SqliteTrajectoryRepository::open(&path) {
        let _ = repository.save_runtime_config(&config);
    }
    Json(config)
}

fn load_runtime_config(workspace: &std::path::Path) -> RecommenderConfig {
    let path = workspace.join(".cog").join(DEFAULT_RECOMMENDER_DB_NAME);
    SqliteTrajectoryRepository::open(&path)
        .and_then(|repository| repository.load_runtime_config())
        .unwrap_or_default()
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>COG Visualization</title>
  <script src="https://unpkg.com/cytoscape@3.30.4/dist/cytoscape.min.js"></script>
  <style>
    :root {
      --primary: #0066cc;
      --primary-focus: #0071e3;
      --primary-on-dark: #2997ff;
      --ink: #1d1d1f;
      --muted: #7a7a7a;
      --body-muted-on-dark: #cccccc;
      --hairline: #e0e0e0;
      --canvas: #ffffff;
      --canvas-parchment: #f5f5f7;
      --surface-pearl: #fafafc;
      --surface-tile: #272729;
      --surface-black: #000000;
    }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      min-height: 100vh;
      background: var(--canvas-parchment);
      color: var(--ink);
      font-family: "SF Pro Text", system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      font-size: 17px;
      font-weight: 400;
      line-height: 1.47;
      letter-spacing: -0.374px;
    }
    header {
      height: 44px;
      display: flex;
      align-items: center;
      gap: 12px;
      padding: 0 22px;
      border-bottom: 0;
      background: var(--surface-black);
      color: #ffffff;
    }
    h1 {
      font-family: "SF Pro Text", system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      font-size: 12px;
      line-height: 1;
      margin: 0 18px 0 0;
      font-weight: 600;
      letter-spacing: -0.12px;
      white-space: nowrap;
    }
    .control {
      display: inline-flex;
      align-items: center;
      gap: 8px;
      color: var(--body-muted-on-dark);
      font-size: 12px;
      line-height: 1;
      letter-spacing: -0.12px;
    }
    select, button {
      height: 32px;
      border: 1px solid rgba(255, 255, 255, 0.18);
      background: var(--surface-tile);
      color: #ffffff;
      border-radius: 9999px;
      padding: 0 15px;
      font: inherit;
      font-size: 14px;
      line-height: 1;
      letter-spacing: -0.224px;
    }
    button {
      cursor: pointer;
      background: var(--primary);
      border-color: var(--primary);
      color: #ffffff;
      transition: transform 120ms ease, background-color 120ms ease;
    }
    button:active { transform: scale(0.95); }
    button:focus-visible {
      outline: 2px solid var(--primary-focus);
      outline-offset: 2px;
    }
    input[type="checkbox"] { accent-color: var(--primary); }
    input[type="number"],
    input[type="range"] {
      accent-color: var(--primary);
      font: inherit;
    }
    input[type="number"] {
      height: 32px;
      min-width: 72px;
      border: 1px solid var(--hairline);
      border-radius: 9999px;
      padding: 0 12px;
      color: var(--ink);
      background: var(--canvas);
      font-size: 14px;
    }
    main {
      height: calc(100vh - 44px);
      display: grid;
      grid-template-rows: 52px minmax(0, 1fr);
      background: var(--canvas-parchment);
    }
    .tabs {
      display: flex;
      align-items: center;
      gap: 8px;
      padding: 0 22px;
      border-bottom: 1px solid rgba(0, 0, 0, 0.08);
      background: rgba(245, 245, 247, 0.82);
      backdrop-filter: saturate(180%) blur(20px);
    }
    .tab {
      height: 34px;
      border: 1px solid transparent;
      background: transparent;
      color: var(--primary);
    }
    .tab.active {
      background: var(--primary);
      border-color: var(--primary);
      color: #ffffff;
    }
    .view {
      min-height: 0;
      display: none;
    }
    .view.active { display: block; }
    #graph {
      width: 100%;
      height: 100%;
      background: var(--canvas);
    }
    .chain-layout {
      height: 100%;
      display: grid;
      grid-template-columns: minmax(0, 1fr) 360px;
      border-top: 0;
    }
    #chain {
      overflow: auto;
      padding: 24px;
      background: var(--canvas-parchment);
    }
    .tool-node {
      display: grid;
      grid-template-columns: 120px minmax(160px, 1fr) 90px;
      gap: 10px;
      align-items: center;
      min-height: 44px;
      margin-bottom: 12px;
      padding: 17px 18px;
      border: 1px solid var(--hairline);
      border-left: 1px solid var(--hairline);
      border-radius: 18px;
      background: var(--canvas);
      cursor: pointer;
    }
    .tool-node.edit,
    .tool-node.test,
    .tool-node.error,
    .tool-node.search,
    .tool-node.cog {
      border-left-color: var(--hairline);
    }
    .tool-node.edit { border-left: 5px solid #dc2626; }
    .tool-node.test { border-left: 5px solid #16a34a; }
    .tool-node.error { border-left: 5px solid #991b1b; }
    .tool-node.search { border-left: 5px solid #7c3aed; }
    .tool-node.cog { border-left: 5px solid #0891b2; }
    .tool-node:hover {
      border-color: var(--primary);
    }
    .tool-name { font-weight: 600; }
    .tool-target, .tool-kind, .tool-status {
      color: var(--muted);
      font-size: 12px;
      line-height: 1.3;
      letter-spacing: -0.12px;
      overflow-wrap: anywhere;
    }
    aside {
      min-width: 0;
      border-left: 1px solid var(--hairline);
      background: var(--canvas);
      padding: 24px;
      overflow: auto;
    }
    aside h2, aside h3 {
      font-weight: 600;
      letter-spacing: -0.224px;
    }
    pre {
      white-space: pre-wrap;
      overflow-wrap: anywhere;
      background: var(--surface-tile);
      color: #ffffff;
      padding: 17px;
      border-radius: 18px;
      font-size: 12px;
      line-height: 1.43;
    }
    #warnings {
      margin-left: auto;
      color: var(--body-muted-on-dark);
      font-size: 12px;
      max-width: 36vw;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .recommendation-layout {
      height: 100%;
      display: grid;
      grid-template-columns: 360px minmax(0, 1fr);
      background: var(--canvas-parchment);
    }
    .ranking-controls,
    .ranking-results {
      min-width: 0;
      overflow: auto;
      padding: 24px;
    }
    .ranking-controls {
      border-right: 1px solid var(--hairline);
      background: var(--canvas);
    }
    .ranking-results {
      background: var(--canvas-parchment);
    }
    .ranking-header {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 12px;
      margin-bottom: 17px;
    }
    .ranking-header h2,
    .ranking-controls h3 {
      margin: 0;
      font-weight: 600;
      letter-spacing: -0.224px;
    }
    .ranking-meta {
      color: var(--muted);
      font-size: 12px;
      letter-spacing: -0.12px;
    }
    .topk-control {
      display: flex;
      align-items: center;
      gap: 8px;
      color: var(--muted);
      font-size: 14px;
      margin: 0 0 17px;
    }
    .secondary-button {
      background: var(--canvas);
      border-color: var(--primary);
      color: var(--primary);
    }
    .weight-grid {
      display: grid;
      gap: 12px;
    }
    .weight-card,
    .recommendation-card,
    .empty-state {
      border: 1px solid var(--hairline);
      border-radius: 18px;
      background: var(--canvas);
      padding: 17px;
    }
    .weight-card {
      display: grid;
      gap: 8px;
    }
    .weight-card label {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 12px;
      font-size: 14px;
      font-weight: 600;
      letter-spacing: -0.224px;
    }
    .weight-value {
      color: var(--muted);
      font-size: 12px;
      font-weight: 400;
      letter-spacing: -0.12px;
    }
    .weight-inputs {
      display: grid;
      grid-template-columns: minmax(0, 1fr) 72px;
      gap: 10px;
      align-items: center;
    }
    .recommendation-list {
      display: grid;
      gap: 12px;
    }
    .recommendation-card {
      display: grid;
      grid-template-columns: 62px minmax(0, 1fr) 128px;
      gap: 17px;
      align-items: start;
    }
    .rank-badge {
      width: 44px;
      height: 44px;
      border-radius: 9999px;
      display: grid;
      place-items: center;
      background: var(--primary);
      color: #ffffff;
      font-size: 14px;
      font-weight: 600;
    }
    .recommendation-title {
      margin: 0 0 4px;
      font-size: 17px;
      font-weight: 600;
      letter-spacing: -0.374px;
      overflow-wrap: anywhere;
    }
    .recommendation-subtitle,
    .recommendation-action,
    .evidence-reason,
    .empty-state {
      color: var(--muted);
      font-size: 14px;
      line-height: 1.43;
      letter-spacing: -0.224px;
      overflow-wrap: anywhere;
    }
    .score-stack {
      text-align: right;
      color: var(--muted);
      font-size: 12px;
      letter-spacing: -0.12px;
    }
    .score-main {
      color: var(--ink);
      font-size: 24px;
      font-weight: 600;
      line-height: 1.1;
      letter-spacing: 0;
    }
    .evidence-tags {
      display: flex;
      flex-wrap: wrap;
      gap: 6px;
      margin: 10px 0;
    }
    .evidence-tag {
      border: 1px solid var(--hairline);
      border-radius: 9999px;
      padding: 4px 9px;
      color: var(--ink);
      background: var(--surface-pearl);
      font-size: 12px;
      letter-spacing: -0.12px;
    }
    .ranking-warning {
      margin: 0 0 12px;
      color: var(--muted);
      font-size: 14px;
    }
    .context-layout {
      height: 100%;
      display: grid;
      grid-template-columns: 320px minmax(0, 1fr);
      background: var(--canvas-parchment);
    }
    .context-sidebar,
    .context-main {
      min-width: 0;
      overflow: auto;
      padding: 24px;
    }
    .context-sidebar {
      border-right: 1px solid var(--hairline);
      background: var(--canvas);
    }
    .context-card {
      border: 1px solid var(--hairline);
      border-radius: 18px;
      background: var(--canvas);
      padding: 17px;
      margin-bottom: 12px;
      cursor: pointer;
    }
    .context-card.active,
    .context-card:hover {
      border-color: var(--primary);
    }
    .context-card.preview {
      border-color: #f59e0b;
      background: #fff7ed;
    }
    .context-title {
      margin: 0 0 4px;
      font-size: 14px;
      font-weight: 600;
      letter-spacing: -0.224px;
    }
    .context-meta {
      color: var(--muted);
      font-size: 12px;
      line-height: 1.43;
      letter-spacing: -0.12px;
      overflow-wrap: anywhere;
    }
    .context-panel {
      border: 1px solid var(--hairline);
      border-radius: 18px;
      background: var(--canvas);
      padding: 24px;
    }
    .context-pre {
      margin: 0;
      white-space: pre-wrap;
      overflow-wrap: anywhere;
      background: var(--surface-tile);
      color: #ffffff;
      border-radius: 18px;
      padding: 17px;
      font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
      font-size: 12px;
      line-height: 1.55;
      letter-spacing: 0;
    }
    .ctx-tag { color: #2997ff; font-weight: 600; }
    .ctx-context { color: #aab4c5; font-weight: 600; }
    .ctx-inserted { color: #5ac8fa; font-weight: 700; }
    .ctx-action { color: #fde68a; }
    .ctx-entity { color: #93c5fd; font-weight: 600; }
    .ctx-score { color: #bbf7d0; }
    .ctx-evidence { color: #c4b5fd; }
    .ctx-why { color: #fed7aa; }
    @media (max-width: 920px) {
      header { gap: 8px; padding: 0 12px; }
      .recommendation-layout,
      .context-layout,
      .chain-layout {
        grid-template-columns: 1fr;
      }
      .ranking-controls,
      .context-sidebar,
      aside {
        border-right: 0;
        border-left: 0;
        border-bottom: 1px solid var(--hairline);
      }
      .recommendation-card {
        grid-template-columns: 48px minmax(0, 1fr);
      }
      .score-stack {
        grid-column: 1 / -1;
        text-align: left;
      }
    }
  </style>
</head>
<body>
  <header>
    <h1>COG Visualization</h1>
    <label class="control">Scope
      <select id="scope">
        <option value="turn">当前 turn</option>
        <option value="session">当前 session</option>
      </select>
    </label>
    <label class="control">
      <input id="contains" type="checkbox" />
      contains
    </label>
    <button id="fit">Fit</button>
    <button id="refresh">Refresh</button>
    <div id="warnings"></div>
  </header>
  <main>
    <nav class="tabs">
      <button class="tab active" data-view="graph-view">实体依赖图</button>
      <button class="tab" data-view="chain-view">Agent 调用链路</button>
    </nav>
    <section id="graph-view" class="view active">
      <div id="graph"></div>
    </section>
    <section id="chain-view" class="view">
      <div class="chain-layout">
        <div id="chain"></div>
        <aside>
          <h2>调用详情</h2>
          <div id="detail">选择一个工具调用节点。</div>
        </aside>
      </div>
    </section>
  </main>
  <script>
    let cy = null;
    let graphData = null;
    let recommendationSummary = null;
    let injectionSnapshots = null;
    let currentWeights = {};
    let activeGraphSignature = '';

    const scopeEl = document.getElementById('scope');
    const containsEl = document.getElementById('contains');
    const warningsEl = document.getElementById('warnings');
    const weightDefinitions = [
      ['cog_graph', 'COG graph', false],
      ['trajectory', 'Trajectory', false],
      ['error', 'Error', false],
      ['search', 'Search', false],
      ['risk', 'Risk', false],
      ['confidence_bonus', 'Confidence bonus', false],
      ['already_seen_penalty', 'Already seen penalty', true],
      ['self_target_penalty', 'Self target penalty', true],
      ['low_confidence_penalty', 'Low confidence penalty', true]
    ];

    function ensureRankingChrome() {
      const tabs = document.querySelector('.tabs');
      const main = document.querySelector('main');
      if (tabs && !document.querySelector('[data-view="ranking-view"]')) {
        const tab = document.createElement('button');
        tab.className = 'tab';
        tab.dataset.view = 'ranking-view';
        tab.textContent = 'Recommendation ranking';
        tabs.appendChild(tab);
      }
      if (tabs && !document.querySelector('[data-view="context-view"]')) {
        const tab = document.createElement('button');
        tab.className = 'tab';
        tab.dataset.view = 'context-view';
        tab.textContent = 'Injected context';
        tabs.appendChild(tab);
      }
      if (main && !document.getElementById('ranking-view')) {
        const section = document.createElement('section');
        section.id = 'ranking-view';
        section.className = 'view';
        section.innerHTML = `
          <div class="recommendation-layout">
            <aside class="ranking-controls">
              <div class="ranking-header">
                <h2>Weights</h2>
                <button id="reset-weights" class="secondary-button">Reset</button>
              </div>
              <label class="topk-control">Top-K
                <input id="top-k" type="number" min="1" max="10" value="5" />
              </label>
              <label class="topk-control">Context characters
                <input id="runtime-max-chars" type="number" min="200" max="4000" value="1200" />
              </label>
              <label class="toggle-line">
                <input id="runtime-enabled" type="checkbox" checked />
                <span>Inject into Agent context</span>
              </label>
              <button id="apply-runtime-config" class="secondary-button">Apply to Agent</button>
              <div id="runtime-config-status" class="ranking-meta"></div>
              <div id="weight-grid" class="weight-grid"></div>
            </aside>
            <section class="ranking-results">
              <div class="ranking-header">
                <div>
                  <h2>Recommendation ranking</h2>
                  <div id="ranking-meta" class="ranking-meta"></div>
                </div>
              </div>
              <div id="ranking-warnings"></div>
              <div id="recommendation-list" class="recommendation-list"></div>
            </section>
          </div>
        `;
        main.appendChild(section);
      }
      if (main && !document.getElementById('context-view')) {
        const section = document.createElement('section');
        section.id = 'context-view';
        section.className = 'view';
        section.innerHTML = `
          <div class="context-layout">
            <aside class="context-sidebar">
              <div class="ranking-header">
                <div>
                  <h2>Injected context</h2>
                  <div id="context-meta" class="ranking-meta"></div>
                </div>
              </div>
              <div id="context-list"></div>
            </aside>
            <section class="context-main">
              <div class="context-panel">
                <div class="ranking-header">
                  <div>
                    <h2>Prompt context snapshot</h2>
                    <div id="context-detail-meta" class="ranking-meta"></div>
                  </div>
                </div>
                <div id="context-warnings"></div>
                <pre id="context-detail" class="context-pre">No injected recommendation context yet.</pre>
              </div>
            </section>
          </div>
        `;
        main.appendChild(section);
      }
    }

    function normalizeChromeText() {
      const turnOption = document.querySelector('option[value="turn"]');
      const sessionOption = document.querySelector('option[value="session"]');
      const graphTab = document.querySelector('[data-view="graph-view"]');
      const chainTab = document.querySelector('[data-view="chain-view"]');
      const rankingTab = document.querySelector('[data-view="ranking-view"]');
      const contextTab = document.querySelector('[data-view="context-view"]');
      const detailTitle = document.querySelector('aside h2');
      const detail = document.getElementById('detail');
      if (turnOption) turnOption.textContent = 'Current turn';
      if (sessionOption) sessionOption.textContent = 'Current session';
      if (graphTab) graphTab.textContent = 'Entity graph';
      if (chainTab) chainTab.textContent = 'Agent call chain';
      if (rankingTab) rankingTab.textContent = 'Recommendation ranking';
      if (contextTab) contextTab.textContent = 'Injected context';
      if (detailTitle) detailTitle.textContent = 'Call detail';
      if (detail) detail.textContent = 'Select a tool call node.';
    }

    async function loadData() {
      const params = new URLSearchParams({
        scope: scopeEl.value,
        include_contains: containsEl.checked ? 'true' : 'false',
        limit: '300'
      });
      const res = await fetch('/api/visualization/session-graph?' + params.toString());
      graphData = await res.json();
      warningsEl.textContent = (graphData.warnings || []).join(' | ');
      renderGraph();
      renderChain();
      await loadRecommendationSummary();
      await loadRecommendationInjections();
    }

    async function loadRecommendationSummary() {
      const params = new URLSearchParams({
        scope: scopeEl.value,
        limit: '300'
      });
      const res = await fetch('/api/recommendations/summary?' + params.toString());
      recommendationSummary = await res.json();
      if (!Object.keys(currentWeights).length) {
        currentWeights = { ...(recommendationSummary.default_weights || {}) };
      }
      renderWeightControls();
      renderRecommendationRanking();
      await loadRuntimeConfig();
    }

    async function loadRuntimeConfig() {
      const res = await fetch('/api/recommendations/runtime-config');
      if (!res.ok) return;
      const config = await res.json();
      const topK = document.getElementById('top-k');
      const maxChars = document.getElementById('runtime-max-chars');
      const enabled = document.getElementById('runtime-enabled');
      if (topK) topK.value = String(config.max_recommendations || 5);
      if (maxChars) maxChars.value = String(config.max_total_chars || 1200);
      if (enabled) enabled.checked = config.enabled !== false;
      renderRecommendationRanking();
    }

    async function loadRecommendationInjections() {
      const params = new URLSearchParams({
        scope: scopeEl.value,
        limit: '20'
      });
      const res = await fetch('/api/recommendations/injections?' + params.toString());
      injectionSnapshots = await res.json();
      renderInjectedContext();
    }

    async function applyRuntimeConfig() {
      const status = document.getElementById('runtime-config-status');
      const current = await fetch('/api/recommendations/runtime-config').then(res => res.json());
      current.max_recommendations = Math.max(1, Math.min(10, Number(document.getElementById('top-k')?.value || 5)));
      current.max_total_chars = Math.max(200, Math.min(4000, Number(document.getElementById('runtime-max-chars')?.value || 1200)));
      current.enabled = document.getElementById('runtime-enabled')?.checked !== false;
      const res = await fetch('/api/recommendations/runtime-config', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(current)
      });
      if (status) status.textContent = res.ok ? 'Applied to the next recommendation trigger.' : 'Failed to update runtime config.';
    }

    function renderWeightControls() {
      const grid = document.getElementById('weight-grid');
      if (!grid || !recommendationSummary) return;
      grid.innerHTML = '';
      for (const [key, label, isPenalty] of weightDefinitions) {
        const value = Number(currentWeights[key] ?? recommendationSummary.default_weights?.[key] ?? 0);
        const card = document.createElement('div');
        card.className = 'weight-card';
        card.innerHTML = `
          <label>
            <span>${escapeHtml(label)}</span>
            <span class="weight-value" data-weight-value="${key}">${isPenalty ? 'Penalty' : 'Signal'} · ${formatScore(value)}</span>
          </label>
          <div class="weight-inputs">
            <input data-weight="${key}" type="range" min="0" max="1" step="0.01" value="${value}" />
            <input data-weight-number="${key}" type="number" min="0" max="1" step="0.01" value="${value.toFixed(2)}" />
          </div>
        `;
        grid.appendChild(card);
      }
      grid.querySelectorAll('[data-weight]').forEach(input => {
        input.addEventListener('input', event => {
          updateWeight(event.target.dataset.weight, Number(event.target.value));
        });
      });
      grid.querySelectorAll('[data-weight-number]').forEach(input => {
        input.addEventListener('input', event => {
          updateWeight(event.target.dataset.weightNumber, Number(event.target.value));
        });
      });
    }

    function updateWeight(key, value) {
      if (!Number.isFinite(value)) return;
      currentWeights[key] = Math.max(0, Math.min(1, value));
      const range = document.querySelector(`[data-weight="${cssEscape(key)}"]`);
      const number = document.querySelector(`[data-weight-number="${cssEscape(key)}"]`);
      const label = document.querySelector(`[data-weight-value="${cssEscape(key)}"]`);
      if (range && Number(range.value) !== currentWeights[key]) range.value = String(currentWeights[key]);
      if (number && Number(number.value) !== currentWeights[key]) number.value = currentWeights[key].toFixed(2);
      if (label) {
        const definition = weightDefinitions.find(([definitionKey]) => definitionKey === key);
        label.textContent = `${definition?.[2] ? 'Penalty' : 'Signal'} · ${formatScore(currentWeights[key])}`;
      }
      renderRecommendationRanking();
    }

    function renderRecommendationRanking() {
      const list = document.getElementById('recommendation-list');
      const meta = document.getElementById('ranking-meta');
      const warnings = document.getElementById('ranking-warnings');
      if (!list || !meta || !warnings) return;
      if (!recommendationSummary) {
        list.innerHTML = '<div class="empty-state">No recommendation data loaded.</div>';
        return;
      }
      const records = (recommendationSummary.records || [])
        .map(record => ({ ...record, client_score: scoreRecommendation(record) }))
        .sort((left, right) => {
          if (right.client_score !== left.client_score) return right.client_score - left.client_score;
          const leftName = left.entity?.qualified_name || '';
          const rightName = right.entity?.qualified_name || '';
          return leftName.localeCompare(rightName);
        });
      const topKInput = document.getElementById('top-k');
      const topK = Math.max(1, Math.min(50, Number(topKInput?.value || 5)));
      const visible = records.slice(0, topK);
      const generatedAt = recommendationSummary.generated_at ? new Date(recommendationSummary.generated_at).toLocaleString() : 'unknown time';
      meta.textContent = `${records.length} records · scope ${recommendationSummary.scope || scopeEl.value} · generated ${generatedAt}`;
      warnings.innerHTML = (recommendationSummary.warnings || [])
        .map(item => `<p class="ranking-warning">${escapeHtml(item)}</p>`)
        .join('');
      if (!visible.length) {
        list.innerHTML = '<div class="empty-state">No recommendation records available.</div>';
        return;
      }
      list.innerHTML = visible.map((record, index) => recommendationCard(record, index + 1)).join('');
    }

    function scoreRecommendation(record) {
      const parts = record.score_parts || {};
      const positive =
        weight('cog_graph') * part(parts, 'cog_graph') +
        weight('trajectory') * part(parts, 'trajectory') +
        weight('error') * part(parts, 'error') +
        weight('search') * part(parts, 'search') +
        weight('risk') * part(parts, 'risk') +
        weight('confidence_bonus') * part(parts, 'confidence_bonus');
      const penalty =
        weight('already_seen_penalty') * part(parts, 'already_seen_penalty') +
        weight('self_target_penalty') * part(parts, 'self_target_penalty') +
        weight('low_confidence_penalty') * part(parts, 'low_confidence_penalty');
      return Math.max(0, Math.min(1, positive - penalty));
    }

    function recommendationCard(record, rank) {
      const entity = record.entity || {};
      const fullName = entity.qualified_name || 'unknown entity';
      const short = shortName(fullName);
      const evidence = record.evidence || [];
      const toolPath = (record.tool_path || []).map(step => escapeHtml(step)).join(' &rarr; ');
      const tags = evidence.slice(0, 6).map(item =>
        `<span class="evidence-tag">${escapeHtml(sourceLabel(item.source))} · ${formatScore(item.weight)}</span>`
      ).join('');
      const reasons = evidence.slice(0, 3).map(item =>
        `<div class="evidence-reason">${escapeHtml(item.reason || '')}</div>`
      ).join('');
      return `
        <article class="recommendation-card">
          <div class="rank-badge">#${rank}</div>
          <div>
            <h3 class="recommendation-title">${escapeHtml(short)}</h3>
            <div class="recommendation-subtitle">${escapeHtml(fullName)}</div>
            ${toolPath ? `<div class="evidence-reason">Suggested tools: ${toolPath}</div>` : ''}
            <div class="recommendation-action">${escapeHtml(actionLabel(record.suggested_action))} · ${escapeHtml(entity.kind || 'unknown')}</div>
            <div class="evidence-tags">${tags}</div>
            ${reasons}
          </div>
          <div class="score-stack">
            <div class="score-main">${formatScore(record.client_score)}</div>
            <div>client score</div>
            <div>server ${formatScore(record.server_score)}</div>
          </div>
        </article>
      `;
    }

    function renderInjectedContext(selectedId) {
      const list = document.getElementById('context-list');
      const meta = document.getElementById('context-meta');
      const detailMeta = document.getElementById('context-detail-meta');
      const detail = document.getElementById('context-detail');
      const warnings = document.getElementById('context-warnings');
      if (!list || !meta || !detailMeta || !detail || !warnings) return;
      if (!injectionSnapshots) {
        list.innerHTML = '<div class="empty-state">No context data loaded.</div>';
        return;
      }
      const injections = injectionSnapshots.injections || [];
      const preview = injectionSnapshots.preview || null;
      const generatedAt = injectionSnapshots.generated_at ? new Date(injectionSnapshots.generated_at).toLocaleString() : 'unknown time';
      meta.textContent = `${injections.length} snapshots${preview ? ' / 1 preview' : ''} · scope ${injectionSnapshots.scope || scopeEl.value} · generated ${generatedAt}`;
      warnings.innerHTML = (injectionSnapshots.warnings || [])
        .map(item => `<p class="ranking-warning">${escapeHtml(item)}</p>`)
        .join('');
      if (!injections.length && !preview) {
        list.innerHTML = '<div class="empty-state">No injected recommendation context or pending preview yet.</div>';
        detailMeta.textContent = 'No context available';
        detail.innerHTML = 'No injected recommendation context has been recorded, and no pending recommendations can be rendered under the current runtime config.';
        return;
      }
      const previewItem = preview ? { ...preview, id: '__preview__' } : null;
      const items = previewItem ? [previewItem, ...injections] : injections;
      const selected = items.find(item => item.id === selectedId) || items[0];
      list.innerHTML = items.map(item => {
        if (item.id === '__preview__') {
          const count = (item.recommendation_ids || []).length;
          return `
            <article class="context-card preview ${item.id === selected.id ? 'active' : ''}" data-context-id="__preview__">
              <h3 class="context-title">Pending context preview</h3>
              <div class="context-meta">${count} recommendation${count === 1 ? '' : 's'} would be inserted</div>
              <div class="context-meta">not yet injected</div>
            </article>
          `;
        }
        const created = item.created_at ? new Date(item.created_at).toLocaleString() : 'unknown time';
        const count = (item.recommendation_ids || []).length;
        return `
          <article class="context-card ${item.id === selected.id ? 'active' : ''}" data-context-id="${escapeHtml(item.id)}">
            <h3 class="context-title">${escapeHtml(created)}</h3>
            <div class="context-meta">turn ${escapeHtml(item.turn_id || '')}</div>
            <div class="context-meta">${count} recommendation${count === 1 ? '' : 's'} inserted</div>
          </article>
        `;
      }).join('');
      list.querySelectorAll('[data-context-id]').forEach(card => {
        card.addEventListener('click', () => renderInjectedContext(card.dataset.contextId));
      });
      if (selected.id === '__preview__') {
        const previewGenerated = selected.generated_at ? new Date(selected.generated_at).toLocaleString() : generatedAt;
        detailMeta.textContent = `Preview generated ${previewGenerated} · ${selected.note || 'not yet injected'}`;
        detail.innerHTML = highlightRecommendationContext(selected.context_text || '');
        return;
      }
      const created = selected.created_at ? new Date(selected.created_at).toLocaleString() : 'unknown time';
      const hasAuditWindow = Boolean(selected.request_context_excerpt);
      detailMeta.textContent = `Inserted ${created} · session ${selected.session_id || ''} · turn ${selected.turn_id || ''}${hasAuditWindow ? ' · request context window captured' : ''}`;
      detail.innerHTML = highlightRecommendationContext(selected.request_context_excerpt || selected.context_text || '');
    }

    function highlightRecommendationContext(value) {
      return escapeHtml(value)
        .split('\n')
        .map(line => {
          if (line.startsWith('&lt;context_before') || line.startsWith('&lt;/context_before') ||
              line.startsWith('&lt;context_after') || line.startsWith('&lt;/context_after')) {
            return `<span class="ctx-context">${line}</span>`;
          }
          if (line.startsWith('&lt;inserted_recommendation_context') || line.startsWith('&lt;/inserted_recommendation_context')) {
            return `<span class="ctx-inserted">${line}</span>`;
          }
          if (line.startsWith('&lt;repository_recommendations') || line.startsWith('&lt;/repository_recommendations')) {
            return `<span class="ctx-tag">${line}</span>`;
          }
          if (line.trim().startsWith('action:')) return `<span class="ctx-action">${line}</span>`;
          if (line.trim().startsWith('entity:')) return `<span class="ctx-entity">${line}</span>`;
          if (line.trim().startsWith('score:')) return `<span class="ctx-score">${line}</span>`;
          if (line.trim().startsWith('evidence:')) return `<span class="ctx-evidence">${line}</span>`;
          if (line.trim().startsWith('why:')) return `<span class="ctx-why">${line}</span>`;
          return line;
        })
        .join('\n');
    }

    function weight(key) {
      return Number(currentWeights[key] ?? recommendationSummary?.default_weights?.[key] ?? 0);
    }

    function part(parts, key) {
      return Number(parts?.[key] || 0);
    }

    function formatScore(value) {
      return Number(value || 0).toFixed(2);
    }

    function sourceLabel(source) {
      return String(source || '').replaceAll('_', ' ');
    }

    function actionLabel(action) {
      return String(action || 'read').replaceAll('_', ' ');
    }

    function cssEscape(value) {
      if (window.CSS && CSS.escape) return CSS.escape(value);
      return String(value).replaceAll('"', '\\"');
    }

    function renderGraph() {
      const modified = new Set(graphData.modified_entities || []);
      const added = new Set(graphData.added_entities || []);
      const deleted = new Set(graphData.deleted_entities || []);
      const impacted = new Set(graphData.impacted_entities || []);
      activeGraphSignature = graphSignature(graphData);
      const cachedPositions = loadGraphPositions(activeGraphSignature);
      const elements = [];
      const entities = graphData.entities || [];
      for (let index = 0; index < entities.length; index += 1) {
        const entity = entities[index];
        let state = 'normal';
        if (deleted.has(entity.id)) state = 'deleted';
        else if (added.has(entity.id)) state = 'added';
        else if (modified.has(entity.id)) state = 'modified';
        else if (impacted.has(entity.id)) state = 'impacted';
        const cached = cachedPositions?.[entity.id];
        elements.push({
          data: {
            id: entity.id,
            label: entity.display_name || shortName(entity.name),
            fullLabel: entity.name,
            kind: entity.kind,
            state
          },
          position: cached || deterministicPosition(entity.id, index, entities.length)
        });
      }
      const edgeBuckets = new Map();
      for (const rel of graphData.relations || []) {
        const pairKey = [rel.source, rel.target].sort().join('|');
        const index = edgeBuckets.get(pairKey) || 0;
        edgeBuckets.set(pairKey, index + 1);
        const sign = index % 2 === 0 ? 1 : -1;
        const magnitude = 26 + Math.floor(index / 2) * 18;
        elements.push({
          data: {
            id: rel.id,
            source: rel.source,
            target: rel.target,
            label: rel.kind,
            kind: rel.kind,
            cpDist: sign * magnitude,
            cpWeight: 0.5,
            laneKey: pairKey
          }
        });
      }
      if (cy) {
        cy.destroy();
        cy = null;
      }
      cy = cytoscape({
        container: document.getElementById('graph'),
        elements,
        wheelSensitivity: 0.18,
        style: [
          { selector: 'node', style: {
            'label': 'data(label)',
            'font-size': 11,
            'text-valign': 'center',
            'text-halign': 'center',
            'text-wrap': 'wrap',
            'text-max-width': 78,
            'background-color': '#ffffff',
            'border-color': '#e0e0e0',
            'border-width': 1,
            'color': '#1d1d1f',
            'width': 58,
            'height': 58
          }},
          { selector: 'node.hover', style: {
            'label': 'data(fullLabel)',
            'font-size': 10,
            'text-max-width': 220,
            'text-background-color': '#f5f5f7',
            'text-background-opacity': 0.94,
            'text-background-padding': 3,
            'z-index': 10
          }},
          { selector: 'node[state = "modified"]', style: {
            'background-color': '#fecaca',
            'border-color': '#dc2626',
            'border-width': 4,
            'shadow-blur': 28,
            'shadow-color': '#dc2626',
            'shadow-opacity': 0.55,
            'shadow-offset-x': 0,
            'shadow-offset-y': 0,
            'z-index': 7
          }},
          { selector: 'node[state = "added"]', style: {
            'background-color': '#d1fae5',
            'border-color': '#10b981',
            'border-width': 4,
            'shadow-blur': 26,
            'shadow-color': '#10b981',
            'shadow-opacity': 0.44,
            'shadow-offset-x': 0,
            'shadow-offset-y': 0,
            'z-index': 7
          }},
          { selector: 'node[state = "deleted"]', style: {
            'background-color': '#f3e8ff',
            'border-color': '#7e22ce',
            'border-style': 'dashed',
            'border-width': 4,
            'color': '#581c87',
            'shadow-blur': 28,
            'shadow-color': '#9333ea',
            'shadow-opacity': 0.52,
            'shadow-offset-x': 0,
            'shadow-offset-y': 0,
            'z-index': 7
          }},
          { selector: 'node[state = "impacted"]', style: {
            'background-color': '#fde68a',
            'border-color': '#f59e0b',
            'border-width': 3,
            'shadow-blur': 24,
            'shadow-color': '#f59e0b',
            'shadow-opacity': 0.46,
            'shadow-offset-x': 0,
            'shadow-offset-y': 0,
            'z-index': 6
          }},
          { selector: 'edge', style: {
            'curve-style': 'unbundled-bezier',
            'control-point-distances': 'data(cpDist)',
            'control-point-weights': 'data(cpWeight)',
            'target-arrow-shape': 'triangle',
            'target-arrow-color': '#7a7a7a',
            'line-color': '#7a7a7a',
            'width': 1.1,
            'opacity': 0.38,
            'label': '',
            'font-size': 10,
            'text-background-color': '#ffffff',
            'text-background-opacity': 0.9,
            'text-background-padding': 2
          }},
          { selector: 'edge.hover', style: {
            'label': 'data(label)',
            'line-color': '#0066cc',
            'target-arrow-color': '#0066cc',
            'width': 3,
            'opacity': 0.95,
            'z-index': 9
          }},
          { selector: 'edge.focus', style: {
            'line-color': '#0066cc',
            'target-arrow-color': '#0066cc',
            'width': 2.6,
            'opacity': 0.92,
            'z-index': 8
          }},
          { selector: '.dimmed', style: {
            'opacity': 0.14
          }}
        ],
        layout: cachedPositions ? {
          name: 'preset',
          fit: true,
          padding: 80
        } : {
          name: 'cose',
          animate: true,
          animationDuration: 650,
          refresh: 20,
          fit: true,
          padding: 80,
          randomize: false,
          nodeRepulsion: 1400000,
          nodeOverlap: 24,
          idealEdgeLength: 170,
          edgeElasticity: 90,
          nestingFactor: 1.2,
          gravity: 0.08,
          numIter: 2600,
          initialTemp: 220,
          coolingFactor: 0.94,
          minTemp: 1.0
        }
      });
      cy.on('layoutstop', () => {
        applyEdgeLanes();
        saveGraphPositions(activeGraphSignature);
      });
      cy.on('free', 'node', () => saveGraphPositions(activeGraphSignature));
      cy.on('mouseover', 'node', event => {
        const node = event.target;
        node.addClass('hover');
        cy.elements().addClass('dimmed');
        node.removeClass('dimmed');
        const edges = node.connectedEdges();
        edges.removeClass('dimmed').addClass('focus');
        edges.connectedNodes().removeClass('dimmed');
      });
      cy.on('mouseout', 'node', event => {
        event.target.removeClass('hover');
        cy.elements().removeClass('dimmed focus');
      });
      cy.on('mouseover', 'edge', event => event.target.addClass('hover'));
      cy.on('mouseout', 'edge', event => event.target.removeClass('hover'));
      setTimeout(() => {
        if (!cy) return;
        applyEdgeLanes();
        cy.fit(undefined, 48);
      }, 80);
    }

    function graphSignature(data) {
      const nodes = (data.entities || []).map(entity => entity.id).sort().join('|');
      const edges = (data.relations || [])
        .map(rel => `${rel.source}->${rel.target}:${rel.kind}`)
        .sort()
        .join('|');
      return String(stableHash(`${scopeEl.value}|${containsEl.checked}|${nodes}|${edges}`));
    }

    function loadGraphPositions(signature) {
      try {
        const raw = localStorage.getItem(`cog-graph-positions:${signature}`);
        if (!raw) return null;
        const parsed = JSON.parse(raw);
        return parsed && typeof parsed === 'object' ? parsed : null;
      } catch {
        return null;
      }
    }

    function saveGraphPositions(signature) {
      if (!cy || !signature) return;
      const positions = {};
      cy.nodes().forEach(node => {
        const position = node.position();
        positions[node.id()] = {
          x: Math.round(position.x * 100) / 100,
          y: Math.round(position.y * 100) / 100
        };
      });
      try {
        localStorage.setItem(`cog-graph-positions:${signature}`, JSON.stringify(positions));
      } catch {
        // Browser storage is best-effort; graph rendering must continue without it.
      }
    }

    function deterministicPosition(id, index, total) {
      const count = Math.max(1, total);
      const hash = stableHash(id);
      const angle = ((index / count) * Math.PI * 2) + ((hash % 628) / 100);
      const ring = 260 + (hash % 7) * 42 + Math.floor(index / Math.max(1, Math.ceil(Math.sqrt(count)))) * 14;
      return {
        x: Math.cos(angle) * ring,
        y: Math.sin(angle) * ring
      };
    }

    function stableHash(value) {
      let hash = 2166136261;
      const text = String(value || '');
      for (let index = 0; index < text.length; index += 1) {
        hash ^= text.charCodeAt(index);
        hash = Math.imul(hash, 16777619);
      }
      return hash >>> 0;
    }

    function applyEdgeLanes() {
      if (!cy) return;
      let sumX = 0;
      let sumY = 0;
      cy.nodes().forEach(node => {
        sumX += node.position('x');
        sumY += node.position('y');
      });
      const center = {
        x: sumX / Math.max(1, cy.nodes().length),
        y: sumY / Math.max(1, cy.nodes().length)
      };
      const buckets = new Map();
      cy.edges().forEach(edge => {
        const source = edge.source().id();
        const target = edge.target().id();
        const key = [source, target].sort().join('|');
        const bucket = buckets.get(key) || [];
        bucket.push(edge);
        buckets.set(key, bucket);
      });
      buckets.forEach(bucket => {
        bucket.sort((left, right) => left.id().localeCompare(right.id()));
        bucket.forEach((edge, index) => {
          const source = edge.source().position();
          const target = edge.target().position();
          const mid = { x: (source.x + target.x) / 2, y: (source.y + target.y) / 2 };
          const outward = ((mid.x - center.x) * (target.y - source.y) - (mid.y - center.y) * (target.x - source.x)) >= 0 ? 1 : -1;
          const pairOffset = index - (bucket.length - 1) / 2;
          const relationOffset = (stableHash(edge.data('kind') || edge.id()) % 7) - 3;
          const distance = outward * (34 + Math.abs(pairOffset) * 22 + Math.abs(relationOffset) * 4);
          edge.data('cpDist', Math.round(distance + pairOffset * 18 + relationOffset * 3));
          edge.data('cpWeight', 0.42 + ((stableHash(edge.id()) % 17) / 100));
        });
      });
    }

    function renderChain() {
      const chain = document.getElementById('chain');
      chain.innerHTML = '';
      for (const item of graphData.tool_chain || []) {
        const node = document.createElement('div');
        node.className = 'tool-node ' + classForKind(item.kind);
        node.innerHTML = `
          <div class="tool-name">${escapeHtml(item.tool_name)}</div>
          <div>
            <div class="tool-kind">${escapeHtml(item.kind)}</div>
            <div class="tool-target">${escapeHtml(item.target || '')}</div>
          </div>
          <div class="tool-status">${escapeHtml(item.status)}</div>
        `;
        node.addEventListener('click', () => showDetail(item));
        chain.appendChild(node);
      }
    }

    function showDetail(item) {
      document.getElementById('detail').innerHTML = `
        <p><strong>${escapeHtml(item.tool_name)}</strong></p>
        <p>${escapeHtml(item.ts)}</p>
        <h3>Input</h3>
        <pre>${escapeHtml(JSON.stringify(item.input_summary, null, 2))}</pre>
        <h3>Output</h3>
        <pre>${escapeHtml(item.output_summary || '')}</pre>
      `;
    }

    function classForKind(kind) {
      if (kind.includes('edit')) return 'edit';
      if (kind.includes('test')) return 'test';
      if (kind.includes('error')) return 'error';
      if (kind.includes('search')) return 'search';
      if (kind.includes('cog')) return 'cog';
      return '';
    }

    function shortName(name) {
      const parts = String(name || '').split('::').filter(Boolean);
      return parts.length ? parts[parts.length - 1] : String(name || '');
    }

    function escapeHtml(value) {
      return String(value)
        .replaceAll('&', '&amp;')
        .replaceAll('<', '&lt;')
        .replaceAll('>', '&gt;')
        .replaceAll('"', '&quot;')
        .replaceAll("'", '&#039;');
    }

    ensureRankingChrome();
    normalizeChromeText();
    document.querySelectorAll('.tab').forEach(tab => {
      tab.addEventListener('click', () => {
        document.querySelectorAll('.tab').forEach(t => t.classList.remove('active'));
        document.querySelectorAll('.view').forEach(v => v.classList.remove('active'));
        tab.classList.add('active');
        document.getElementById(tab.dataset.view).classList.add('active');
        if (tab.dataset.view === 'graph-view' && cy) setTimeout(() => cy.fit(undefined, 48), 50);
      });
    });
    document.getElementById('fit').addEventListener('click', () => cy && cy.fit(undefined, 48));
    document.getElementById('refresh').addEventListener('click', loadData);
    scopeEl.addEventListener('change', loadData);
    containsEl.addEventListener('change', loadData);
    document.getElementById('top-k').addEventListener('input', renderRecommendationRanking);
    document.getElementById('apply-runtime-config').addEventListener('click', applyRuntimeConfig);
    document.getElementById('reset-weights').addEventListener('click', () => {
      currentWeights = { ...(recommendationSummary?.default_weights || {}) };
      renderWeightControls();
      renderRecommendationRanking();
    });
    loadData();
  </script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_can_be_constructed() {
        let router = visualization_router(".");
        let _ = router;
    }

    #[test]
    fn index_html_contains_recommendation_ranking_frontend() {
        assert!(INDEX_HTML.contains("/api/recommendations/summary"));
        assert!(INDEX_HTML.contains("Recommendation ranking"));
        assert!(INDEX_HTML.contains("scoreRecommendation"));
        assert!(INDEX_HTML.contains("reset-weights"));
        assert!(INDEX_HTML.contains("/api/recommendations/runtime-config"));
        assert!(INDEX_HTML.contains("apply-runtime-config"));
        assert!(INDEX_HTML.contains("renderRecommendationRanking();"));
        assert!(INDEX_HTML.contains("loadGraphPositions"));
        assert!(INDEX_HTML.contains("applyEdgeLanes"));
        assert!(INDEX_HTML.contains("/api/recommendations/injections"));
        assert!(INDEX_HTML.contains("Injected context"));
        assert!(INDEX_HTML.contains("highlightRecommendationContext"));
        assert!(INDEX_HTML.contains("Pending context preview"));
    }
}
