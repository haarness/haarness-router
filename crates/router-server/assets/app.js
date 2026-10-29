"use strict";

// Haarness Router dashboard. Plain DOM, no dependencies: the router embeds this file and serves it with a
// strict Content-Security-Policy, so everything here is same-origin and nothing is inlined into the page.
(() => {
  const root = document.documentElement;
  const BASE = root.dataset.uiBase || "/ui";
  const SVG = "http://www.w3.org/2000/svg";
  const THEME_KEY = "haarness.theme";
  const ROUTER_KEY = "haarness.routerKey";
  const ROUTER_KEY_ID = "haarness.routerKeyId";
  const MODEL_KEY = "haarness.router.model";
  const CONNECT_KEY = "haarness.router.connect";
  const PLAYGROUND_KEY = "haarness.router.playground";
  const SAMPLE_MS = 5000;
  const OVERVIEW_MS = 15000;
  const WINDOW_MS = 5 * 60 * 1000;
  const NARROW = window.matchMedia("(max-width: 860px)");
  const VIEWS = {
    overview: "Overview",
    playground: "Playground",
    models: "Models & routes",
    keys: "API keys",
    observability: "Observability",
    settings: "Settings",
  };

  // Storage can be missing or throw (private windows, blocked site data); every use goes through here.
  const storage = (area) => ({
    get(key) {
      try {
        return area().getItem(key);
      } catch {
        return null;
      }
    },
    set(key, value) {
      try {
        if (value === null || value === undefined || value === "") area().removeItem(key);
        else area().setItem(key, value);
      } catch {
        // Not saved; the page still works for this visit.
      }
    },
  });
  const local = storage(() => window.localStorage);
  const session = storage(() => window.sessionStorage);

  function applyTheme(choice) {
    if (choice === "light" || choice === "dark") root.dataset.theme = choice;
    else delete root.dataset.theme;
  }
  // Before the first paint, so a saved choice never flashes the other appearance.
  applyTheme(local.get(THEME_KEY));

  const state = {
    view: "",
    overview: null,
    ready: null,
    reachable: true,
    catalog: null,
    catalogStatus: "idle",
    catalogAt: 0,
    metrics: null,
    metricsRaw: "",
    metricsStatus: "idle",
    metricsAt: 0,
    samples: [],
    keys: null,
    keysSignature: "",
    pendingRevoke: "",
    showIdleRoutes: false,
    model: local.get(MODEL_KEY) || "",
    options: loadOptions(),
    attachments: [],
    turns: [],
    running: null,
    activity: [],
    pickerActive: -1,
    pickerItems: [],
    timers: {},
  };

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start, { once: true });
  else start();

  // Helpers -------------------------------------------------------------------------------------------------
  function byId(id) {
    return document.getElementById(id);
  }

  function el(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = text;
    return node;
  }

  function icon(name, extra) {
    const svg = document.createElementNS(SVG, "svg");
    svg.setAttribute("class", extra ? `icon ${extra}` : "icon");
    svg.setAttribute("aria-hidden", "true");
    const use = document.createElementNS(SVG, "use");
    use.setAttribute("href", `#i-${name}`);
    svg.append(use);
    return svg;
  }

  function button(label, className, onClick) {
    const node = el("button", className, label);
    node.type = "button";
    if (onClick) node.addEventListener("click", onClick);
    return node;
  }

  function iconButton(name, label, onClick) {
    const node = el("button", "icon-button");
    node.type = "button";
    node.setAttribute("aria-label", label);
    node.title = label;
    node.append(icon(name));
    if (onClick) node.addEventListener("click", onClick);
    return node;
  }

  function setText(id, value) {
    const node = byId(id);
    if (node && node.textContent !== value) node.textContent = value;
  }

  const numberFormat = new Intl.NumberFormat("en");
  function count(value) {
    return numberFormat.format(Math.round(Number(value) || 0));
  }

  function compact(value) {
    const number = Number(value) || 0;
    if (Math.abs(number) < 10_000) return count(number);
    if (Math.abs(number) < 1_000_000) return `${(number / 1000).toFixed(number < 100_000 ? 1 : 0)}k`;
    return `${(number / 1_000_000).toFixed(number < 10_000_000 ? 2 : 1)}M`;
  }

  // [number, unit] so a figure can set its unit smaller than the number.
  function duration(milliseconds) {
    if (milliseconds === null || milliseconds === undefined || !Number.isFinite(milliseconds)) return ["—", ""];
    if (milliseconds < 1000) return [String(Math.max(0, Math.round(milliseconds))), "ms"];
    const seconds = milliseconds / 1000;
    if (seconds < 60) return [seconds.toFixed(seconds < 10 ? 2 : 1), "s"];
    return [(seconds / 60).toFixed(1), "min"];
  }

  function durationText(milliseconds) {
    const [value, unit] = duration(milliseconds);
    return unit ? `${value} ${unit}` : value;
  }

  function bytes(value) {
    const number = Number(value) || 0;
    if (number < 1024) return [count(number), "B"];
    const units = ["KB", "MB", "GB", "TB"];
    let scaled = number / 1024;
    let unit = 0;
    while (scaled >= 1024 && unit < units.length - 1) {
      scaled /= 1024;
      unit += 1;
    }
    return [scaled.toFixed(scaled < 10 ? 1 : 0), units[unit]];
  }

  function percent(ratio) {
    if (!Number.isFinite(ratio)) return ["—", ""];
    const value = ratio * 100;
    if (value === 0) return ["0", "%"];
    if (value < 1) return [value.toFixed(1), "%"];
    return [value.toFixed(value < 10 ? 1 : 0), "%"];
  }

  function since(seconds) {
    const value = Math.max(0, Math.floor(seconds));
    if (value < 60) return `${value}s`;
    if (value < 3600) return `${Math.floor(value / 60)}m`;
    if (value < 86_400) return `${Math.floor(value / 3600)}h ${Math.floor((value % 3600) / 60)}m`;
    return `${Math.floor(value / 86_400)}d ${Math.floor((value % 86_400) / 3600)}h`;
  }

  function relative(seconds) {
    if (!seconds) return "Never";
    const elapsed = Date.now() / 1000 - Number(seconds);
    if (elapsed < 45) return "Just now";
    if (elapsed < 3600) return `${Math.max(1, Math.round(elapsed / 60))} min ago`;
    if (elapsed < 86_400) return `${Math.round(elapsed / 3600)} h ago`;
    return new Intl.DateTimeFormat(undefined, { dateStyle: "medium" }).format(new Date(Number(seconds) * 1000));
  }

  function absolute(seconds) {
    return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "medium" }).format(new Date(Number(seconds) * 1000));
  }

  function clockTime(date) {
    return new Intl.DateTimeFormat(undefined, { timeStyle: "medium" }).format(date);
  }

  function plural(number, one, many) {
    return `${count(number)} ${number === 1 ? one : many}`;
  }

  function sentence(text) {
    const value = String(text || "").trim();
    if (!value) return "";
    const capital = value.charAt(0).toUpperCase() + value.slice(1);
    return /[.!?]$/.test(capital) ? capital : `${capital}.`;
  }

  function setFigure(node, [value, unit]) {
    if (!node) return;
    const signature = `${value}|${unit}`;
    if (node.dataset.value === signature) return;
    node.dataset.value = signature;
    node.replaceChildren(document.createTextNode(value));
    if (unit) node.append(el("span", "stat-unit", unit));
  }

  function toast(message, kind) {
    const region = byId("toasts");
    const item = el("div", kind === "error" ? "toast error" : "toast", message);
    item.setAttribute("role", kind === "error" ? "alert" : "status");
    region.append(item);
    while (region.children.length > 3) region.firstElementChild.remove();
    window.setTimeout(() => {
      item.classList.add("leaving");
      window.setTimeout(() => item.remove(), 200);
    }, kind === "error" ? 5200 : 2600);
  }

  async function writeClipboard(text) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // A plain-HTTP origin has no async clipboard; the older path still works there.
      const area = el("textarea");
      area.value = text;
      area.setAttribute("readonly", "");
      area.className = "sr-only";
      document.body.append(area);
      area.select();
      let copied = false;
      try {
        copied = document.execCommand("copy");
      } catch {
        copied = false;
      }
      area.remove();
      return copied;
    }
  }

  async function copy(text, trigger) {
    const copied = await writeClipboard(text);
    if (!copied) {
      toast("The browser didn’t allow copying. Select the text instead.", "error");
      return;
    }
    const use = trigger?.querySelector("use");
    if (trigger?.classList.contains("icon-button") && use) {
      use.setAttribute("href", "#i-check");
      trigger.classList.add("copied");
      window.clearTimeout(trigger.copyTimer);
      trigger.copyTimer = window.setTimeout(() => {
        use.setAttribute("href", "#i-copy");
        trigger.classList.remove("copied");
      }, 1400);
    } else {
      toast("Copied");
    }
  }

  function schedule(name, callback, delay) {
    window.clearTimeout(state.timers[name]);
    state.timers[name] = window.setTimeout(callback, delay);
  }

  // Requests ------------------------------------------------------------------------------------------------
  class RequestError extends Error {
    constructor(message, status, code) {
      super(message);
      this.status = status;
      this.code = code;
    }
  }

  async function readPayload(response) {
    const type = response.headers.get("content-type") || "";
    if (type.includes("json")) return response.json().catch(() => null);
    return response.text().catch(() => "");
  }

  function failure(status, payload) {
    const body = payload && typeof payload === "object" ? payload.error || {} : {};
    const raw = typeof body.message === "string" ? body.message : typeof payload === "string" ? payload.trim().slice(0, 300) : "";
    return new RequestError(raw || `HTTP ${status}`, status, body.code || "");
  }

  async function uiApi(path, options = {}) {
    let response;
    try {
      response = await fetch(`${BASE}${path}`, { credentials: "same-origin", cache: "no-store", ...options });
    } catch {
      throw new RequestError("The router isn’t reachable.", 0, "network");
    }
    const payload = await readPayload(response);
    if (!response.ok) throw failure(response.status, payload);
    return payload;
  }

  function routerKey() {
    return session.get(ROUTER_KEY) || "";
  }

  function keyRequired() {
    return state.overview?.security?.router_auth_required === true;
  }

  function canCallApi() {
    return Boolean(state.overview) && (!keyRequired() || Boolean(routerKey()));
  }

  function apiHeaders(extra = {}) {
    const headers = { ...extra };
    if (keyRequired() && routerKey()) headers.authorization = `Bearer ${routerKey()}`;
    return headers;
  }

  function origin() {
    return window.location.origin;
  }

  // Theme ---------------------------------------------------------------------------------------------------
  function initTheme() {
    const buttons = [...document.querySelectorAll("[data-theme-choice]")];
    const sync = () => {
      const current = root.dataset.theme || "system";
      for (const item of buttons) item.setAttribute("aria-checked", String(item.dataset.themeChoice === current));
    };
    for (const item of buttons) {
      item.addEventListener("click", () => {
        const choice = item.dataset.themeChoice;
        local.set(THEME_KEY, choice === "system" ? null : choice);
        applyTheme(choice);
        sync();
      });
    }
    sync();
  }

  // Navigation ----------------------------------------------------------------------------------------------
  function viewFromHash() {
    const name = window.location.hash.slice(1);
    return Object.hasOwn(VIEWS, name) ? name : "overview";
  }

  function showView(view) {
    const changed = state.view !== view;
    state.view = view;
    for (const section of document.querySelectorAll("section.view")) section.hidden = section.dataset.view !== view;
    for (const link of document.querySelectorAll(".nav-link")) {
      if (link.dataset.view === view) link.setAttribute("aria-current", "page");
      else link.removeAttribute("aria-current");
    }
    byId("main").classList.toggle("is-playground", view === "playground");
    document.title = view === "overview" ? "Haarness Router" : `${VIEWS[view]} · Haarness Router`;
    closeNav();
    if (changed) window.scrollTo(0, 0);
    if (view === "keys") loadKeys();
    if (view === "playground" || view === "models") ensureCatalog();
    if (view === "observability") renderObservability();
    if (view === "overview") renderActivity();
    if (view === "playground") autosize();
    if (view === "playground" && !NARROW.matches && !state.running) byId("prompt").focus({ preventScroll: true });
  }

  function openNav() {
    byId("shell").classList.add("nav-open");
    byId("scrim").hidden = false;
    byId("nav-open").setAttribute("aria-expanded", "true");
    document.querySelector(".sidebar .nav-link[aria-current]")?.focus();
  }

  function closeNav() {
    if (!byId("shell").classList.contains("nav-open")) return;
    byId("shell").classList.remove("nav-open");
    byId("scrim").hidden = true;
    byId("nav-open").setAttribute("aria-expanded", "false");
  }

  function initNavigation() {
    window.addEventListener("hashchange", () => showView(viewFromHash()));
    byId("nav-open").addEventListener("click", openNav);
    byId("scrim").addEventListener("click", closeNav);
    document.addEventListener("keydown", (event) => {
      if (event.key !== "Escape") return;
      if (byId("shell").classList.contains("nav-open")) closeNav();
    });
    // Clicking the current view's link still closes the drawer on a phone.
    for (const link of document.querySelectorAll(".nav-link, .brand")) link.addEventListener("click", closeNav);
    showView(viewFromHash());
  }

  // Overview data -------------------------------------------------------------------------------------------
  async function loadOverview() {
    try {
      const [overview, ready] = await Promise.all([uiApi("/api/overview"), checkReady()]);
      const keyModeChanged = state.overview && state.overview.security?.router_auth_required !== overview.security?.router_auth_required;
      state.overview = overview;
      state.ready = ready;
      setReachable(true);
      renderAll();
      if (keyModeChanged) refreshAccess();
      else if (state.view === "keys") loadKeys();
      if (!state.catalogAt || Date.now() - state.catalogAt > WINDOW_MS) ensureCatalog(true);
    } catch (error) {
      state.ready = false;
      if (!error.status) setReachable(false);
      else if (!state.overview) toast(sentence(error.message), "error");
      renderReady();
    } finally {
      schedule("overview", loadOverview, state.reachable ? OVERVIEW_MS : SAMPLE_MS);
    }
  }

  async function checkReady() {
    try {
      const response = await fetch("/readyz", { cache: "no-store", credentials: "same-origin" });
      return response.ok;
    } catch {
      return false;
    }
  }

  function setReachable(reachable) {
    if (state.reachable === reachable) return;
    state.reachable = reachable;
    byId("offline-banner").hidden = reachable;
    if (reachable) {
      loadMetrics();
      if (state.view === "keys") loadKeys();
    }
  }

  function renderAll() {
    renderReady();
    renderEndpoint();
    renderProviders();
    renderConnect();
    renderPicker();
    renderModels();
    renderPlaygroundState();
    renderSettings();
    renderKeysChrome();
    renderActivityNote();
    const version = state.overview?.version;
    setText("sidebar-version", version ? `v${version}` : "");
  }

  function renderReady() {
    const badge = byId("ready-badge");
    let label = "Checking";
    let kind = "badge pending";
    if (!state.reachable) {
      label = "Unreachable";
      kind = "badge bad";
    } else if (state.ready === true) {
      label = "Ready";
      kind = "badge ok";
    } else if (state.ready === false) {
      label = "Not ready";
      kind = "badge bad";
    }
    if (badge.className !== kind) badge.className = kind;
    setText("ready-badge", label);
  }

  function renderEndpoint() {
    setText("endpoint-url", `${origin()}/v1`);
    if (!state.overview) return;
    setText("endpoint-note", keyRequired() ? "Clients send a router key with each request." : "Clients need no key.");
  }

  function renderActivityNote() {
    const started = state.overview?.started_at;
    setText("activity-note", started ? `Since the router started ${since(Date.now() / 1000 - started)} ago` : "\u00a0");
  }

  const KIND_NAMES = {
    openai: "OpenAI",
    anthropic: "Anthropic",
    gemini: "Google Gemini",
    groq: "Groq",
    mistral: "Mistral",
    xai: "xAI",
    deepseek: "DeepSeek",
    together: "Together AI",
    fireworks: "Fireworks AI",
    "openai-compatible": "OpenAI-compatible",
    "amazon-bedrock": "Amazon Bedrock",
  };

  function kindName(kind) {
    return KIND_NAMES[kind] || String(kind || "").replaceAll("-", " ");
  }

  function sameName(name, kind) {
    const squash = (value) => String(value || "").toLowerCase().replace(/[^a-z0-9]/g, "");
    const label = squash(kindName(kind));
    return label === squash(name) || label.endsWith(squash(name));
  }

  function providerLabel(provider) {
    return sameName(provider.name, provider.kind) ? kindName(provider.kind) : `${provider.name} · ${kindName(provider.kind)}`;
  }

  function host(url) {
    try {
      return new URL(url).host;
    } catch {
      return url || "";
    }
  }

  // `config` (where the configuration came from, and the key variables a router started without a file
  // looks for) is absent from older routers; the page then falls back to the common names.
  const DEFAULT_KEY_VARIABLES = ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY"].map((name) => ({ name, alternatives: [] }));

  function keyVariables(data) {
    const listed = Array.isArray(data?.config?.key_variables) ? data.config.key_variables : [];
    const variables = listed
      .map((entry) => {
        const names = (Array.isArray(entry?.variables) ? entry.variables : []).filter((name) => typeof name === "string" && name);
        return names.length ? { name: names[0], alternatives: names.slice(1), provider: entry.provider } : null;
      })
      .filter(Boolean);
    return variables.length ? variables : DEFAULT_KEY_VARIABLES;
  }

  function configSource(data) {
    const kind = data?.config?.source;
    if (!["file", "environment", "stdin"].includes(kind)) return null;
    return { kind, path: typeof data.config.path === "string" ? data.config.path : "" };
  }

  function circuitStates() {
    return new Map((state.overview?.circuits || []).map((circuit) => [circuit.target, circuit.state]));
  }

  function renderProviders() {
    const list = byId("provider-list");
    const data = state.overview;
    if (!data) return;
    const providers = data.providers || [];
    const { models, aliases } = catalogEntries();
    setText("catalog-counts", `${plural(models.length, "model", "models")} · ${plural(aliases.length, "alias", "aliases")}`);
    const signature = JSON.stringify([providers, data.circuits, models.map((model) => model.id), configSource(data), keyVariables(data)]);
    if (list.dataset.signature === signature) return;
    list.dataset.signature = signature;

    if (!providers.length) {
      list.replaceChildren(providerEmptyState(data));
      return;
    }
    const circuits = circuitStates();
    const rows = providers.map((provider) => {
      const row = el("div", "provider");
      const name = el("div", "provider-name");
      name.append(el("strong", "", provider.name));
      if (!sameName(provider.name, provider.kind)) name.append(el("span", "provider-kind", kindName(provider.kind)));
      const owned = models.filter((model) => model.provider === provider.name).length;
      const meta = el("span", "provider-meta", `${provider.credential_configured ? "Key set" : "Caller’s key only"} · ${plural(owned, "model", "models")}`);
      if (!provider.credential_configured) meta.title = "Requests need a provider key in X-Upstream-API-Key";
      const targets = [...circuits.entries()].filter(([target]) => target.startsWith(`${provider.name}/`));
      const open = targets.filter(([, value]) => value === "open").length;
      const half = targets.filter(([, value]) => value === "half-open").length;
      let badge = el("span", "badge ok", "Healthy");
      if (open) {
        badge = el("span", "badge bad", open === targets.length ? "Circuit open" : `${open} of ${targets.length} open`);
        badge.title = "The router stopped sending to these targets after repeated failures and retries them shortly.";
      } else if (half) {
        badge = el("span", "badge warn", "Recovering");
        badge.title = "The router is letting a trial request through after failures.";
      }
      row.append(name, meta, badge);
      return row;
    });
    list.replaceChildren(...rows);
  }

  function providerEmptyState(data) {
    const box = el("div", "empty");
    box.append(el("strong", "", "No providers yet"));
    const source = configSource(data);
    const variables = keyVariables(data);
    const envList = el("ul", "env-list");
    for (const variable of variables) {
      const item = el("li");
      const code = el("code", "", variable.name);
      if (variable.alternatives.length) code.title = `Or ${variable.alternatives.join(" or ")}`;
      item.append(code);
      envList.append(item);
    }
    if (source && source.kind !== "environment") {
      box.append(
        el("span", "", "This configuration lists no providers. Add one to it, then restart the router."),
        el("span", "", "Started without a configuration file, the router adds a provider for each of these it finds in its environment:"),
        envList,
      );
    } else {
      box.append(el("span", "", "Set one of these in the router’s environment, then restart the router:"), envList);
    }
    return box;
  }

  // Connect card --------------------------------------------------------------------------------------------
  function defaultModel() {
    const aliases = Object.keys(state.overview?.aliases || {});
    if (aliases.includes("default")) return "default";
    if (aliases.length) return aliases[0];
    const first = catalogEntries().models[0];
    return first ? first.id : "default";
  }

  function connectSamples() {
    const base = origin();
    const required = keyRequired();
    const model = defaultModel();
    const body = JSON.stringify({ model, messages: [{ role: "user", content: "Hello" }] });
    return {
      haarness: {
        code: required
          ? `printf '%s' "$HAARNESS_ROUTER_KEY" | haarness provider connect-router ${base} --key-stdin`
          : `haarness provider connect-router ${base}`,
        hint: required
          ? "Adds this router to Haarness as a model provider, reading its key from standard input."
          : "Adds this router to Haarness as a model provider, with its aliases in the model picker.",
      },
      python: {
        code: required
          ? `import os\nfrom openai import OpenAI\n\nclient = OpenAI(base_url="${base}/v1", api_key=os.environ["HAARNESS_ROUTER_KEY"])`
          : `from openai import OpenAI\n\nclient = OpenAI(base_url="${base}/v1", api_key="unused")`,
        hint: required
          ? "Any OpenAI-compatible SDK takes the same base URL and a router key."
          : "Any OpenAI-compatible SDK takes the same base URL. The SDK wants a key; this router ignores it.",
      },
      curl: {
        code: [
          `curl ${base}/v1/chat/completions \\`,
          ...(required ? ['  -H "Authorization: Bearer $HAARNESS_ROUTER_KEY" \\'] : []),
          '  -H "Content-Type: application/json" \\',
          `  -d '${body}'`,
        ].join("\n"),
        hint: `Sends one request to ${model}.`,
      },
    };
  }

  function renderConnect() {
    const choice = ["haarness", "python", "curl"].includes(local.get(CONNECT_KEY)) ? local.get(CONNECT_KEY) : "haarness";
    for (const tab of document.querySelectorAll("[data-connect]")) {
      tab.setAttribute("aria-selected", String(tab.dataset.connect === choice));
      tab.tabIndex = tab.dataset.connect === choice ? 0 : -1;
    }
    const sample = connectSamples()[choice];
    setText("connect-code", sample.code);
    setText("connect-hint", sample.hint);
  }

  function initConnect() {
    const tabs = [...document.querySelectorAll("[data-connect]")];
    for (const tab of tabs) {
      tab.addEventListener("click", () => {
        local.set(CONNECT_KEY, tab.dataset.connect);
        renderConnect();
      });
      tab.addEventListener("keydown", (event) => {
        if (event.key !== "ArrowRight" && event.key !== "ArrowLeft") return;
        const next = tabs[(tabs.indexOf(tab) + (event.key === "ArrowRight" ? 1 : tabs.length - 1)) % tabs.length];
        next.click();
        next.focus();
      });
    }
    renderConnect();
  }

  // Model catalog -------------------------------------------------------------------------------------------
  async function ensureCatalog(force) {
    if (!canCallApi()) {
      if (state.overview) state.catalogStatus = keyRequired() ? "needs-key" : "idle";
      return;
    }
    if (state.catalogStatus === "loading") return;
    if (keyRequired() && state.refusedKey === routerKey()) {
      state.catalogStatus = "rejected";
      return;
    }
    if (!force && state.catalogAt && Date.now() - state.catalogAt < WINDOW_MS) return;
    state.catalogStatus = "loading";
    try {
      const response = await fetch("/v1/models", { cache: "no-store", credentials: "same-origin", headers: apiHeaders() });
      if (response.status === 401) {
        state.refusedKey = routerKey();
        state.catalogStatus = "rejected";
        return;
      }
      if (!response.ok) throw new Error(String(response.status));
      const body = await response.json();
      state.catalog = Array.isArray(body?.data) ? body.data : [];
      state.catalogStatus = "ok";
      state.catalogAt = Date.now();
    } catch {
      state.catalogStatus = "error";
    } finally {
      if (state.catalogStatus === "loading") state.catalogStatus = "error";
      renderProviders();
      renderPicker();
      renderModels();
      renderOptions();
    }
  }

  function metadataOf(item) {
    const meta = {};
    for (const [key, value] of Object.entries(item || {})) {
      if (!["id", "object", "created", "owned_by"].includes(key)) meta[key] = value;
    }
    return meta;
  }

  // Declared models with their capabilities, then the models providers report (the overview's
  // discovered_models, and /v1/models, which also carries metadata for declared ones). Aliases come from the
  // configuration.
  function catalogEntries() {
    const data = state.overview;
    const aliasMap = data?.aliases || {};
    const aliases = Object.entries(aliasMap).map(([id, definition]) => ({ id, targets: definition?.targets || [] }));
    const models = new Map();
    for (const model of data?.models || []) {
      models.set(model.id, { id: model.id, provider: model.provider, capabilities: model.capabilities, declared: true, meta: {} });
    }
    const reported = [...(Array.isArray(data?.discovered_models) ? data.discovered_models : []), ...(state.catalog || [])];
    for (const item of reported) {
      if (!item?.id || Object.hasOwn(aliasMap, item.id) || item.owned_by === "haarness-router") continue;
      const provider = item.owned_by || item.id.split("/")[0];
      const existing = models.get(item.id);
      if (existing) existing.meta = { ...existing.meta, ...metadataOf(item) };
      else models.set(item.id, { id: item.id, provider, capabilities: null, declared: false, meta: metadataOf(item) });
    }
    const ordered = [...models.values()].sort((left, right) => Number(right.declared) - Number(left.declared) || left.id.localeCompare(right.id));
    return { aliases, models: ordered };
  }

  function providerOf(name) {
    return (state.overview?.providers || []).find((provider) => provider.name === name);
  }

  function capabilitiesOf(model) {
    const caps = { ...(model.capabilities || providerOf(model.provider)?.capabilities || {}) };
    const meta = model.meta || {};
    if (typeof meta.supports_vision === "boolean") caps.images = meta.supports_vision;
    if (typeof meta.supports_tools === "boolean") caps.function_tools = meta.supports_tools;
    if (typeof meta.supports_reasoning === "boolean") caps.reasoning = meta.supports_reasoning;
    return caps;
  }

  const MAIN_CAPABILITIES = [
    ["images", "Images"],
    ["pdf", "PDF"],
    ["input_audio", "Audio"],
    ["video", "Video"],
    ["function_tools", "Tools"],
    ["structured_outputs", "Structured output"],
    ["reasoning", "Reasoning"],
  ];
  const OTHER_CAPABILITIES = {
    text: "text",
    streaming: "streaming",
    pdf_text_extraction: "PDF text extraction",
    custom_tools: "custom tools",
    parallel_tools: "parallel tool calls",
    opaque_reasoning: "encrypted reasoning",
    logprobs: "log probabilities",
    seed: "seed",
    multiple_choices: "several choices",
    audio_output: "audio output",
  };

  function capabilityWords(caps) {
    return Object.entries(caps)
      .filter(([, enabled]) => enabled)
      .map(([name]) => MAIN_CAPABILITIES.find(([key]) => key === name)?.[1] || OTHER_CAPABILITIES[name] || name.replaceAll("_", " "));
  }

  function contextNote(meta) {
    const parts = [];
    if (typeof meta.display_name === "string" && meta.display_name) parts.push(meta.display_name);
    if (Number(meta.context_window) > 0) parts.push(`${compactTokens(meta.context_window)} context`);
    return parts.join(" · ");
  }

  function compactTokens(value) {
    const number = Number(value);
    if (number >= 1_000_000) return `${Number((number / 1_000_000).toFixed(number % 1_000_000 ? 1 : 0))}M`;
    if (number >= 1000) return `${Math.round(number / 1000)}k`;
    return String(number);
  }

  // Models & routes -----------------------------------------------------------------------------------------
  function renderModels() {
    if (!state.overview) return;
    const query = byId("model-search").value.trim().toLowerCase();
    const { aliases, models } = catalogEntries();
    const circuits = circuitStates();
    // The overview refreshes every few seconds; rebuild only what changed, so focus and hover stay put.
    const signature = JSON.stringify([query, aliases, models, state.overview.providers, [...circuits], state.catalogStatus, keyRequired() && !routerKey()]);
    if (byId("model-groups").dataset.signature === signature) return;
    byId("model-groups").dataset.signature = signature;
    const matches = (...values) => !query || values.join(" ").toLowerCase().includes(query);

    const aliasList = byId("alias-list");
    const shownAliases = aliases.filter((alias) => matches(alias.id, ...alias.targets));
    byId("alias-block").hidden = !aliases.length;
    aliasList.replaceChildren(
      ...shownAliases.map((alias) => {
        const row = el("div", "row");
        const main = el("div", "row-main");
        const title = el("div", "row-title");
        title.append(el("code", "", alias.id), el("span", "row-note", plural(alias.targets.length, "target", "targets")));
        const chain = el("ol", "chain");
        for (const target of alias.targets) {
          const item = el("li");
          const chip = el("span", "target");
          const circuit = circuits.get(target);
          const dot = el("span", `dot ${circuit === "open" ? "open" : circuit === "half-open" ? "half-open" : ""}`);
          dot.title = circuit === "open" ? "Circuit open" : circuit === "half-open" ? "Recovering" : "Healthy";
          chip.append(dot, document.createTextNode(target));
          item.append(chip);
          chain.append(item);
        }
        main.append(title, chain);
        row.append(main, tryButton(alias.id));
        return row;
      }),
    );
    if (aliases.length && !shownAliases.length) aliasList.replaceChildren(emptyRow("No alias matches."));

    const groups = byId("model-groups");
    const providers = state.overview.providers || [];
    if (!providers.length) {
      const empty = el("div", "card empty center");
      empty.append(el("strong", "", "No models yet"), el("span", "", "Models appear here once the router has a provider."));
      groups.replaceChildren(empty);
      return;
    }
    const nodes = [];
    if (keyRequired() && !routerKey() && !Array.isArray(state.overview.discovered_models)) {
      nodes.push(el("p", "block-text", "Models the providers report appear here once this tab has a router key."));
    } else if (state.catalogStatus === "rejected") {
      nodes.push(el("p", "block-text", "The router didn’t accept this tab’s key, so only configured models are listed."));
    }
    let shown = 0;
    for (const provider of providers) {
      const owned = models.filter((model) => model.provider === provider.name);
      const visible = owned.filter((model) => matches(model.id, provider.kind, kindName(provider.kind), contextNote(model.meta), ...capabilityWords(capabilitiesOf(model))));
      if (query && !visible.length) continue;
      const card = el("article", "card list group");
      const head = el("header", "group-head");
      head.append(el("strong", "", provider.name));
      if (!sameName(provider.name, provider.kind)) head.append(el("span", "", kindName(provider.kind)));
      if (provider.base_url) head.append(el("code", "", host(provider.base_url)));
      card.append(head);
      if (!owned.length) {
        card.append(emptyRow(state.catalogStatus === "loading" ? "Asking the provider for its models…" : "No models declared. Any model this provider serves can be called as provider/model."));
      }
      for (const model of visible) {
        shown += 1;
        const row = el("div", "row");
        const main = el("div", "row-main");
        const title = el("div", "row-title");
        title.append(el("code", "", model.id));
        const note = contextNote(model.meta);
        if (note) title.append(el("span", "row-note", note));
        main.append(title);
        const caps = capabilitiesOf(model);
        const tags = el("div", "caps");
        for (const [key, label] of MAIN_CAPABILITIES) if (caps[key]) tags.append(el("span", "tag", label));
        if (caps.streaming === false) tags.append(el("span", "tag", "No streaming"));
        const others = Object.entries(caps)
          .filter(([key, enabled]) => enabled && Object.hasOwn(OTHER_CAPABILITIES, key) && key !== "text" && key !== "streaming")
          .map(([key]) => OTHER_CAPABILITIES[key]);
        if (others.length) {
          const more = el("span", "tag", `+${others.length}`);
          more.title = `Also: ${others.join(", ")}`;
          tags.append(more);
        }
        if (tags.children.length) main.append(tags);
        row.append(main, tryButton(model.id));
        card.append(row);
      }
      nodes.push(card);
    }
    if (query && !shown && !shownAliases.length) {
      const empty = el("div", "card empty center");
      empty.append(el("strong", "", "Nothing matches"), el("span", "", `No model or alias matches “${byId("model-search").value.trim()}”.`));
      nodes.push(empty);
    }
    groups.replaceChildren(...nodes);
  }

  function emptyRow(text) {
    const row = el("div", "row");
    row.append(el("span", "row-note", text));
    return row;
  }

  function tryButton(id) {
    const node = button("Try", "ghost try", () => {
      selectModel(id);
      window.location.hash = "#playground";
    });
    node.setAttribute("aria-label", `Try ${id} in the playground`);
    return node;
  }

  // Metrics -------------------------------------------------------------------------------------------------
  function parseMetrics(raw) {
    const families = new Map();
    for (const line of raw.split(/\r?\n/)) {
      if (!line || line.startsWith("#")) continue;
      const match = line.match(/^([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{(.*)\})?\s+(\S+)(?:\s+\S+)?$/);
      if (!match) continue;
      const labels = {};
      for (const label of (match[2] || "").matchAll(/([a-zA-Z_][a-zA-Z0-9_]*)="((?:\\.|[^"\\])*)"/g)) {
        labels[label[1]] = label[2].replace(/\\(.)/g, (_, character) => (character === "n" ? "\n" : character));
      }
      if (!families.has(match[1])) families.set(match[1], []);
      families.get(match[1]).push({ labels, value: Number(match[3]) });
    }
    return families;
  }

  function total(name) {
    return (state.metrics?.get(name) || []).reduce((sum, sample) => sum + (Number.isFinite(sample.value) ? sample.value : 0), 0);
  }

  async function loadMetrics(sample) {
    try {
      if (!state.overview) return;
      if (keyRequired() && !routerKey()) {
        setMetricsStatus("needs-key");
        return;
      }
      // A refused key stays refused; ask again only once the tab has another one.
      if (keyRequired() && state.refusedKey === routerKey()) return;
      let response;
      try {
        response = await fetch("/metrics", { cache: "no-store", credentials: "same-origin", headers: apiHeaders() });
      } catch {
        setReachable(false);
        return;
      }
      if (response.status === 401) {
        state.refusedKey = routerKey();
        setMetricsStatus("rejected");
        return;
      }
      if (!response.ok) {
        setMetricsStatus("error");
        return;
      }
      state.metricsRaw = await response.text();
      state.metrics = parseMetrics(state.metricsRaw);
      state.metricsAt = Date.now();
      if (sample) takeSample();
      setMetricsStatus("ok");
      renderActivity();
      if (state.view === "observability") renderObservability();
    } finally {
      if (sample) schedule("metrics", () => loadMetrics(true), SAMPLE_MS);
    }
  }

  function takeSample() {
    const next = {
      t: Date.now(),
      requests: total("haarness_router_requests_total"),
      errors: total("haarness_router_errors_total"),
      active: total("haarness_router_active_streams"),
      first: total("haarness_router_first_event_microseconds_total"),
      // Every routed request starts a stream (or a buffered answer) and records one first event.
      routed:
        total("haarness_router_active_streams") +
        total("haarness_router_completed_streams_total") +
        total("haarness_router_failed_streams_total") +
        total("haarness_router_cancelled_streams_total"),
    };
    const last = state.samples[state.samples.length - 1];
    // Counters going backwards mean the router restarted: the old history no longer applies.
    if (last && (next.requests < last.requests || next.routed < last.routed)) state.samples = [];
    state.samples.push(next);
    const horizon = next.t - WINDOW_MS - SAMPLE_MS * 2;
    while (state.samples.length && state.samples[0].t < horizon) state.samples.shift();
  }

  function setMetricsStatus(status) {
    state.metricsStatus = status;
    renderAccessNotice(byId("activity-key"));
    renderAccessNotice(byId("obs-key"));
    byId("activity-stats").classList.toggle("hidden", status === "needs-key" || status === "rejected");
    if (state.view === "observability") renderObservability();
  }

  // The prompt for a router key, shared by the activity card, observability and the playground.
  function renderAccessNotice(container, purpose = "activity") {
    if (!container) return;
    const status = purpose === "playground" ? playgroundAccess() : state.metricsStatus;
    if (status !== "needs-key" && status !== "rejected") {
      if (!container.classList.contains("hidden")) {
        container.classList.add("hidden");
        container.replaceChildren();
        container.dataset.status = "";
      }
      return;
    }
    if (container.dataset.status === status) return;
    container.dataset.status = status;
    container.classList.remove("hidden");
    const notice = container.classList.contains("notice") ? container : el("div", "notice");
    notice.replaceChildren();
    const managed = state.overview?.security?.runtime_key_management;
    const what = purpose === "playground" ? "send requests" : "see activity";
    const text =
      status === "rejected"
        ? "The router didn’t accept this tab’s key."
        : `This router requires a key. Give this tab one to ${what}.`;
    const actions = el("div", "notice-actions");
    if (status === "rejected") actions.append(button("Forget the key", "", () => setRouterKey("", "")));
    if (managed) actions.append(button("Create one for this tab", status === "rejected" ? "" : "primary", createTabKey));
    const enter = el("a", "text-button", "Enter a key");
    enter.href = "#settings";
    actions.append(enter);
    notice.append(el("span", "", text), actions);
    if (notice !== container) container.replaceChildren(notice);
  }

  async function createTabKey(event) {
    const trigger = event?.currentTarget;
    if (trigger) trigger.disabled = true;
    try {
      const created = await uiApi("/api/keys", {
        method: "POST",
        headers: { "content-type": "application/json", "x-haarness-ui": "1" },
        body: JSON.stringify({ label: "Dashboard" }),
      });
      setRouterKey(created.secret, created.id);
      toast("This tab now has its own router key");
      if (state.view === "keys") loadKeys();
    } catch (error) {
      toast(sentence(error.message), "error");
      if (trigger) trigger.disabled = false;
    }
  }

  function setRouterKey(secret, id) {
    state.keySetAt = Date.now();
    session.set(ROUTER_KEY, secret);
    session.set(ROUTER_KEY_ID, secret ? id : "");
    refreshAccess();
  }

  // Whatever depends on the tab's router key, after it or the router's key requirement changed.
  function refreshAccess() {
    state.catalogAt = 0;
    const refused = keyRequired() && Boolean(routerKey()) && state.refusedKey === routerKey();
    state.catalogStatus = refused ? "rejected" : "idle";
    state.metricsStatus = refused ? "rejected" : "idle";
    renderAll();
    renderAccessNotice(byId("pg-key-notice"), "playground");
    loadMetrics(false);
    ensureCatalog(true);
    if (state.keys) loadKeys();
  }

  function seriesFor(name) {
    const samples = state.samples;
    const points = [];
    for (let index = 1; index < samples.length; index += 1) {
      const previous = samples[index - 1];
      const current = samples[index];
      const elapsed = current.t - previous.t;
      if (elapsed <= 0) continue;
      const gap = elapsed > SAMPLE_MS * 3;
      let value = null;
      const requests = current.requests - previous.requests;
      if (name === "requests") value = (requests / elapsed) * 60_000;
      if (name === "active") value = current.active;
      if (name === "errors") value = requests > 0 ? Math.min(1, (current.errors - previous.errors) / requests) : 0;
      if (name === "first") {
        const routed = current.routed - previous.routed;
        value = routed > 0 ? (current.first - previous.first) / routed / 1000 : null;
      }
      points.push({ t: current.t, value, gap });
    }
    return points;
  }

  function drawSpark(svg, points) {
    const width = Math.max(40, Math.round(svg.getBoundingClientRect().width) || 120);
    const height = 28;
    svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
    svg.setAttribute("preserveAspectRatio", "none");
    // The line spans what the page has seen, at least a minute and at most the last five.
    const now = Date.now();
    const seen = state.samples.length ? now - state.samples[0].t : 0;
    const span = Math.min(WINDOW_MS, Math.max(60_000, seen));
    const start = now - span;
    const values = points.filter((point) => point.value !== null && point.t >= start).map((point) => point.value);
    const max = Math.max(...values, 0) || 1;
    const x = (t) => ((t - start) / span) * width;
    const y = (value) => height - 3 - (value / max) * (height - 7);
    const nodes = [];
    const base = document.createElementNS(SVG, "line");
    base.setAttribute("class", "base");
    base.setAttribute("x1", "0");
    base.setAttribute("x2", String(width));
    base.setAttribute("y1", String(height - 2.5));
    base.setAttribute("y2", String(height - 2.5));
    nodes.push(base);
    const segments = [];
    let current = [];
    for (const point of points) {
      if (point.t < start) continue;
      if (point.value === null || point.gap) {
        if (current.length) segments.push(current);
        current = [];
        if (point.value === null) continue;
      }
      current.push(point);
    }
    if (current.length) segments.push(current);
    for (const segment of segments) {
      if (segment.length === 1) {
        const dot = document.createElementNS(SVG, "circle");
        dot.setAttribute("class", "line");
        dot.setAttribute("cx", x(segment[0].t).toFixed(1));
        dot.setAttribute("cy", y(segment[0].value).toFixed(1));
        dot.setAttribute("r", "1.6");
        nodes.push(dot);
        continue;
      }
      const line = segment.map((point, index) => `${index ? "L" : "M"}${x(point.t).toFixed(1)} ${y(point.value).toFixed(1)}`).join(" ");
      const area = document.createElementNS(SVG, "path");
      area.setAttribute("class", "area");
      area.setAttribute("d", `${line} L${x(segment[segment.length - 1].t).toFixed(1)} ${height - 2.5} L${x(segment[0].t).toFixed(1)} ${height - 2.5} Z`);
      const path = document.createElementNS(SVG, "path");
      path.setAttribute("class", "line");
      path.setAttribute("d", line);
      nodes.push(area, path);
    }
    svg.replaceChildren(...nodes);
  }

  function renderActivity() {
    if (!state.metrics) return;
    const requests = total("haarness_router_requests_total");
    const errors = total("haarness_router_errors_total");
    const routed =
      total("haarness_router_active_streams") +
      total("haarness_router_completed_streams_total") +
      total("haarness_router_failed_streams_total") +
      total("haarness_router_cancelled_streams_total");
    setFigure(byId("stat-requests"), [count(requests), ""]);
    setFigure(byId("stat-active"), [count(total("haarness_router_active_streams")), ""]);
    setFigure(byId("stat-errors"), requests ? percent(errors / requests) : ["—", ""]);
    setFigure(byId("stat-first"), routed ? duration(total("haarness_router_first_event_microseconds_total") / routed / 1000) : ["—", ""]);
    if (state.view !== "overview") return;
    drawSpark(byId("spark-requests"), seriesFor("requests"));
    drawSpark(byId("spark-active"), seriesFor("active"));
    drawSpark(byId("spark-errors"), seriesFor("errors"));
    drawSpark(byId("spark-first"), seriesFor("first"));
    renderActivityNote();
  }

  // Observability -------------------------------------------------------------------------------------------
  const FAILURE_WORDS = {
    "4xx": "Refused by the provider (4xx)",
    "5xx": "Provider error (5xx)",
    timeout: "Timed out",
    connect: "Couldn’t connect",
    other: "Other",
  };

  function renderObservability() {
    const fresh = state.metricsStatus === "ok" && Date.now() - state.metricsAt < SAMPLE_MS * 3;
    const updated = byId("obs-updated");
    updated.className = fresh ? "badge ok" : "page-note";
    updated.textContent = fresh ? "Live" : state.metricsAt ? `Updated ${clockTime(new Date(state.metricsAt))}` : "\u00a0";
    renderRecent();
    if (!state.metrics) {
      byId("obs-figures").replaceChildren();
      byId("route-rows").replaceChildren(emptyCell(8, state.metricsStatus === "ok" ? "No routes yet." : "Waiting for metrics…"));
      return;
    }
    const completed = total("haarness_router_completed_streams_total");
    const failed = total("haarness_router_failed_streams_total");
    const cancelled = total("haarness_router_cancelled_streams_total");
    const active = total("haarness_router_active_streams");
    const finished = completed + failed + cancelled;
    const routed = finished + active;
    const figures = [
      ["Requests", [count(total("haarness_router_requests_total")), ""]],
      ["Errors", [count(total("haarness_router_errors_total")), ""]],
      ["Fallback attempts", [count(total("haarness_router_fallback_attempts_total")), ""]],
      ["Active streams", [count(active), ""]],
      ["Response data", bytes(total("haarness_router_response_bytes_total"))],
      ["Completed", [count(completed), ""]],
      ["Failed", [count(failed), ""]],
      ["Cancelled", [count(cancelled), ""]],
      ["First event, average", routed ? duration(total("haarness_router_first_event_microseconds_total") / routed / 1000) : ["—", ""]],
      ["Completion, average", finished ? duration(total("haarness_router_completion_microseconds_total") / finished / 1000) : ["—", ""]],
    ];
    const list = byId("obs-figures");
    if (list.children.length !== figures.length) {
      list.replaceChildren(
        ...figures.map(([label]) => {
          const item = el("div");
          item.append(el("dt", "", label), el("dd"));
          return item;
        }),
      );
    }
    figures.forEach(([, value], index) => setFigure(list.children[index].querySelector("dd"), value));

    const routes = new Map();
    const columns = {
      haarness_router_route_requests_total: "requests",
      haarness_router_route_active_streams: "active",
      haarness_router_route_completed_streams_total: "completed",
      haarness_router_route_failed_streams_total: "failed",
      haarness_router_route_cancelled_streams_total: "cancelled",
      haarness_router_route_first_event_microseconds_total: "first",
    };
    for (const [family, column] of Object.entries(columns)) {
      for (const sample of state.metrics.get(family) || []) {
        const key = `${sample.labels.provider}/${sample.labels.model}`;
        if (!routes.has(key)) routes.set(key, { provider: sample.labels.provider, model: sample.labels.model, requests: 0, active: 0, completed: 0, failed: 0, cancelled: 0, first: 0 });
        routes.get(key)[column] = sample.value;
      }
    }
    const circuits = circuitStates();
    const all = [...routes.values()];
    const shown = state.showIdleRoutes ? all : all.filter((route) => route.requests > 0 || route.active > 0);
    const toggle = byId("toggle-idle");
    toggle.hidden = all.length === shown.length && !state.showIdleRoutes;
    toggle.textContent = state.showIdleRoutes ? "Hide idle routes" : `Show idle routes (${all.length - shown.length})`;
    toggle.setAttribute("aria-pressed", String(state.showIdleRoutes));
    const rows = shown.map((route) => {
      const row = el("tr");
      const name = el("td");
      const label = route.model === "_undeclared" ? `${route.provider}/…` : `${route.provider}/${route.model}`;
      const code = el("code", "", label);
      if (route.model === "_undeclared") code.title = "Models of this provider that the configuration doesn’t declare";
      name.append(code);
      const circuit = circuits.get(`${route.provider}/${route.model}`);
      const stateCell = el("td");
      if (circuit) stateCell.append(el("span", circuit === "closed" ? "badge ok" : circuit === "open" ? "badge bad" : "badge warn", circuit === "closed" ? "Healthy" : circuit === "open" ? "Open" : "Recovering"));
      else stateCell.textContent = "—";
      row.append(
        name,
        el("td", "", count(route.requests)),
        el("td", "", count(route.active)),
        el("td", "", count(route.completed)),
        el("td", "", count(route.failed)),
        el("td", "", count(route.cancelled)),
        el("td", "", route.requests ? durationText(route.first / route.requests / 1000) : "—"),
        stateCell,
      );
      return row;
    });
    byId("route-rows").replaceChildren(...(rows.length ? rows : [emptyCell(8, all.length ? "No traffic yet." : "No routes yet.")]));

    const failures = (state.metrics.get("haarness_router_upstream_failures_total") || []).filter((sample) => sample.value > 0);
    byId("failures-card").classList.toggle("hidden", !failures.length);
    byId("failure-rows").replaceChildren(
      ...failures.map((sample) => {
        const row = el("tr");
        const name = el("td");
        name.append(el("code", "", sample.labels.model === "_undeclared" ? `${sample.labels.provider}/…` : `${sample.labels.provider}/${sample.labels.model}`));
        row.append(name, el("td", "", sample.labels.alias && sample.labels.alias !== "_direct" ? sample.labels.alias : "Direct"), el("td", "", FAILURE_WORDS[sample.labels.class] || sample.labels.class), el("td", "", count(sample.value)));
        return row;
      }),
    );
    const raw = byId("raw-metrics");
    if (byId("raw-details").open && raw.textContent !== state.metricsRaw) raw.textContent = state.metricsRaw;
  }

  function emptyCell(span, text) {
    const row = el("tr");
    const cell = el("td", "empty-cell", text);
    cell.colSpan = span;
    row.append(cell);
    return row;
  }

  function renderRecent() {
    const rows = state.activity.map((entry) => {
      const row = el("tr");
      const time = el("td", "", clockTime(entry.at));
      const model = el("td");
      model.append(el("code", "", entry.route || entry.model));
      const result = el("td");
      result.append(el("span", entry.status === "ok" ? "badge ok" : entry.status === "stopped" ? "badge plain" : "badge bad", entry.status === "ok" ? "Answered" : entry.status === "stopped" ? "Stopped" : "Failed"));
      if (entry.error) result.title = entry.error;
      row.append(time, model, result, el("td", "", entry.first === null ? "—" : durationText(entry.first)), el("td", "", durationText(entry.total)), el("td", "", entry.tokens === null ? "—" : count(entry.tokens)));
      return row;
    });
    byId("recent-rows").replaceChildren(...(rows.length ? rows : [emptyCell(6, "Requests from the playground in this tab appear here.")]));
  }

  function initObservability() {
    byId("toggle-idle").addEventListener("click", () => {
      state.showIdleRoutes = !state.showIdleRoutes;
      renderObservability();
    });
    byId("raw-details").addEventListener("toggle", () => {
      if (byId("raw-details").open) byId("raw-metrics").textContent = state.metricsRaw || "No metrics yet.";
    });
  }

  // API keys ------------------------------------------------------------------------------------------------
  async function loadKeys() {
    const requestedAt = Date.now();
    try {
      const data = await uiApi("/api/keys");
      state.keys = data;
      state.keysRequestedAt = requestedAt;
      renderKeys();
    } catch (error) {
      if (error.status) toast(sentence(error.message), "error");
    }
  }

  function renderKeysChrome() {
    const security = state.overview?.security;
    if (!security) return;
    const managed = security.runtime_key_management;
    const create = byId("create-key");
    create.disabled = !managed;
    create.title = managed ? "" : "Turned off in this router’s configuration";
    const lede = keyRequired() ? "Clients need a router key to call this router." : "This router takes requests without a key. Keys made here work too and show when they were last used.";
    setText("keys-lede", managed ? lede : `${lede} Creating and revoking keys is turned off in its configuration.`);
  }

  function renderKeys() {
    const data = state.keys;
    if (!data) return;
    renderKeysChrome();
    const keys = data.runtime_keys || [];
    const sessionId = session.get(ROUTER_KEY_ID);
    if (sessionId && routerKey() && state.keysRequestedAt > (state.keySetAt || 0) && !keys.some((key) => key.id === sessionId)) {
      // The tab's key was revoked elsewhere or the router restarted: it can't work any more.
      session.set(ROUTER_KEY, "");
      session.set(ROUTER_KEY_ID, "");
      refreshAccess();
      return;
    }
    const signature = JSON.stringify([keys, state.pendingRevoke, sessionId, data.runtime_key_management, Math.floor(Date.now() / 30_000)]);
    if (signature !== state.keysSignature) {
      state.keysSignature = signature;
      const rows = keys.map((key) => keyRow(key, sessionId, data.runtime_key_management));
      byId("key-rows").replaceChildren(...(rows.length ? rows : [emptyCell(5, "No keys made here yet.")]));
    }
    const parts = [];
    if (data.bootstrap_key_count) {
      parts.push(
        data.bootstrap_key_count === 1
          ? "One key from the router’s environment also works; it isn’t listed and can’t be revoked here."
          : `${count(data.bootstrap_key_count)} keys from the router’s environment also work; they aren’t listed and can’t be revoked here.`,
      );
    }
    parts.push(`Keys made here last until the router restarts. ${count(keys.length)} of ${count(data.max_runtime_keys)} in use.`);
    setText("keys-foot", parts.join(" "));
  }

  function keyRow(key, sessionId, managed) {
    const row = el("tr");
    row.dataset.id = key.id;
    const name = el("td");
    const label = el("strong", "", key.label);
    name.append(label);
    if (key.id === sessionId) {
      const tag = el("span", "tag", "This tab");
      tag.title = "The playground and activity in this tab use this key";
      name.append(" ", tag);
    }
    const id = el("td");
    id.append(el("code", "", key.id));
    const created = el("td", "", relative(key.created_at));
    created.title = absolute(key.created_at);
    const used = el("td", "", relative(key.last_used_at));
    if (key.last_used_at) used.title = absolute(key.last_used_at);
    const actions = el("td");
    const group = el("div", "actions");
    if (state.pendingRevoke === key.id) {
      const keep = button("Keep", "ghost", () => {
        state.pendingRevoke = "";
        renderKeys();
      });
      const confirm = button("Revoke", "danger", () => revokeKey(key, row, confirm));
      confirm.setAttribute("aria-label", `Revoke ${key.label} now`);
      group.append(keep, confirm);
      window.setTimeout(() => confirm.focus(), 0);
    } else {
      const revoke = button("Revoke", "ghost", () => {
        state.pendingRevoke = key.id;
        renderKeys();
        schedule("revoke", () => {
          if (state.pendingRevoke === key.id) {
            state.pendingRevoke = "";
            renderKeys();
          }
        }, 6000);
      });
      revoke.disabled = !managed;
      revoke.setAttribute("aria-label", `Revoke ${key.label}`);
      group.append(revoke);
    }
    actions.append(group);
    row.append(name, id, created, used, actions);
    return row;
  }

  async function revokeKey(key, row, trigger) {
    trigger.disabled = true;
    try {
      await uiApi(`/api/keys/${encodeURIComponent(key.id)}`, { method: "DELETE", headers: { "x-haarness-ui": "1" } });
      state.pendingRevoke = "";
      const wasThisTab = session.get(ROUTER_KEY_ID) === key.id;
      row.classList.add("row-leave");
      await new Promise((resolve) => window.setTimeout(resolve, 240));
      if (wasThisTab) setRouterKey("", "");
      toast(wasThisTab ? "Key revoked. This tab no longer has a router key." : "Key revoked");
      await loadKeys();
    } catch (error) {
      trigger.disabled = false;
      toast(sentence(error.message), "error");
    }
  }

  function initKeys() {
    const card = byId("new-key");
    const form = byId("new-key-form");
    const reveal = byId("new-key-reveal");
    let secret = "";
    let createdId = "";
    const close = () => {
      card.classList.add("hidden");
      secret = "";
      createdId = "";
      byId("new-key-secret").textContent = "";
    };
    byId("create-key").addEventListener("click", () => {
      card.classList.remove("hidden");
      form.classList.remove("hidden");
      reveal.classList.add("hidden");
      const taken = new Set((state.keys?.runtime_keys || []).map((key) => key.label));
      let index = (state.keys?.runtime_keys || []).length + 1;
      while (taken.has(`Key ${index}`)) index += 1;
      const input = byId("new-key-label");
      input.value = `Key ${index}`;
      input.focus();
      input.select();
    });
    byId("new-key-cancel").addEventListener("click", close);
    form.addEventListener("keydown", (event) => {
      if (event.key === "Escape") close();
    });
    form.addEventListener("submit", async (event) => {
      event.preventDefault();
      const label = byId("new-key-label").value.trim();
      if (!label) return;
      const submit = byId("new-key-submit");
      submit.disabled = true;
      try {
        const created = await uiApi("/api/keys", {
          method: "POST",
          headers: { "content-type": "application/json", "x-haarness-ui": "1" },
          body: JSON.stringify({ label }),
        });
        secret = created.secret;
        createdId = created.id;
        byId("new-key-secret").textContent = secret;
        setText("new-key-title", `Copy “${created.label}” now`);
        form.classList.add("hidden");
        reveal.classList.remove("hidden");
        byId("new-key-copy").focus();
        await loadKeys();
      } catch (error) {
        toast(sentence(error.message), "error");
      } finally {
        submit.disabled = false;
      }
    });
    byId("new-key-copy").addEventListener("click", (event) => copy(secret, event.currentTarget));
    byId("new-key-done").addEventListener("click", close);
    byId("new-key-use").addEventListener("click", () => {
      setRouterKey(secret, createdId);
      close();
      toast("The playground in this tab now uses this key");
      window.location.hash = "#playground";
    });
  }

  // Settings ------------------------------------------------------------------------------------------------
  function definition(container, rows) {
    container.replaceChildren(
      ...rows.flatMap(([term, value, note, copyValue]) => {
        const dt = el("dt", "", term);
        const dd = el("dd");
        if (value instanceof Node) dd.append(value);
        else dd.append(document.createTextNode(value));
        if (copyValue) dd.append(iconButton("copy", `Copy ${term.toLowerCase()}`, (event) => copy(copyValue, event.currentTarget)));
        if (note) dd.append(note instanceof Node ? note : el("span", "note", note));
        return [dt, dd];
      }),
    );
  }

  function codeText(text) {
    return el("code", "", text);
  }

  function renderSettings() {
    const data = state.overview;
    if (!data) return;
    const signature = JSON.stringify([data.security, data.endpoints, data.version, data.started_at, data.config, routerKey()]);
    if (byId("set-about").dataset.signature === signature) return;
    byId("set-about").dataset.signature = signature;
    const security = data.security || {};
    const required = keyRequired();
    const key = routerKey();
    byId("key-form").classList.toggle("hidden", !required);
    setText(
      "set-key-text",
      !required
        ? "Not needed: this router takes requests without a key."
        : key
          ? `This tab uses a key ending in ${key.slice(-4)} for the playground and activity. It’s kept only until the tab closes.`
          : "This router requires a key. Enter one here, or create one on API keys. It’s kept only until the tab closes.",
    );
    byId("set-key-forget").hidden = !key;

    const exposed = !security.loopback_bind;
    const signIn = el("span", "", security.basic_auth ? "Password" : "None");
    const signNote = !security.basic_auth && exposed ? el("span", "badge warn", "Anyone who can reach the router can open the dashboard") : null;
    definition(byId("set-access"), [
      ["Router key", required ? "Required" : "Not required", security.bootstrap_keys ? `${plural(security.bootstrap_keys, "key", "keys")} from the environment` : ""],
      ["Dashboard sign-in", signIn, signNote],
      ["Listening on", security.loopback_bind ? "This computer only" : "The network"],
      ["Keys made here", security.runtime_key_management ? `Allowed, up to ${count(security.max_runtime_keys)}` : "Turned off"],
      ["Provider key per request", security.upstream_key_override ? "Allowed" : "Turned off", security.upstream_key_override ? "X-Upstream-API-Key, for provider/model targets" : ""],
    ]);

    const endpoints = data.endpoints || {};
    const base = origin();
    definition(byId("set-endpoints"), [
      ["Chat completions", codeText(`${base}${endpoints.chat || "/v1/chat/completions"}`), "", `${base}${endpoints.chat || "/v1/chat/completions"}`],
      ["Models", codeText(`${base}${endpoints.models || "/v1/models"}`), "", `${base}${endpoints.models || "/v1/models"}`],
      ["Metrics", codeText(`${base}${endpoints.metrics || "/metrics"}`), required ? "Router key or metrics token" : "", `${base}${endpoints.metrics || "/metrics"}`],
      ["Health", codeText(`${base}${endpoints.health || "/healthz"}`), "", `${base}${endpoints.health || "/healthz"}`],
      ["Dashboard", codeText(`${base}${endpoints.ui || BASE}`), "", ""],
    ]);

    const source = configSource(data);
    const about = [
      ["Version", data.version ? `v${data.version}` : "—"],
      ["Started", data.started_at ? absolute(data.started_at) : "—"],
    ];
    if (source) {
      about.push([
        "Configuration",
        source.kind === "environment" ? "From the environment" : source.kind === "stdin" ? "Standard input" : source.path ? codeText(source.path) : "A file",
      ]);
    }
    definition(byId("set-about"), about);
  }

  function initSettings() {
    byId("key-form").addEventListener("submit", (event) => {
      event.preventDefault();
      const input = byId("set-key-input");
      const value = input.value.trim();
      if (!value) {
        input.focus();
        return;
      }
      input.value = "";
      setRouterKey(value, "");
      toast("This tab now uses the key");
    });
    byId("set-key-forget").addEventListener("click", () => {
      setRouterKey("", "");
      toast("Key forgotten");
    });
  }

  // Playground: options -------------------------------------------------------------------------------------
  function loadOptions() {
    const defaults = { system: "", effort: "", stream: true, temperature: "", maxTokens: "", tools: "", schema: "", open: false };
    try {
      const saved = JSON.parse(local.get(PLAYGROUND_KEY) || "{}");
      return { ...defaults, ...(saved && typeof saved === "object" ? saved : {}) };
    } catch {
      return defaults;
    }
  }

  function saveOptions() {
    schedule("save-options", () => local.set(PLAYGROUND_KEY, JSON.stringify(state.options)), 300);
  }

  const EFFORT_NAMES = { none: "None", minimal: "Minimal", low: "Low", medium: "Medium", high: "High", xhigh: "Extra high", max: "Max" };

  function modelInfo(id) {
    const { aliases, models } = catalogEntries();
    const alias = aliases.find((item) => item.id === id);
    if (alias) return { alias };
    return { model: models.find((item) => item.id === id) };
  }

  function effortsFor(id) {
    const { model } = modelInfo(id);
    const listed = model?.meta?.reasoning_efforts;
    if (Array.isArray(listed)) return listed.filter((effort) => typeof effort === "string");
    if (model && capabilitiesOf(model).reasoning === false) return [];
    return ["none", "low", "medium", "high"];
  }

  function renderOptions() {
    const options = state.options;
    const efforts = effortsFor(state.model);
    if (options.effort && !efforts.includes(options.effort)) options.effort = "";
    const select = byId("opt-reasoning");
    byId("opt-reasoning-field").classList.toggle("hidden", !efforts.length);
    const choices = ["", ...efforts];
    const signature = choices.join("|");
    if (select.dataset.signature !== signature) {
      select.dataset.signature = signature;
      select.replaceChildren(...choices.map((effort) => new Option(effort ? EFFORT_NAMES[effort] || effort : "Default", effort)));
    }
    select.value = options.effort;
    byId("opt-stream").setAttribute("aria-checked", String(options.stream !== false));
    const security = state.overview?.security || {};
    byId("opt-upstream-field").classList.toggle("hidden", !security.upstream_key_override);
    byId("opt-router-key-field").classList.toggle("hidden", !keyRequired());
    setText("opt-router-key-state", routerKey() ? `A key ending in ${routerKey().slice(-4)}` : "None for this tab");
  }

  function setOptionsOpen(open, remember = true) {
    const panel = byId("pg-options");
    panel.hidden = !open;
    byId("pg").classList.toggle("with-options", open);
    byId("options-toggle").setAttribute("aria-expanded", String(open));
    if (remember && !NARROW.matches) {
      state.options.open = open;
      saveOptions();
    }
  }

  function parseJsonField(id, label, normalize) {
    const field = byId(id);
    const source = field.value.trim();
    field.classList.remove("invalid");
    if (!source) return undefined;
    let value;
    try {
      value = JSON.parse(source);
    } catch (error) {
      field.classList.add("invalid");
      throw new Error(`${label} isn’t valid JSON: ${error.message}`);
    }
    return normalize(value);
  }

  // A bare function, or a bare JSON Schema, is wrapped into the request shape the API expects.
  function normalizeTools(value) {
    const list = Array.isArray(value) ? value : [value];
    return list.map((tool) => {
      if (tool && typeof tool === "object" && !tool.type && tool.name) return { type: "function", function: tool };
      return tool;
    });
  }

  function normalizeSchema(value) {
    if (value && typeof value === "object" && !Array.isArray(value) && !value.schema && (value.type || value.properties)) {
      return { name: "answer", strict: true, schema: value };
    }
    return value;
  }

  const TOOL_EXAMPLE = JSON.stringify(
    [
      {
        type: "function",
        function: {
          name: "get_weather",
          description: "Current weather for a city",
          parameters: { type: "object", properties: { city: { type: "string" } }, required: ["city"], additionalProperties: false },
        },
      },
    ],
    null,
    2,
  );
  const SCHEMA_EXAMPLE = JSON.stringify(
    {
      name: "answer",
      strict: true,
      schema: { type: "object", properties: { answer: { type: "string" }, confidence: { type: "number" } }, required: ["answer", "confidence"], additionalProperties: false },
    },
    null,
    2,
  );

  function initOptions() {
    const options = state.options;
    const bind = (id, key) => {
      const field = byId(id);
      field.value = options[key] ?? "";
      field.addEventListener("input", () => {
        options[key] = field.value;
        field.classList.remove("invalid");
        saveOptions();
      });
    };
    bind("opt-system", "system");
    bind("opt-temperature", "temperature");
    bind("opt-max-tokens", "maxTokens");
    bind("opt-tools", "tools");
    bind("opt-schema", "schema");
    byId("opt-reasoning").addEventListener("change", (event) => {
      options.effort = event.target.value;
      saveOptions();
    });
    byId("opt-stream").addEventListener("click", () => {
      options.stream = options.stream === false;
      saveOptions();
      renderOptions();
    });
    const example = (id, key, text) => {
      const field = byId(id);
      field.value = text;
      options[key] = text;
      field.classList.remove("invalid");
      saveOptions();
      field.focus();
    };
    byId("opt-tools-example").addEventListener("click", () => example("opt-tools", "tools", TOOL_EXAMPLE));
    byId("opt-schema-example").addEventListener("click", () => example("opt-schema", "schema", SCHEMA_EXAMPLE));
    for (const [id, label, normalize] of [
      ["opt-tools", "Tools", normalizeTools],
      ["opt-schema", "Structured output", normalizeSchema],
    ]) {
      byId(id).addEventListener("blur", () => {
        try {
          parseJsonField(id, label, normalize);
        } catch {
          // Marked invalid; the message comes when sending.
        }
      });
    }
    byId("options-toggle").addEventListener("click", () => setOptionsOpen(byId("pg-options").hidden));
    byId("options-close").addEventListener("click", () => setOptionsOpen(false));
    setOptionsOpen(Boolean(options.open) && !NARROW.matches, false);
    renderOptions();
  }

  // Playground: model picker --------------------------------------------------------------------------------
  function selectModel(id) {
    state.model = id;
    local.set(MODEL_KEY, id);
    renderPicker();
    renderOptions();
    renderPlaygroundState();
  }

  function pickerItems() {
    const { aliases, models } = catalogEntries();
    const items = aliases.map((alias) => ({ id: alias.id, group: "Aliases", note: alias.targets.length ? `${plural(alias.targets.length, "target", "targets")}: ${alias.targets.join(" → ")}` : "No targets" }));
    for (const model of models) {
      const provider = providerOf(model.provider);
      items.push({ id: model.id, group: provider ? providerLabel(provider) : model.provider, note: contextNote(model.meta) });
    }
    return items;
  }

  function renderPicker() {
    if (!state.overview) return;
    const items = pickerItems();
    const settled = !canCallApi() || !["idle", "loading"].includes(state.catalogStatus);
    if (!items.some((item) => item.id === state.model) && (settled || !state.model)) {
      const fallback = items.find((item) => item.id === "default") || items[0];
      state.model = fallback ? fallback.id : "";
    }
    setText("picker-name", state.model || "No models yet");
    byId("picker-button").disabled = !items.length;
    byId("prompt").placeholder = state.model ? `Message ${state.model}` : "Message";
    if (!byId("picker-menu").hidden) renderPickerList(true);
  }

  function renderPickerList(keepActive) {
    const query = byId("picker-search").value.trim().toLowerCase();
    // A model's own name, its provider and its display name match; an alias's targets don't, or every alias
    // would match every model it routes to.
    const items = pickerItems().filter((item) => !query || `${item.id} ${item.group} ${item.group === "Aliases" ? "" : item.note}`.toLowerCase().includes(query));
    state.pickerItems = items;
    if (!keepActive && query) {
      const rank = (item) => {
        const id = item.id.toLowerCase();
        const name = id.slice(id.indexOf("/") + 1);
        if (id === query || name === query) return 0;
        if (id.startsWith(query) || name.startsWith(query)) return 1;
        if (id.includes(query)) return 2;
        return 3;
      };
      let best = 0;
      items.forEach((item, index) => {
        if (rank(item) < rank(items[best])) best = index;
      });
      state.pickerActive = best;
    }
    if (state.pickerActive >= items.length) state.pickerActive = items.length - 1;
    const list = byId("picker-list");
    const nodes = [];
    let group = "";
    items.forEach((item, index) => {
      if (item.group !== group) {
        group = item.group;
        nodes.push(el("div", "picker-group", group));
      }
      const option = el("div", "picker-option");
      option.id = `picker-option-${index}`;
      option.setAttribute("role", "option");
      option.setAttribute("aria-selected", String(item.id === state.model));
      if (index === state.pickerActive) option.classList.add("active");
      option.append(el("span", "picker-option-id", item.id));
      if (item.note) option.append(el("span", "picker-option-note", item.note));
      if (item.id === state.model) option.append(icon("check"));
      option.addEventListener("mousedown", (event) => event.preventDefault());
      option.addEventListener("click", () => choosePickerItem(item));
      option.addEventListener("mousemove", () => {
        if (state.pickerActive !== index) {
          state.pickerActive = index;
          highlightPicker();
        }
      });
      nodes.push(option);
    });
    if (!items.length) nodes.push(el("div", "picker-empty", "No model or alias matches."));
    list.replaceChildren(...nodes);
    highlightPicker();
  }

  function highlightPicker() {
    const list = byId("picker-list");
    for (const option of list.querySelectorAll(".picker-option")) option.classList.toggle("active", option.id === `picker-option-${state.pickerActive}`);
    const active = list.querySelector(".picker-option.active");
    const search = byId("picker-search");
    if (active) {
      search.setAttribute("aria-activedescendant", active.id);
      active.scrollIntoView({ block: "nearest" });
    } else search.removeAttribute("aria-activedescendant");
  }

  function choosePickerItem(item) {
    selectModel(item.id);
    closePicker();
    byId("prompt").focus();
  }

  function openPicker() {
    const menu = byId("picker-menu");
    menu.hidden = false;
    byId("picker-button").setAttribute("aria-expanded", "true");
    byId("picker-search").value = "";
    state.pickerActive = Math.max(0, pickerItems().findIndex((item) => item.id === state.model));
    renderPickerList(true);
    byId("picker-search").focus();
    ensureCatalog();
  }

  function closePicker() {
    byId("picker-menu").hidden = true;
    byId("picker-button").setAttribute("aria-expanded", "false");
  }

  function initPicker() {
    byId("picker-button").addEventListener("click", () => (byId("picker-menu").hidden ? openPicker() : closePicker()));
    byId("picker-search").addEventListener("input", () => {
      state.pickerActive = 0;
      renderPickerList();
    });
    byId("picker-search").addEventListener("keydown", (event) => {
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        const length = state.pickerItems.length;
        if (!length) return;
        state.pickerActive = (state.pickerActive + (event.key === "ArrowDown" ? 1 : length - 1)) % length;
        highlightPicker();
      } else if (event.key === "Enter") {
        event.preventDefault();
        const item = state.pickerItems[state.pickerActive];
        if (item) choosePickerItem(item);
      } else if (event.key === "Escape") {
        event.preventDefault();
        closePicker();
        byId("picker-button").focus();
      }
    });
    document.addEventListener("mousedown", (event) => {
      if (!byId("picker").contains(event.target)) closePicker();
      if (!event.target.closest(".attach-wrap")) closeAttachMenu();
    });
  }

  // Playground: attachments ---------------------------------------------------------------------------------
  const AUDIO_FORMATS = { "audio/mpeg": "mp3", "audio/mp3": "mp3", "audio/wav": "wav", "audio/x-wav": "wav", "audio/wave": "wav" };

  function attachmentKind(type, name) {
    if (type.startsWith("image/")) return "image";
    if (type === "application/pdf" || /\.pdf$/i.test(name)) return "pdf";
    if (type.startsWith("video/")) return "video";
    if (type.startsWith("audio/")) return "audio";
    return "";
  }

  function readFile(file) {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve(String(reader.result));
      reader.onerror = () => reject(reader.error);
      reader.readAsDataURL(file);
    });
  }

  async function addFiles(files) {
    for (const file of files) {
      const kind = attachmentKind(file.type || "", file.name || "");
      if (!kind) {
        toast(`${file.name || "That file"} isn’t an image, PDF, audio or video file.`, "error");
        continue;
      }
      if (kind === "audio" && !AUDIO_FORMATS[file.type]) {
        toast("Audio needs to be MP3 or WAV.", "error");
        continue;
      }
      try {
        let url = await readFile(file);
        // Some browsers label PDFs as application/octet-stream; the router wants the real type.
        if (kind === "pdf" && !url.startsWith("data:application/pdf")) url = `data:application/pdf;base64,${url.slice(url.indexOf(",") + 1)}`;
        state.attachments.push({ kind, name: file.name || kind, url, format: AUDIO_FORMATS[file.type] || "", size: file.size });
      } catch {
        toast(`${file.name} couldn’t be read.`, "error");
      }
    }
    renderAttachments();
  }

  function addUrl() {
    const input = byId("url-input");
    const url = input.value.trim();
    if (!url) {
      input.focus();
      return;
    }
    let parsed;
    try {
      parsed = new URL(url);
    } catch {
      parsed = null;
    }
    if (!parsed || parsed.protocol !== "https:") {
      input.classList.add("invalid");
      toast("Use an https:// address the provider can reach.", "error");
      input.focus();
      return;
    }
    let kind = byId("url-kind").value;
    if (kind === "auto") {
      const path = parsed.pathname.toLowerCase();
      kind = /\.pdf$/.test(path) ? "pdf" : /\.(mp4|webm|mov|m4v|mpeg|avi)$/.test(path) ? "video" : "image";
    }
    const name = decodeURIComponent(parsed.pathname.split("/").filter(Boolean).pop() || parsed.host);
    state.attachments.push({ kind, name, url, format: "", size: 0, remote: true });
    input.value = "";
    input.classList.remove("invalid");
    byId("url-row").classList.add("hidden");
    renderAttachments();
    byId("prompt").focus();
  }

  function attachmentChip(attachment, onRemove) {
    const chip = el("span", "chip-file");
    if (attachment.kind === "image") {
      const image = el("img");
      image.alt = "";
      image.src = attachment.url;
      chip.append(image);
    } else {
      const glyph = el("span", "chip-glyph");
      glyph.append(icon(attachment.remote ? "link" : "file"));
      chip.append(glyph);
    }
    const label = el("span", "", attachment.name);
    label.title = attachment.remote ? attachment.url : attachment.name;
    chip.append(label);
    if (onRemove) chip.append(iconButton("close", `Remove ${attachment.name}`, onRemove));
    return chip;
  }

  function renderAttachments() {
    byId("attachments").replaceChildren(
      ...state.attachments.map((attachment, index) =>
        attachmentChip(attachment, () => {
          state.attachments.splice(index, 1);
          renderAttachments();
          byId("prompt").focus();
        }),
      ),
    );
  }

  function closeAttachMenu() {
    byId("attach-menu").hidden = true;
    byId("attach-button").setAttribute("aria-expanded", "false");
  }

  function initAttachments() {
    const menu = byId("attach-menu");
    byId("attach-button").addEventListener("click", () => {
      menu.hidden = !menu.hidden;
      byId("attach-button").setAttribute("aria-expanded", String(!menu.hidden));
      if (!menu.hidden) byId("attach-file").focus();
    });
    menu.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        closeAttachMenu();
        byId("attach-button").focus();
      }
    });
    byId("attach-file").addEventListener("click", () => {
      closeAttachMenu();
      byId("file-input").click();
    });
    byId("attach-url").addEventListener("click", () => {
      closeAttachMenu();
      byId("url-row").classList.remove("hidden");
      byId("url-input").focus();
    });
    byId("file-input").addEventListener("change", (event) => {
      addFiles([...event.target.files]);
      event.target.value = "";
    });
    byId("url-add").addEventListener("click", addUrl);
    byId("url-input").addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        addUrl();
      } else if (event.key === "Escape") {
        byId("url-row").classList.add("hidden");
        byId("prompt").focus();
      }
    });
    byId("url-cancel").addEventListener("click", () => {
      byId("url-row").classList.add("hidden");
      byId("prompt").focus();
    });
    const composer = byId("composer");
    composer.addEventListener("dragover", (event) => {
      if (![...(event.dataTransfer?.types || [])].includes("Files")) return;
      event.preventDefault();
      composer.classList.add("dragging");
    });
    composer.addEventListener("dragleave", (event) => {
      if (!composer.contains(event.relatedTarget)) composer.classList.remove("dragging");
    });
    composer.addEventListener("drop", (event) => {
      composer.classList.remove("dragging");
      if (!event.dataTransfer?.files?.length) return;
      event.preventDefault();
      addFiles([...event.dataTransfer.files]);
    });
    byId("prompt").addEventListener("paste", (event) => {
      const files = [...(event.clipboardData?.files || [])];
      if (!files.length) return;
      event.preventDefault();
      addFiles(files);
    });
  }

  // Playground: conversation --------------------------------------------------------------------------------
  function playgroundAccess() {
    if (!state.overview || !keyRequired()) return "ok";
    if (!routerKey()) return "needs-key";
    if (state.catalogStatus === "rejected" || state.metricsStatus === "rejected") return "rejected";
    return "ok";
  }

  function renderPlaygroundState() {
    if (!state.overview) return;
    const hasModels = pickerItems().length > 0;
    const empty = byId("pg-empty");
    empty.hidden = state.turns.length > 0;
    setText("pg-empty-title", hasModels ? "Send a message to try a model." : "No models to try yet.");
    setText("pg-empty-text", hasModels ? "Requests go through this router’s own API, the way any client would send them." : "Add a provider to the router first: the Overview says how.");
    byId("prompt").disabled = !hasModels;
    byId("attach-button").disabled = !hasModels;
    updateSendButton();
    renderAccessNotice(byId("pg-key-notice"), "playground");
  }

  function updateSendButton() {
    const send = byId("send");
    const running = Boolean(state.running);
    send.classList.toggle("running", running);
    send.setAttribute("aria-label", running ? "Stop" : "Send");
    send.title = running ? "Stop (Esc)" : "Send (Enter)";
    const hasContent = byId("prompt").value.trim() || state.attachments.length;
    send.disabled = !running && (!state.model || !hasContent);
    setText("composer-hint", running ? "Esc stops the answer" : "");
  }

  function autosize() {
    const prompt = byId("prompt");
    if (!prompt.offsetParent) return;
    prompt.style.height = "auto";
    prompt.style.height = `${Math.min(prompt.scrollHeight, Math.round(window.innerHeight * 0.4))}px`;
  }

  function nearBottom() {
    const transcript = byId("transcript");
    return transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight < 96;
  }

  function scrollToEnd(force) {
    const transcript = byId("transcript");
    if (force || state.stickToEnd) transcript.scrollTop = transcript.scrollHeight;
  }

  function userContent(text, attachments) {
    if (!attachments.length) return text;
    const parts = [];
    if (text) parts.push({ type: "text", text });
    for (const attachment of attachments) {
      if (attachment.kind === "image") parts.push({ type: "image_url", image_url: { url: attachment.url } });
      else if (attachment.kind === "pdf") parts.push({ type: "file", file: { file_data: attachment.url, filename: attachment.name } });
      else if (attachment.kind === "video") parts.push({ type: "video_url", video_url: { url: attachment.url } });
      else if (attachment.kind === "audio") parts.push({ type: "input_audio", input_audio: { data: attachment.url.slice(attachment.url.indexOf(",") + 1), format: attachment.format } });
    }
    return parts;
  }

  // The conversation as Chat Completions messages, rebuilt from the turns each time.
  function history() {
    const messages = [];
    for (const turn of state.turns) {
      if (turn.role === "user") messages.push({ role: "user", content: userContent(turn.text, turn.attachments) });
      else if (turn.role === "tool") for (const result of turn.results) messages.push({ role: "tool", tool_call_id: result.id, content: result.content });
      else if (turn.role === "assistant") {
        const calls = turn.abandoned ? [] : [...turn.calls.values()].filter((call) => call.name);
        if (!turn.content && !calls.length) continue;
        const message = { role: "assistant", content: turn.content || null };
        if (calls.length) {
          message.tool_calls = calls.map((call) =>
            call.type === "custom"
              ? { id: call.id, type: "custom", custom: { name: call.name, input: call.arguments } }
              : { id: call.id, type: "function", function: { name: call.name, arguments: call.arguments || "{}" } },
          );
        }
        if (turn.details.length) message.reasoning_details = turn.details;
        messages.push(message);
      }
    }
    return messages;
  }

  function buildRequest() {
    const options = state.options;
    const tools = parseJsonField("opt-tools", "Tools", normalizeTools);
    const schema = parseJsonField("opt-schema", "Structured output", normalizeSchema);
    const messages = [];
    if (options.system.trim()) messages.push({ role: "system", content: options.system.trim() });
    messages.push(...history());
    const stream = options.stream !== false;
    const request = { model: state.model, messages, stream };
    if (stream) request.stream_options = { include_usage: true };
    const maxTokens = Number(options.maxTokens);
    if (options.maxTokens !== "" && Number.isFinite(maxTokens) && maxTokens > 0) request.max_completion_tokens = Math.round(maxTokens);
    if (options.temperature !== "" && Number.isFinite(Number(options.temperature))) request.temperature = Number(options.temperature);
    if (options.effort) request.reasoning_effort = options.effort;
    if (tools) request.tools = tools;
    if (schema) request.response_format = { type: "json_schema", json_schema: schema };
    return request;
  }

  function submitPrompt() {
    if (state.running) return;
    const prompt = byId("prompt");
    const text = prompt.value.trim();
    if (!state.model || (!text && !state.attachments.length)) return;
    if (playgroundAccess() !== "ok") {
      renderAccessNotice(byId("pg-key-notice"), "playground");
      byId("pg-key-notice").querySelector("button")?.focus();
      return;
    }
    const upstream = byId("opt-upstream-key").value;
    if (upstream && modelInfo(state.model).alias) {
      setOptionsOpen(true);
      byId("opt-upstream-key").focus();
      toast(`A provider key works only with a provider/model target, not the alias “${state.model}”.`, "error");
      return;
    }
    try {
      buildRequest();
    } catch (error) {
      setOptionsOpen(true);
      byId("pg-options").querySelector(".invalid")?.focus();
      toast(error.message, "error");
      return;
    }
    const turn = { role: "user", text, attachments: state.attachments };
    state.turns.push(turn);
    renderUserTurn(turn);
    state.attachments = [];
    renderAttachments();
    prompt.value = "";
    autosize();
    retireToolReplies();
    runAssistant();
  }

  function renderUserTurn(turn) {
    const node = el("div", "turn turn-user");
    if (turn.attachments.length) {
      const list = el("div", "turn-attachments");
      for (const attachment of turn.attachments) list.append(attachmentChip(attachment));
      node.append(list);
    }
    if (turn.text) node.append(el("div", "bubble", turn.text));
    appendTurn(node);
  }

  function renderToolTurn(turn) {
    const node = el("div", "turn turn-user");
    for (const result of turn.results) {
      const bubble = el("div", "bubble tool-result");
      bubble.append(el("span", "bubble-label", `Result for ${result.name}`), document.createTextNode(result.content));
      node.append(bubble);
    }
    appendTurn(node);
  }

  function appendTurn(node) {
    byId("pg-empty").hidden = true;
    byId("transcript").append(node);
    state.stickToEnd = true;
    scrollToEnd(true);
  }

  // A new message instead of tool results: those calls go unanswered, so they leave the conversation.
  function retireToolReplies() {
    for (const turn of state.turns) {
      if (turn.awaitingTools) {
        turn.awaitingTools = false;
        turn.abandoned = true;
      }
    }
    for (const form of byId("transcript").querySelectorAll(".tool-reply-form")) form.remove();
  }

  async function runAssistant() {
    let request;
    try {
      request = buildRequest();
    } catch (error) {
      toast(error.message, "error");
      return;
    }
    const turn = {
      role: "assistant",
      model: state.model,
      content: "",
      reasoning: "",
      details: [],
      calls: new Map(),
      finish: "",
      usage: null,
      route: "",
      attempts: 1,
      started: performance.now(),
      first: null,
      reasoningEnded: null,
      ended: null,
      error: "",
      stopped: false,
    };
    for (const stale of byId("transcript").querySelectorAll(".retry")) stale.remove();
    state.turns.push(turn);
    turn.node = createAssistantNode(turn);
    appendTurn(turn.node.root);
    const controller = new AbortController();
    state.running = { controller, turn };
    updateSendButton();
    const headers = apiHeaders({ "content-type": "application/json" });
    const upstream = byId("opt-upstream-key").value;
    if (upstream) headers["x-upstream-api-key"] = upstream;
    try {
      let response;
      try {
        response = await fetch("/v1/chat/completions", { method: "POST", headers, body: JSON.stringify(request), signal: controller.signal, cache: "no-store", credentials: "same-origin" });
      } catch (error) {
        if (error.name === "AbortError") throw error;
        throw new RequestError("The router isn’t reachable.", 0, "network");
      }
      const provider = response.headers.get("x-haarness-provider");
      const model = response.headers.get("x-haarness-model");
      if (provider && model) turn.route = `${provider}/${model}`;
      turn.attempts = Number(response.headers.get("x-haarness-attempts")) || 1;
      if (!response.ok) throw failure(response.status, await readPayload(response));
      if (request.stream) await readStream(response, turn);
      else applyCompletion(await response.json(), turn);
    } catch (error) {
      if (error.name === "AbortError") turn.stopped = true;
      else turn.error = plainError(error);
      if (error.status === 401 && keyRequired() && routerKey()) {
        state.refusedKey = routerKey();
        setMetricsStatus("rejected");
        renderAccessNotice(byId("pg-key-notice"), "playground");
      }
    } finally {
      turn.ended = performance.now();
      state.running = null;
      finishAssistant(turn);
      updateSendButton();
      recordActivity(turn);
      schedule("metrics-soon", () => loadMetrics(false), 400);
      if (!NARROW.matches && state.view === "playground") byId("prompt").focus({ preventScroll: true });
    }
  }

  function plainError(error) {
    const status = error.status || 0;
    const message = error.message || "";
    if (!status) return message || "The router isn’t reachable.";
    if (status === 401) return keyRequired() ? "The router didn’t accept this tab’s key." : sentence(message);
    if (status === 413) return "This request is larger than the router accepts. Try a smaller attachment.";
    if (status === 429) return error.code === "router_overloaded" ? "The router is at its limit of requests at once. Try again in a moment." : `The provider is limiting requests. Try again in a moment. (${sentence(message)})`;
    if (status >= 500) return `The provider couldn’t answer: ${sentence(message.charAt(0).toLowerCase() + message.slice(1))}`;
    return sentence(message);
  }

  function createAssistantNode(turn) {
    const root = el("div", "turn turn-assistant");
    const waiting = el("div", "waiting");
    waiting.append(el("span", "shimmer", "Waiting for the first token…"));
    const answer = el("div", "answer");
    const text = document.createTextNode("");
    answer.append(text);
    const tools = el("div", "tool-calls");
    const meta = el("div", "turn-meta");
    root.append(waiting, answer, tools, meta);
    return { root, waiting, answer, text, tools, meta, reasoning: null, reasoningText: null, reasoningSummary: null, callNodes: new Map(), turn };
  }

  function markFirst(turn) {
    if (turn.first === null) turn.first = performance.now();
    turn.node.waiting.remove();
  }

  function ensureReasoning(turn) {
    const node = turn.node;
    if (node.reasoning) return node;
    const details = el("details", "reasoning");
    const summary = el("summary");
    summary.append(icon("chevron"));
    node.reasoningSummary = el("span", "shimmer", "Thinking…");
    summary.append(node.reasoningSummary);
    node.reasoningText = el("div", "reasoning-text");
    details.append(summary, node.reasoningText);
    node.reasoning = details;
    node.root.insertBefore(details, node.answer);
    return node;
  }

  function endReasoning(turn) {
    if (!turn.node.reasoning || turn.reasoningEnded !== null) return;
    turn.reasoningEnded = performance.now();
    const summary = turn.node.reasoningSummary;
    summary.className = "";
    const seconds = (turn.reasoningEnded - (turn.reasoningStarted ?? turn.started)) / 1000;
    summary.textContent = `Thought for ${seconds < 10 ? seconds.toFixed(1) : Math.round(seconds)} s`;
  }

  function addReasoning(turn, text) {
    markFirst(turn);
    if (turn.reasoningStarted === undefined) turn.reasoningStarted = performance.now();
    turn.reasoning += text;
    ensureReasoning(turn).reasoningText.textContent = turn.reasoning;
  }

  // Opaque reasoning (signatures, encrypted items) has nothing to show; it is kept to send back with the
  // conversation, which some providers need to continue after a tool call.
  function addDetail(turn, incoming) {
    // Same identity as the router's own merge: a later piece extends the text, summary, data or signature.
    const existing = turn.details.find((detail) => detail.type === incoming.type && detail.index === incoming.index && detail.id === incoming.id);
    if (!existing) {
      turn.details.push({ ...incoming });
      return;
    }
    for (const [key, value] of Object.entries(incoming)) {
      if (["text", "summary", "data", "signature"].includes(key) && typeof value === "string" && typeof existing[key] === "string") existing[key] += value;
      else existing[key] = value;
    }
  }

  function addText(turn, text) {
    markFirst(turn);
    endReasoning(turn);
    turn.content += text;
    turn.node.text.appendData(text);
  }

  function addToolDelta(turn, call) {
    markFirst(turn);
    endReasoning(turn);
    const index = call.index || 0;
    const existing = turn.calls.get(index) || { id: "", type: call.type || "function", name: "", arguments: "" };
    existing.id = call.id || existing.id;
    if (call.type) existing.type = call.type;
    const source = call.function || call.custom || {};
    existing.name = source.name || existing.name;
    existing.arguments += source.arguments ?? source.input ?? "";
    turn.calls.set(index, existing);
    renderToolCall(turn, index, existing);
  }

  function renderToolCall(turn, index, call) {
    let node = turn.node.callNodes.get(index);
    if (!node) {
      const card = el("div", "tool-call");
      const head = el("div", "tool-call-head");
      const name = el("code");
      head.append(icon("tool"), el("span", "", "Called"), name);
      const args = el("pre", "tool-args");
      card.append(head, args);
      turn.node.tools.append(card);
      node = { card, name, args };
      turn.node.callNodes.set(index, node);
    }
    node.name.textContent = call.name || "a tool";
    node.args.textContent = call.arguments;
  }

  function applyCompletion(payload, turn) {
    if (payload?.error) throw new RequestError(payload.error.message || "The request failed.", 502, payload.error.code);
    const choice = payload?.choices?.[0] || {};
    const message = choice.message || {};
    const reasoning = message.reasoning_content || message.reasoning;
    if (typeof reasoning === "string" && reasoning) addReasoning(turn, reasoning);
    for (const detail of message.reasoning_details || []) addDetail(turn, detail);
    if (typeof message.content === "string" && message.content) addText(turn, message.content);
    else if (Array.isArray(message.content)) addText(turn, message.content.map((part) => part.text || "").join(""));
    (message.tool_calls || []).forEach((call, index) => addToolDelta(turn, { ...call, index }));
    turn.finish = choice.finish_reason || "";
    turn.usage = payload.usage || null;
    if (turn.first !== null) turn.first = null;
  }

  async function readStream(response, turn) {
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    for (;;) {
      const { value, done } = await reader.read();
      buffer += decoder.decode(value || new Uint8Array(), { stream: !done });
      let boundary = buffer.search(/\r?\n\r?\n/);
      while (boundary >= 0) {
        const frame = buffer.slice(0, boundary);
        buffer = buffer.slice(boundary).replace(/^\r?\n\r?\n/, "");
        const data = frame
          .split(/\r?\n/)
          .filter((line) => line.startsWith("data:"))
          .map((line) => line.slice(5).trimStart())
          .join("\n");
        if (data === "[DONE]") return;
        if (data) applyChunk(JSON.parse(data), turn);
        boundary = buffer.search(/\r?\n\r?\n/);
      }
      if (done) return;
    }
  }

  function applyChunk(payload, turn) {
    if (payload.error) throw new RequestError(payload.error.message || "The stream failed.", 502, payload.error.code);
    state.stickToEnd = nearBottom();
    if (payload.usage) turn.usage = payload.usage;
    for (const choice of payload.choices || []) {
      if ((choice.index || 0) !== 0) continue;
      const delta = choice.delta || {};
      const reasoning = delta.reasoning_content ?? delta.reasoning;
      if (typeof reasoning === "string" && reasoning) addReasoning(turn, reasoning);
      for (const detail of delta.reasoning_details || []) addDetail(turn, detail);
      if (typeof delta.content === "string" && delta.content) addText(turn, delta.content);
      for (const call of delta.tool_calls || []) addToolDelta(turn, call);
      if (choice.finish_reason) turn.finish = choice.finish_reason;
    }
    scrollToEnd();
  }

  function finishAssistant(turn) {
    const node = turn.node;
    node.waiting.remove();
    endReasoning(turn);
    for (const [index, call] of turn.calls) {
      try {
        call.pretty = JSON.stringify(JSON.parse(call.arguments || "{}"), null, 2);
      } catch {
        call.pretty = call.arguments;
      }
      renderToolCall(turn, index, { ...call, arguments: call.pretty });
    }
    // Structured output arrives as JSON text; set it as code so it reads.
    if (turn.content && state.options.schema.trim()) {
      try {
        node.text.data = JSON.stringify(JSON.parse(turn.content), null, 2);
        node.answer.classList.add("json");
      } catch {
        // Not JSON after all; leave the text as it came.
      }
    }
    if (turn.error) {
      const box = el("div", "turn-error");
      box.append(icon("alert"));
      const text = el("div");
      text.append(el("span", "", turn.error));
      box.append(text);
      const retry = button("Try again", "ghost retry", () => {
        const index = state.turns.indexOf(turn);
        if (index >= 0) state.turns.splice(index, 1);
        node.root.remove();
        runAssistant();
      });
      box.append(retry);
      node.root.append(box);
      if (!turn.content && !turn.calls.size && !turn.reasoning) node.answer.remove();
    }
    renderMeta(turn);
    const calls = [...turn.calls.values()].filter((call) => call.name && call.id);
    if (!turn.error && !turn.stopped && calls.length && state.turns[state.turns.length - 1] === turn) renderToolReply(turn, calls);
    scrollToEnd();
  }

  function renderMeta(turn) {
    const meta = turn.node.meta;
    meta.replaceChildren();
    if (turn.content) meta.append(iconButton("copy", "Copy the answer", (event) => copy(turn.content, event.currentTarget)));
    const route = turn.route && turn.route !== turn.model ? turn.route : turn.model;
    meta.append(el("span", "route", route));
    if (turn.usage) {
      const usage = turn.usage;
      const reasoning = usage.completion_tokens_details?.reasoning_tokens;
      meta.append(el("span", "", `${count(usage.prompt_tokens)} in · ${count(usage.completion_tokens)} out${reasoning ? ` (${count(reasoning)} reasoning)` : ""}`));
    }
    if (turn.first !== null) meta.append(el("span", "", `First token ${durationText(turn.first - turn.started)}`));
    if (turn.ended !== null) meta.append(el("span", "", durationText(turn.ended - turn.started)));
    if (turn.attempts > 1) {
      const attempts = el("span", "", `${turn.attempts} attempts`);
      attempts.title = "Earlier targets of the alias failed before answering";
      meta.append(attempts);
    }
    if (turn.stopped) meta.append(el("span", "", "Stopped"));
    else if (turn.finish === "length") meta.append(el("span", "", "Cut off at the token limit"));
    else if (turn.finish === "content_filter") meta.append(el("span", "", "Filtered by the provider"));
  }

  function renderToolReply(turn, calls) {
    const form = el("form", "tool-reply-form");
    const fields = calls.map((call) => {
      const row = el("div", "tool-reply");
      const input = el("input");
      input.placeholder = `Result for ${call.name}`;
      input.setAttribute("aria-label", `Result for ${call.name}`);
      input.autocomplete = "off";
      input.spellcheck = false;
      row.append(input);
      return { row, input, call };
    });
    const send = el("button", "primary", calls.length > 1 ? "Send results" : "Send result");
    send.type = "submit";
    fields[fields.length - 1].row.append(send);
    form.append(...fields.map((field) => field.row));
    turn.awaitingTools = true;
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      if (state.running) return;
      turn.awaitingTools = false;
      const results = fields.map(({ input, call }) => ({ id: call.id, name: call.name, content: input.value.trim() || "{}" }));
      form.remove();
      const toolTurn = { role: "tool", results };
      state.turns.push(toolTurn);
      renderToolTurn(toolTurn);
      runAssistant();
    });
    turn.node.root.append(form);
    if (!NARROW.matches) window.setTimeout(() => fields[0].input.focus({ preventScroll: true }), 0);
  }

  function recordActivity(turn) {
    state.activity.unshift({
      at: new Date(),
      model: turn.model,
      route: turn.route,
      status: turn.error ? "error" : turn.stopped ? "stopped" : "ok",
      error: turn.error,
      first: turn.first === null ? null : turn.first - turn.started,
      total: turn.ended - turn.started,
      tokens: turn.usage ? turn.usage.total_tokens ?? (turn.usage.prompt_tokens || 0) + (turn.usage.completion_tokens || 0) : null,
    });
    state.activity.length = Math.min(state.activity.length, 20);
    if (state.view === "observability") renderRecent();
  }

  function stopAnswer() {
    state.running?.controller.abort();
  }

  function newChat() {
    stopAnswer();
    state.turns = [];
    const transcript = byId("transcript");
    transcript.replaceChildren(byId("pg-empty"));
    byId("pg-empty").hidden = false;
    renderPlaygroundState();
    byId("prompt").focus();
  }

  function initPlayground() {
    const prompt = byId("prompt");
    byId("composer").addEventListener("submit", (event) => {
      event.preventDefault();
      if (state.running) stopAnswer();
      else submitPrompt();
    });
    prompt.addEventListener("input", () => {
      autosize();
      updateSendButton();
    });
    prompt.addEventListener("keydown", (event) => {
      if (event.key === "Enter" && !event.shiftKey && !event.isComposing && !NARROW.matches) {
        event.preventDefault();
        if (!state.running) submitPrompt();
      }
    });
    document.addEventListener("keydown", (event) => {
      if (event.key !== "Escape" || state.view !== "playground") return;
      if (state.running) {
        event.preventDefault();
        stopAnswer();
      } else if (!byId("pg-options").hidden && NARROW.matches) setOptionsOpen(false);
    });
    byId("transcript").addEventListener("scroll", () => {
      state.stickToEnd = nearBottom();
    });
    byId("new-chat").addEventListener("click", newChat);
    const observer = new MutationObserver(updateSendButton);
    observer.observe(byId("attachments"), { childList: true });
    autosize();
    updateSendButton();
  }

  // Copy buttons with a fixed target ------------------------------------------------------------------------
  function initCopy() {
    const sources = {
      endpoint: () => `${origin()}/v1`,
      connect: () => byId("connect-code").textContent,
      metrics: () => state.metricsRaw,
    };
    for (const trigger of document.querySelectorAll("[data-copy]")) {
      trigger.addEventListener("click", () => copy(sources[trigger.dataset.copy](), trigger));
    }
  }

  function start() {
    initTheme();
    initNavigation();
    initConnect();
    initCopy();
    initKeys();
    initSettings();
    initObservability();
    initOptions();
    initPicker();
    initAttachments();
    initPlayground();
    byId("model-search").addEventListener("input", renderModels);
    renderEndpoint();
    window.addEventListener("resize", () => schedule("resize", renderActivity, 120));
    NARROW.addEventListener("change", () => {
      if (NARROW.matches && !byId("pg-options").hidden) setOptionsOpen(false, false);
      else if (!NARROW.matches && state.options.open) setOptionsOpen(true, false);
    });
    loadOverview().then(() => loadMetrics(true));
  }
})();
