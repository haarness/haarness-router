"use strict";

const uiBase = document.body.dataset.uiBase || "/ui";
const state = {
  overview: null,
  metrics: null,
  rawMetrics: "",
  requestHistory: [],
  controller: null,
  responseText: "",
  pendingKey: "",
};

const $ = (selector, root = document) => root.querySelector(selector);
const $$ = (selector, root = document) => [...root.querySelectorAll(selector)];

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function formatNumber(value) {
  const number = Number(value || 0);
  if (number >= 1_000_000) return `${(number / 1_000_000).toFixed(number >= 10_000_000 ? 0 : 1)}m`;
  if (number >= 1_000) return `${(number / 1_000).toFixed(number >= 100_000 ? 0 : 1)}k`;
  return number.toLocaleString();
}

function formatDuration(seconds) {
  const value = Math.max(0, Number(seconds || 0));
  if (value < 60) return `${Math.floor(value)}s uptime`;
  if (value < 3600) return `${Math.floor(value / 60)}m uptime`;
  if (value < 86400) return `${Math.floor(value / 3600)}h ${Math.floor((value % 3600) / 60)}m uptime`;
  return `${Math.floor(value / 86400)}d ${Math.floor((value % 86400) / 3600)}h uptime`;
}

function formatTimestamp(seconds) {
  if (!seconds) return "Never";
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(Number(seconds) * 1000));
}

function toast(message, kind = "success") {
  const region = $("#toast-region");
  const item = element("div", `toast ${kind === "error" ? "error" : ""}`, message);
  region.append(item);
  window.setTimeout(() => item.remove(), 4200);
}

async function copyText(value, notice = "Copied") {
  try {
    await navigator.clipboard.writeText(value);
    toast(notice);
  } catch {
    toast("Clipboard access was unavailable", "error");
  }
}

async function api(path, options = {}) {
  const response = await fetch(`${uiBase}${path}`, {
    credentials: "same-origin",
    cache: "no-store",
    ...options,
  });
  const type = response.headers.get("content-type") || "";
  const payload = type.includes("json") ? await response.json() : await response.text();
  if (!response.ok) {
    const message = payload?.error?.message || payload || `Request failed with HTTP ${response.status}`;
    throw new Error(message);
  }
  return payload;
}

function navigate(view) {
  const target = $(`#view-${view}`) || $("#view-overview");
  $$(".view").forEach((item) => item.classList.toggle("active", item === target));
  $$(".nav-item").forEach((item) => item.classList.toggle("active", item.dataset.view === target.id.replace("view-", "")));
  $("#page-title").textContent = target.dataset.title || "Overview";
  $("#page-eyebrow").textContent = target.dataset.eyebrow || "Control room";
  const hash = target.id.replace("view-", "");
  if (location.hash !== `#${hash}`) history.replaceState(null, "", `#${hash}`);
  if (hash === "observability") refreshMetrics();
  if (hash === "keys") refreshKeys();
}

function initializeNavigation() {
  $$("[data-view]").forEach((button) => button.addEventListener("click", () => navigate(button.dataset.view)));
  $$("[data-go]").forEach((button) => button.addEventListener("click", () => navigate(button.dataset.go)));
  window.addEventListener("hashchange", () => navigate(location.hash.slice(1) || "overview"));
  navigate(location.hash.slice(1) || "overview");
}

function initializeTheme() {
  const preferred = localStorage.getItem("haarness.theme");
  if (preferred === "light" || preferred === "dark") document.documentElement.dataset.theme = preferred;
  $("#theme-toggle").addEventListener("click", () => {
    const next = document.documentElement.dataset.theme === "light" ? "dark" : "light";
    document.documentElement.dataset.theme = next;
    localStorage.setItem("haarness.theme", next);
  });
}

async function loadOverview() {
  try {
    state.overview = await api("/api/overview");
    renderOverview();
    renderModels();
    renderSettings();
    await refreshKeys();
    $("#router-status").textContent = "Ready";
  } catch (error) {
    $("#router-status").textContent = "Unavailable";
    toast(error.message, "error");
  }
}

