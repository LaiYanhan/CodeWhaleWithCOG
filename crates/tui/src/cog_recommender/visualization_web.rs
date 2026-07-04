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

use super::visualization::{VisualizationScope, VisualizationStore};

const DEFAULT_LIMIT: usize = 200;

#[derive(Debug, Clone)]
struct VisualizationState {
    store: VisualizationStore,
}

#[derive(Debug, Deserialize)]
struct GraphQuery {
    #[serde(default)]
    scope: VisualizationScope,
    #[serde(default)]
    include_contains: bool,
    limit: Option<usize>,
}

pub fn visualization_router(workspace: impl Into<PathBuf>) -> Router {
    let state = VisualizationState {
        store: VisualizationStore::new(workspace),
    };
    Router::new()
        .route("/", get(index))
        .route("/api/visualization/session-graph", get(session_graph))
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

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>COG Visualization</title>
  <script src="https://unpkg.com/cytoscape@3.30.4/dist/cytoscape.min.js"></script>
  <style>
    :root {
      --bg: #f7f8fb;
      --panel: #ffffff;
      --line: #d8dee9;
      --text: #1f2937;
      --muted: #64748b;
      --edit: #dc2626;
      --impact: #f59e0b;
      --node: #dbeafe;
      --node-border: #3b82f6;
      --edge: #94a3b8;
    }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      min-height: 100vh;
      background: var(--bg);
      color: var(--text);
      font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    }
    header {
      height: 56px;
      display: flex;
      align-items: center;
      gap: 12px;
      padding: 0 16px;
      border-bottom: 1px solid var(--line);
      background: var(--panel);
    }
    h1 {
      font-size: 16px;
      line-height: 1;
      margin: 0 16px 0 0;
      font-weight: 700;
    }
    .control {
      display: inline-flex;
      align-items: center;
      gap: 8px;
      color: var(--muted);
      font-size: 13px;
    }
    select, button {
      height: 32px;
      border: 1px solid var(--line);
      background: #fff;
      color: var(--text);
      border-radius: 6px;
      padding: 0 10px;
      font: inherit;
    }
    button { cursor: pointer; }
    main {
      height: calc(100vh - 56px);
      display: grid;
      grid-template-rows: 44px minmax(0, 1fr);
    }
    .tabs {
      display: flex;
      align-items: end;
      gap: 4px;
      padding: 0 16px;
      border-bottom: 1px solid var(--line);
      background: #f1f5f9;
    }
    .tab {
      height: 34px;
      border: 1px solid transparent;
      background: transparent;
    }
    .tab.active {
      background: var(--panel);
      border-color: var(--line);
      border-bottom-color: var(--panel);
    }
    .view {
      min-height: 0;
      display: none;
    }
    .view.active { display: block; }
    #graph {
      width: 100%;
      height: 100%;
      background: #fff;
    }
    .chain-layout {
      height: 100%;
      display: grid;
      grid-template-columns: minmax(0, 1fr) 360px;
      border-top: 0;
    }
    #chain {
      overflow: auto;
      padding: 20px;
      background: #fff;
    }
    .tool-node {
      display: grid;
      grid-template-columns: 120px minmax(160px, 1fr) 90px;
      gap: 10px;
      align-items: center;
      min-height: 44px;
      margin-bottom: 10px;
      padding: 10px 12px;
      border: 1px solid var(--line);
      border-left: 5px solid #64748b;
      border-radius: 8px;
      background: #fff;
      cursor: pointer;
    }
    .tool-node.edit { border-left-color: var(--edit); }
    .tool-node.test { border-left-color: #16a34a; }
    .tool-node.error { border-left-color: #991b1b; }
    .tool-node.search { border-left-color: #7c3aed; }
    .tool-node.cog { border-left-color: #0891b2; }
    .tool-name { font-weight: 700; }
    .tool-target, .tool-kind, .tool-status { color: var(--muted); font-size: 12px; overflow-wrap: anywhere; }
    aside {
      min-width: 0;
      border-left: 1px solid var(--line);
      background: #f8fafc;
      padding: 16px;
      overflow: auto;
    }
    pre {
      white-space: pre-wrap;
      overflow-wrap: anywhere;
      background: #0f172a;
      color: #e2e8f0;
      padding: 12px;
      border-radius: 8px;
      font-size: 12px;
    }
    #warnings {
      margin-left: auto;
      color: #b45309;
      font-size: 12px;
      max-width: 36vw;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
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

    const scopeEl = document.getElementById('scope');
    const containsEl = document.getElementById('contains');
    const warningsEl = document.getElementById('warnings');

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
            'background-color': '#dbeafe',
            'border-color': '#3b82f6',
            'border-width': 1,
            'width': 58,
            'height': 58
          }},
          { selector: 'node.hover', style: {
            'label': 'data(fullLabel)',
            'font-size': 10,
            'text-max-width': 220,
            'text-background-color': '#fff',
            'text-background-opacity': 0.94,
            'text-background-padding': 3,
            'z-index': 10
          }},
          { selector: 'node[state = "modified"]', style: {
            'background-color': '#fecaca',
            'border-color': '#dc2626',
            'border-width': 4
          }},
          { selector: 'node[state = "impacted"]', style: {
            'background-color': '#fde68a',
            'border-color': '#f59e0b',
            'border-width': 3
          }},
          { selector: 'edge', style: {
            'curve-style': 'unbundled-bezier',
            'control-point-distances': 'data(cpDist)',
            'control-point-weights': 'data(cpWeight)',
            'target-arrow-shape': 'triangle',
            'target-arrow-color': '#94a3b8',
            'line-color': '#94a3b8',
            'width': 1.1,
            'opacity': 0.38,
            'label': '',
            'font-size': 10,
            'text-background-color': '#fff',
            'text-background-opacity': 0.9,
            'text-background-padding': 2
          }},
          { selector: 'edge.hover', style: {
            'label': 'data(label)',
            'line-color': '#334155',
            'target-arrow-color': '#334155',
            'width': 3,
            'opacity': 0.95,
            'z-index': 9
          }},
          { selector: 'edge.focus', style: {
            'line-color': '#334155',
            'target-arrow-color': '#334155',
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
}
