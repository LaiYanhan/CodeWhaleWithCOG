use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use tokio::net::TcpListener;

use super::recommendation_summary::{RecommendationSummary, RecommendationSummaryStore};
use super::visualization::{VisualizationScope, VisualizationStore};

const DEFAULT_LIMIT: usize = 200;

#[derive(Debug, Clone)]
struct VisualizationState {
    store: VisualizationStore,
    recommendation_store: RecommendationSummaryStore,
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

pub fn visualization_router(workspace: impl Into<PathBuf>) -> Router {
    let workspace = workspace.into();
    let state = VisualizationState {
        store: VisualizationStore::new(workspace.clone()),
        recommendation_store: RecommendationSummaryStore::new(workspace),
    };
    Router::new()
        .route("/", get(index))
        .route("/api/visualization/session-graph", get(session_graph))
        .route("/api/recommendations/summary", get(recommendation_summary))
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
    @media (max-width: 920px) {
      header { gap: 8px; padding: 0 12px; }
      .recommendation-layout,
      .chain-layout {
        grid-template-columns: 1fr;
      }
      .ranking-controls,
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
    let currentWeights = {};

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
                <input id="top-k" type="number" min="1" max="50" value="5" />
              </label>
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
    }

    function normalizeChromeText() {
      const turnOption = document.querySelector('option[value="turn"]');
      const sessionOption = document.querySelector('option[value="session"]');
      const graphTab = document.querySelector('[data-view="graph-view"]');
      const chainTab = document.querySelector('[data-view="chain-view"]');
      const rankingTab = document.querySelector('[data-view="ranking-view"]');
      const detailTitle = document.querySelector('aside h2');
      const detail = document.getElementById('detail');
      if (turnOption) turnOption.textContent = 'Current turn';
      if (sessionOption) sessionOption.textContent = 'Current session';
      if (graphTab) graphTab.textContent = 'Entity graph';
      if (chainTab) chainTab.textContent = 'Agent call chain';
      if (rankingTab) rankingTab.textContent = 'Recommendation ranking';
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
      const impacted = new Set(graphData.impacted_entities || []);
      const elements = [];
      for (const entity of graphData.entities || []) {
        let state = 'normal';
        if (modified.has(entity.id)) state = 'modified';
        else if (impacted.has(entity.id)) state = 'impacted';
        elements.push({
          data: {
            id: entity.id,
            label: entity.display_name || shortName(entity.name),
            fullLabel: entity.name,
            kind: entity.kind,
            state
          }
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
            cpWeight: 0.5
          }
        });
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
        layout: {
          name: 'cose',
          animate: true,
          animationDuration: 650,
          refresh: 20,
          fit: true,
          padding: 80,
          randomize: true,
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
      setTimeout(() => cy && cy.fit(undefined, 48), 80);
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
    }
}