function renderOverview() {
  const data = state.overview;
  if (!data) return;
  $("#sidebar-version").textContent = `v${data.version}`;
  $("#sidebar-security").textContent = data.security.basic_auth ? "Basic auth secured" : "Local UI unlocked";
  $("#uptime-label").textContent = formatDuration(Date.now() / 1000 - data.started_at);
  $("#stat-providers").textContent = data.providers.length;
  $("#stat-models").textContent = data.models.length;
  $("#stat-aliases").textContent = Object.keys(data.aliases).length;
  const closed = data.circuits.filter((circuit) => circuit.state === "closed").length;
  $("#stat-circuits").textContent = `${closed}/${data.circuits.length}`;

  const circuits = new Map(data.circuits.map((circuit) => [circuit.target, circuit.state]));
  const providerTable = $("#provider-table");
  providerTable.replaceChildren();
  for (const provider of data.providers) {
    const row = element("tr");
    const name = element("td");
    name.append(element("strong", "", provider.name));
    row.append(name, element("td", "", provider.kind));
    const credential = element("td");
    credential.append(element("span", `badge ${provider.credential_configured ? "success" : "warning"}`, provider.credential_configured ? "Configured" : "BYOK only"));
    row.append(credential);
    const related = [...circuits.entries()].filter(([target]) => target.startsWith(`${provider.name}/`));
    const providerState = related.some(([, value]) => value === "open") ? "open" : related.some(([, value]) => value === "half-open") ? "half-open" : "closed";
    const stateCell = element("td");
    stateCell.append(element("span", `badge ${providerState === "closed" ? "success" : providerState === "open" ? "danger" : "warning"}`, providerState));
    row.append(stateCell);
    providerTable.append(row);
  }

  const aliasList = $("#alias-list");
  aliasList.replaceChildren();
  for (const [name, definition] of Object.entries(data.aliases)) {
    const card = element("section", "alias-item");
    const header = element("header");
    header.append(element("strong", "", name), element("span", "tiny-label", `${definition.targets.length} targets`));
    const list = element("ol");
    definition.targets.forEach((target) => list.append(element("li", "", target)));
    card.append(header, list);
    aliasList.append(card);
  }
  if (!aliasList.children.length) aliasList.append(element("p", "empty-copy", "No aliases configured."));

  const select = $("#playground-model");
  const current = select.value;
  select.replaceChildren();
  for (const alias of Object.keys(data.aliases)) select.append(new Option(`${alias} · alias`, alias));
  for (const model of data.models) select.append(new Option(model.id, model.id));
  if ([...select.options].some((option) => option.value === current)) select.value = current;
  else if ([...select.options].some((option) => option.value === "default")) select.value = "default";
}

function renderModels(filter = "") {
  const data = state.overview;
  if (!data) return;
  const query = filter.trim().toLowerCase();
  const grid = $("#model-grid");
  grid.replaceChildren();
  for (const model of data.models) {
    const capabilities = Object.entries(model.capabilities).filter(([, enabled]) => enabled).map(([name]) => name);
    const search = `${model.id} ${model.provider} ${capabilities.join(" ")}`.toLowerCase();
    if (query && !search.includes(query)) continue;
    const card = element("article", "panel model-card");
    const header = element("header");
    header.append(element("span", "model-provider", model.provider), element("span", "badge success", "declared"));
    const title = element("h3", "", model.id);
    const description = element("p", "", `${capabilities.length} declared capabilities · capability-gated routing`);
    const list = element("div", "capability-list");
    capabilities.forEach((capability) => list.append(element("span", "capability active", capability.replaceAll("_", " "))));
    card.append(header, title, description, list);
    grid.append(card);
  }
  if (!grid.children.length) grid.append(element("article", "panel loading-card", "No models match this filter."));

  const routeMap = $("#route-map");
  routeMap.replaceChildren();
  for (const [alias, definition] of Object.entries(data.aliases)) {
    const row = element("div", "route-map-row");
    row.append(element("strong", "", alias));
    const chain = element("div", "route-chain");
    definition.targets.forEach((target, index) => {
      if (index) chain.append(element("i", "", "→"));
      chain.append(element("span", "", target));
    });
    row.append(chain);
    routeMap.append(row);
  }
}

function parseMetrics(raw) {
  const families = new Map();
  for (const line of raw.split(/\r?\n/)) {
    if (!line || line.startsWith("#")) continue;
    const match = line.match(/^([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{([^}]*)\})?\s+([^\s]+)$/);
    if (!match) continue;
    const labels = {};
    const labelText = match[2] || "";
    const labelPattern = /([a-zA-Z_][a-zA-Z0-9_]*)="((?:\\.|[^"])*)"/g;
    for (const label of labelText.matchAll(labelPattern)) {
      labels[label[1]] = label[2].replaceAll("\\n", "\n").replaceAll('\\"', '"').replaceAll("\\\\", "\\");
    }
    const sample = { labels, value: Number(match[3]) };
    if (!families.has(match[1])) families.set(match[1], []);
    families.get(match[1]).push(sample);
  }
  return families;
}

function metric(name, fallback = 0) {
  return state.metrics?.get(name)?.[0]?.value ?? fallback;
}

async function refreshMetrics() {
  try {
    const response = await fetch("/metrics", { cache: "no-store", credentials: "same-origin" });
    if (!response.ok) throw new Error(`Metrics returned HTTP ${response.status}`);
    state.rawMetrics = await response.text();
    state.metrics = parseMetrics(state.rawMetrics);
    renderMetrics();
  } catch (error) {
    toast(error.message, "error");
  }
}

function renderMetrics() {
  const requests = metric("haarness_router_requests_total");
  const errors = metric("haarness_router_errors_total");
  const active = metric("haarness_router_active_streams");
  const firstMicros = metric("haarness_router_first_event_microseconds_total");
  const fallbacks = metric("haarness_router_fallback_attempts_total");
  const cancelled = metric("haarness_router_cancelled_streams_total");
  const errorRate = requests ? (errors / requests) * 100 : 0;
  const firstAverage = requests ? firstMicros / requests / 1000 : 0;
  $("#metric-requests").textContent = formatNumber(requests);
  $("#metric-active").textContent = formatNumber(active);
  $("#metric-error-rate").textContent = `${errorRate.toFixed(errorRate < 1 ? 2 : 1)}%`;
  $("#metric-first-event").textContent = `${firstAverage.toFixed(firstAverage < 10 ? 2 : 1)} ms`;
  $("#obs-requests").textContent = formatNumber(requests);
  $("#obs-errors").textContent = formatNumber(errors);
  $("#obs-fallbacks").textContent = formatNumber(fallbacks);
  $("#obs-cancelled").textContent = formatNumber(cancelled);
  $("#raw-metrics").textContent = state.rawMetrics;
  $("#metrics-updated").textContent = `Updated ${new Date().toLocaleTimeString()}`;

  state.requestHistory.push(requests);
  if (state.requestHistory.length > 28) state.requestHistory.shift();
  const deltas = state.requestHistory.map((value, index, values) => index ? Math.max(0, value - values[index - 1]) : 0);
  const max = Math.max(1, ...deltas);
  const spark = $("#request-sparkline");
  spark.replaceChildren();
  const padded = Array(Math.max(0, 28 - deltas.length)).fill(0).concat(deltas);
  padded.forEach((value) => {
    const bar = element("i");
    bar.style.height = `${Math.max(7, (value / max) * 100)}%`;
    spark.append(bar);
  });

  const routeNames = [
    "haarness_router_route_requests_total",
    "haarness_router_route_active_streams",
    "haarness_router_route_completed_streams_total",
    "haarness_router_route_failed_streams_total",
  ];
  const routes = new Map();
  routeNames.forEach((name, column) => {
    for (const sample of state.metrics.get(name) || []) {
      const key = `${sample.labels.provider}/${sample.labels.model}`;
      if (!routes.has(key)) routes.set(key, [0, 0, 0, 0]);
      routes.get(key)[column] = sample.value;
    }
  });
  const table = $("#route-metrics-table");
  table.replaceChildren();
  for (const [route, values] of routes) {
    const row = element("tr");
    const routeCell = element("td");
    routeCell.append(element("strong", "", route));
    row.append(routeCell, ...values.map((value) => element("td", "", formatNumber(value))));
    table.append(row);
  }
  if (!table.children.length) {
    const row = element("tr");
    const cell = element("td", "empty-cell", "No route metrics yet.");
    cell.colSpan = 5;
    row.append(cell);
    table.append(row);
  }
}

async function refreshKeys() {
  try {
    const data = await api("/api/keys");
    $("#bootstrap-key-count").textContent = data.bootstrap_key_count;
    $("#runtime-key-count").textContent = data.runtime_keys.length;
    $("#runtime-key-limit").textContent = `${data.runtime_keys.length}/${data.max_runtime_keys} process-local keys`;
    $("#create-key-button").disabled = !data.runtime_key_management;
    renderKeys(data.runtime_keys, data.runtime_key_management);
  } catch (error) {
    toast(error.message, "error");
  }
}

function renderKeys(keys, managementEnabled) {
  const table = $("#key-table");
  table.replaceChildren();
  for (const key of keys) {
    const row = element("tr");
    const label = element("td");
    label.append(element("strong", "", key.label));
    row.append(label, element("td", "", key.id), element("td", "", formatTimestamp(key.created_at)), element("td", "", formatTimestamp(key.last_used_at)));
    const action = element("td");
    const remove = element("button", "delete-button", "Revoke");
    remove.type = "button";
    remove.disabled = !managementEnabled;
    remove.addEventListener("click", () => revokeKey(key));
    action.append(remove);
    row.append(action);
    table.append(row);
  }
  if (!keys.length) {
    const row = element("tr");
    const cell = element("td", "empty-cell", "No runtime keys. Create one for the playground or use a bootstrap router key.");
    cell.colSpan = 5;
    row.append(cell);
    table.append(row);
  }
}

async function revokeKey(key) {
  if (!window.confirm(`Revoke “${key.label}”? Requests using it will fail immediately.`)) return;
  try {
    await api(`/api/keys/${encodeURIComponent(key.id)}`, {
      method: "DELETE",
      headers: { "x-haarness-ui": "1" },
    });
    toast("Runtime key revoked");
    await refreshKeys();
  } catch (error) {
    toast(error.message, "error");
  }
}

function initializeKeyManagement() {
  const dialog = $("#create-key-dialog");
  $("#close-key-dialog").addEventListener("click", () => dialog.close());
  $("#cancel-key-dialog").addEventListener("click", () => dialog.close());
  $("#create-key-button").addEventListener("click", () => {
    state.pendingKey = "";
    $("#new-key-label").value = "Local playground";
    $("#new-key-reveal").classList.add("hidden");
    $("#create-key-actions").classList.remove("hidden");
    dialog.showModal();
    $("#new-key-label").focus();
  });
  $("#refresh-keys").addEventListener("click", refreshKeys);
  $("#create-key-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    if (state.pendingKey) {
      dialog.close();
      return;
    }
    try {
      const data = await api("/api/keys", {
        method: "POST",
        headers: { "content-type": "application/json", "x-haarness-ui": "1" },
        body: JSON.stringify({ label: $("#new-key-label").value }),
      });
      state.pendingKey = data.secret;
      $("#new-key-secret").textContent = data.secret;
      $("#new-key-reveal").classList.remove("hidden");
      $("#create-key-actions").classList.add("hidden");
      await refreshKeys();
    } catch (error) {
      toast(error.message, "error");
    }
  });
  $("#copy-new-key").addEventListener("click", async () => {
    if (!state.pendingKey) return;
    setRouterKey(state.pendingKey);
    await copyText(state.pendingKey, "Key copied and selected for the playground");
    state.pendingKey = "";
    dialog.close();
    navigate("playground");
  });
}

function routerKey() {
  return sessionStorage.getItem("haarness.routerKey") || "";
}

function routerKeyRequired() {
  return state.overview?.security.router_auth_required !== false;
}

function setRouterKey(value) {
  if (value) sessionStorage.setItem("haarness.routerKey", value);
  else sessionStorage.removeItem("haarness.routerKey");
  $("#settings-router-key").value = value;
  $("#playground-key-state").textContent = !routerKeyRequired() ? "No key required" : value ? "Session key ready" : "No router key";
  $("#playground-key-state").classList.toggle("ready", !routerKeyRequired() || Boolean(value));
}

function initializeSettings() {
  setRouterKey(routerKey());
  $("#save-router-key").addEventListener("click", () => {
    const value = $("#settings-router-key").value.trim();
    if (!value) return toast("Enter a router key first", "error");
    setRouterKey(value);
    toast("Router key stored for this tab session");
  });
  $("#clear-router-key").addEventListener("click", () => {
    setRouterKey("");
    toast("Session router key forgotten");
  });
}

function renderSettings() {
  if (!state.overview) return;
  const security = state.overview.security;
  setRouterKey(routerKey());
  $("#router-key-help").textContent = routerKeyRequired()
    ? "This router requires an access key. The browser keeps it for this tab session."
    : "No access key is required. You can send requests immediately.";
  const values = [
    routerKeyRequired() ? "Required" : "Not required",
    security.basic_auth ? "Enabled" : "Disabled",
    security.loopback_bind ? "Yes" : "No",
    security.runtime_key_management ? "Enabled" : "Disabled",
    security.upstream_key_override ? "Enabled" : "Disabled",
  ];
  $$("#security-settings b").forEach((node, index) => { node.textContent = values[index]; });
  const endpoint = `${location.origin}/v1`;
  $("#endpoint-code").textContent = endpoint;
  const authFlag = routerKeyRequired() ? '-H "Authorization: Bearer <router-key>" ' : "";
  $("#curl-sample").textContent = `curl -N ${endpoint}/chat/completions ${authFlag}-H 'Content-Type: application/json' -d '{"model":"default","messages":[{"role":"user","content":"Hello"}],"stream":true}'`;
}

function resetResponse() {
  state.responseText = "";
  const body = $("#response-body");
  body.replaceChildren();
  const reasoning = element("div", "reasoning-block hidden");
  reasoning.id = "response-reasoning";
  const tools = element("div", "tool-block hidden");
  tools.id = "response-tools";
  const content = element("div", "response-content");
  content.id = "response-content";
  body.append(reasoning, tools, content);
  $("#usage-strip").replaceChildren(...["Input", "Output", "Total", "Finish"].map((name) => {
    const span = element("span", "", `${name} `);
    span.append(element("b", "", "—"));
    return span;
  }));
}

function parseOptionalJson(value, label) {
  const source = value.trim();
  if (!source) return undefined;
  try {
    return JSON.parse(source);
  } catch (error) {
    throw new Error(`${label} is not valid JSON: ${error.message}`);
  }
}

function buildPlaygroundRequest() {
  const message = $("#playground-message").value.trim();
  if (!message) throw new Error("Enter a user message");
  const content = [{ type: "text", text: message }];
  const attachment = $("#playground-attachment").value.trim();
  const kind = $("#playground-attachment-kind").value;
  if (attachment && kind === "image_url") content.push({ type: "image_url", image_url: { url: attachment } });
  if (attachment && kind === "file") content.push({ type: "file", file: { file_data: attachment } });
  if (attachment && kind === "video_url") content.push({ type: "video_url", video_url: { url: attachment } });
  if (attachment && kind.startsWith("input_audio_")) {
    const format = kind.slice("input_audio_".length);
    const data = attachment.includes(",") && attachment.startsWith("data:") ? attachment.slice(attachment.indexOf(",") + 1) : attachment;
    content.push({ type: "input_audio", input_audio: { data, format } });
  }
  const messages = [];
  const system = $("#playground-system").value.trim();
  if (system) messages.push({ role: "developer", content: system });
  messages.push({ role: "user", content: content.length === 1 ? message : content });
  const stream = $("#playground-stream").checked;
  const request = {
    model: $("#playground-model").value,
    messages,
    stream,
  };
  if (stream) request.stream_options = { include_usage: true };
  const tokens = Number($("#playground-max-tokens").value);
  if (tokens > 0) request.max_completion_tokens = tokens;
  const temperature = $("#playground-temperature").value;
  if (temperature !== "") request.temperature = Number(temperature);
  const reasoning = $("#playground-reasoning").value;
  if (reasoning) request.reasoning_effort = reasoning;
  const tools = parseOptionalJson($("#playground-tools").value, "Tools");
  if (tools) request.tools = tools;
  const schema = parseOptionalJson($("#playground-schema").value, "JSON Schema");
  if (schema) request.response_format = { type: "json_schema", json_schema: schema };
  return request;
}

function updateResponseMetadata(response) {
  const values = [
    response.headers.get("x-haarness-provider") || "—",
    response.headers.get("x-haarness-model") || "—",
    response.headers.get("x-haarness-attempts") || "—",
  ];
  $$("#response-meta b").forEach((node, index) => { node.textContent = values[index]; });
}

function updateUsage(usage, finishReason) {
  const values = [
    usage?.prompt_tokens ?? "—",
    usage?.completion_tokens ?? "—",
    usage?.total_tokens ?? "—",
    finishReason || "—",
  ];
  $$("#usage-strip b").forEach((node, index) => { node.textContent = values[index]; });
}

function appendChunk(payload, toolState) {
  if (payload.error) throw new Error(payload.error.message || payload.error.code || "Stream failed");
  if (payload.usage) updateUsage(payload.usage, toolState.finishReason);
  for (const choice of payload.choices || []) {
    const delta = choice.delta || {};
    if (typeof delta.content === "string") {
      state.responseText += delta.content;
      $("#response-content").textContent += delta.content;
    }
    const reasoning = delta.reasoning_content || delta.reasoning;
    if (typeof reasoning === "string") {
      const block = $("#response-reasoning");
      block.classList.remove("hidden");
      block.textContent += reasoning;
    }
    if (Array.isArray(delta.reasoning_details)) {
      const block = $("#response-reasoning");
      block.classList.remove("hidden");
      block.textContent += `${block.textContent ? "\n" : ""}[opaque reasoning detail preserved]`;
    }
    for (const call of delta.tool_calls || []) {
      const index = call.index || 0;
      const existing = toolState.calls.get(index) || { id: "", name: "", arguments: "", type: call.type || "function" };
      existing.id = call.id || existing.id;
      const source = call.function || call.custom || {};
      existing.name = source.name || existing.name;
      existing.arguments += source.arguments || source.input || "";
      toolState.calls.set(index, existing);
      const block = $("#response-tools");
      block.classList.remove("hidden");
      block.textContent = JSON.stringify([...toolState.calls.values()], null, 2);
    }
    if (choice.finish_reason) {
      toolState.finishReason = choice.finish_reason;
      updateUsage(payload.usage, choice.finish_reason);
    }
  }
  $("#response-body").scrollTop = $("#response-body").scrollHeight;
}

async function readSse(response) {
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  const toolState = { calls: new Map(), finishReason: "" };
  let buffer = "";
  while (true) {
    const { value, done } = await reader.read();
    buffer += decoder.decode(value || new Uint8Array(), { stream: !done });
    let boundary;
    while ((boundary = buffer.search(/\r?\n\r?\n/)) >= 0) {
      const frame = buffer.slice(0, boundary);
      const delimiter = buffer.slice(boundary).match(/^\r?\n\r?\n/)[0];
      buffer = buffer.slice(boundary + delimiter.length);
      const data = frame.split(/\r?\n/).filter((line) => line.startsWith("data:")).map((line) => line.slice(5).trimStart()).join("\n");
      if (!data) continue;
      if (data === "[DONE]") return;
      appendChunk(JSON.parse(data), toolState);
    }
    if (done) break;
  }
}

function renderNonStreaming(payload) {
  if (payload.error) throw new Error(payload.error.message || payload.error.code || "Request failed");
  const message = payload.choices?.[0]?.message || {};
  const content = typeof message.content === "string" ? message.content : JSON.stringify(message.content ?? "", null, 2);
  state.responseText = content;
  $("#response-content").textContent = content;
  const reasoning = message.reasoning_content || message.reasoning;
  if (reasoning || message.reasoning_details?.length) {
    const block = $("#response-reasoning");
    block.classList.remove("hidden");
    block.textContent = reasoning || "[opaque reasoning details preserved]";
  }
  if (message.tool_calls?.length) {
    const block = $("#response-tools");
    block.classList.remove("hidden");
    block.textContent = JSON.stringify(message.tool_calls, null, 2);
  }
  updateUsage(payload.usage, payload.choices?.[0]?.finish_reason);
}

async function sendPlaygroundRequest() {
  const key = routerKey();
  if (routerKeyRequired() && !key) {
    toast("Create or enter a router key before sending", "error");
    navigate("keys");
    return;
  }
  let request;
  try {
    request = buildPlaygroundRequest();
  } catch (error) {
    toast(error.message, "error");
    return;
  }
  resetResponse();
  state.controller?.abort();
  state.controller = new AbortController();
  const headers = {
    "content-type": "application/json",
  };
  if (routerKeyRequired() && key) headers.authorization = `Bearer ${key}`;
  const upstreamKey = $("#playground-upstream-key").value;
  if (upstreamKey) headers["x-upstream-api-key"] = upstreamKey;
  $("#playground-send").disabled = true;
  try {
    const response = await fetch("/v1/chat/completions", {
      method: "POST",
      headers,
      body: JSON.stringify(request),
      signal: state.controller.signal,
      cache: "no-store",
    });
    updateResponseMetadata(response);
    if (!response.ok) {
      const error = await response.json().catch(() => null);
      throw new Error(error?.error?.message || `Router returned HTTP ${response.status}`);
    }
    if (request.stream) await readSse(response);
    else renderNonStreaming(await response.json());
    toast("Request completed");
    refreshMetrics();
  } catch (error) {
    if (error.name === "AbortError") toast("Request cancelled");
    else {
      const block = element("div", "error-block", error.message);
      $("#response-body").prepend(block);
      toast(error.message, "error");
    }
  } finally {
    $("#playground-send").disabled = false;
    state.controller = null;
  }
}

function initializePlayground() {
  $("#playground-send").addEventListener("click", sendPlaygroundRequest);
  $("#stop-response").addEventListener("click", () => state.controller?.abort());
  $("#playground-clear").addEventListener("click", () => {
    $("#playground-message").value = "";
    $("#playground-system").value = "";
    $("#playground-tools").value = "";
    $("#playground-schema").value = "";
    $("#playground-attachment").value = "";
    $("#playground-upstream-key").value = "";
    resetResponse();
  });
  $("#copy-response").addEventListener("click", () => copyText(state.responseText, "Response copied"));
  $("#playground-message").addEventListener("keydown", (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key === "Enter") sendPlaygroundRequest();
  });
}

function initializeUtilityActions() {
  $("#model-search").addEventListener("input", (event) => renderModels(event.target.value));
  $("#refresh-metrics").addEventListener("click", refreshMetrics);
  $("#copy-metrics").addEventListener("click", () => copyText(state.rawMetrics, "Metrics copied"));
  $("#copy-endpoint").addEventListener("click", () => copyText(`${location.origin}/v1`, "Endpoint copied"));
}

async function start() {
  initializeTheme();
  initializeNavigation();
  initializeSettings();
  initializeKeyManagement();
  initializePlayground();
  initializeUtilityActions();
  resetResponse();
  await Promise.all([loadOverview(), refreshMetrics()]);
  window.setInterval(refreshMetrics, 5000);
  window.setInterval(loadOverview, 30000);
}

start();
