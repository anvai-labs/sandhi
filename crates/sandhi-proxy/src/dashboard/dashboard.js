"use strict";

const fmt = n => typeof n === "number" && Number.isFinite(n) ? n.toLocaleString() : "—";
const esc = s => String(s ?? "").replace(/[&<>"']/g, c =>
  ({ "&":"&amp;", "<":"&lt;", ">":"&gt;", '"':"&quot;", "'":"&#39;" }[c]));
const orDash = s => (s === null || s === undefined || s === "") ? "—" : esc(s);
// No latency is "—", never "0 ms": a call that never reported a duration is unknown, not fast.
const lat = l => (!l || !l.samples) ? "—"
  : `${fmt(l.p50_ms)} / ${fmt(l.p95_ms)} ms <span class="muted">(n=${fmt(l.samples)})</span>`;

// Reporting coverage is a call count, not a cache hit rate or a token discount. Legacy
// aggregates have no observation evidence, including when their numeric cache count is > 0.
function cacheCoverage(row) {
  const c = row.cache_read_coverage;
  const fields = ["reported", "absent", "malformed", "unsupported", "unknown"];
  if (!c || !Number.isSafeInteger(row.calls) || row.calls < 0
      || fields.some(k => !Number.isSafeInteger(c[k]) || c[k] < 0)
      || fields.reduce((n, k) => n + c[k], 0) !== row.calls) return null;
  return c;
}
function cacheRead(row) {
  const c = cacheCoverage(row);
  if (!c) return "unknown";
  if (!row.calls) return "—";
  if (c.reported === row.calls) return fmt(row.cache_read_tokens);
  if (c.absent === row.calls) return "not reported";
  if (c.malformed === row.calls) return "malformed";
  if (c.unsupported === row.calls) return "unsupported";
  if (c.unknown === row.calls) return "unknown";
  return `${fmt(row.cache_read_tokens)} <span class="muted">(mixed reporting)</span>`;
}
function cacheCoverageLabel(row) {
  const c = cacheCoverage(row);
  if (!c) return `unknown / ${fmt(row.calls)} calls`;
  return `${fmt(c.reported)} / ${fmt(row.calls)} reported`
    + ` <span class="muted">(not reported ${fmt(c.absent)}; malformed ${fmt(c.malformed)};`
    + ` unsupported ${fmt(c.unsupported)}; unknown ${fmt(c.unknown)})</span>`;
}

// The token lives only in memory. Editing/clearing it cancels reads and hides all prior
// privileged data immediately. Old responses cannot repopulate a new authentication session.
const tokenEl = document.getElementById("admin-token");
const dotEl = document.getElementById("token-dot");
let activeToken = "";
let authMode = "loading";
let inferenceAuthMode = "loading";
let starterPermissions = [];
let ssoSession = null;
let expiryTimer;
const actionPermissions = { lookupRun: "read", lookupEvidence: "diagnostics", ackAlert: "operate", setBudget: "operate" };
function permits(permission) {
  if (authMode === "none") return starterPermissions.includes(permission);
  return authMode === "tokens" ? !!activeToken : !!ssoSession?.permissions?.includes(permission);
}
function signedIn() {
  if (authMode === "none") return starterPermissions.includes("read");
  return authMode === "tokens" ? !!activeToken : !!ssoSession?.subject;
}

let authRevision = 0;
const pending = new Set();
const panels = new Map();
let usageData = null;
let attributionQuery = "";
try { sessionStorage.removeItem("sandhi_admin_token"); } catch { /* Storage may be disabled. */ }
function refreshTokenState() {
  const on = signedIn();
  dotEl.classList.toggle("on", on);
  dotEl.title = authMode === "none" ? "Shared starter access; authentication is off"
    : on ? "Each request is authorized by the server" : "No operator credential supplied";
  document.querySelectorAll("[data-needs-token]").forEach(b => b.disabled = !permits(actionPermissions[b.dataset.action] || "admin") || b.dataset.revoked === "true"
    || (b.dataset.action === "mintVkey" && inferenceAuthMode !== "tokens"));
  document.getElementById("vkey-auth-status").textContent = inferenceAuthMode === "tokens"
    ? "Ordinary virtual keys authenticate inference in token mode."
    : inferenceAuthMode === "oidc"
      ? "Ordinary virtual-key authentication is inactive in OIDC mode. Use OIDC access tokens; delegated keys require an explicit policy."
      : inferenceAuthMode === "none"
        ? "Inference authentication is off. Ordinary virtual keys do not restrict the shared starter."
        : "Loading the inference access profile.";
  document.getElementById("token-form").hidden = authMode !== "tokens";
  document.getElementById("sso-box").hidden = authMode !== "oidc";
  document.getElementById("sso-login").hidden = !!ssoSession?.subject;
  document.getElementById("sso-logout").hidden = !ssoSession?.subject;
  if (authMode === "oidc") document.getElementById("auth-status").textContent = ssoSession?.subject
    ? `${ssoSession.subject} — ${ssoSession.role}` : "Sign in with SSO to view protected data.";
  if (authMode === "none") document.getElementById("auth-status").textContent =
    "Authentication is off. Requests share one starter identity and budget.";
  else if (authMode === "tokens" && inferenceAuthMode === "none") document.getElementById("auth-status").textContent =
    "Inference needs no authentication. Operator actions require the configured admin token.";
  document.getElementById("workspace-role").textContent = on
    ? (authMode === "none" ? "Shared starter access" : authMode === "oidc" ? ssoSession.role : "Token supplied")
    : "No operator session";
}
function resetAuth(token) {
  activeToken = token;
  authRevision++;
  for (const controller of pending) controller.abort();
  pending.clear();
  panels.clear();
  pagers.clear();
  usageData = null;
  attributionQuery = "";
  for (const id of ["execution", "usage", "keys", "budgets", "alerts", "config", "policy", "run-tree", "backends", "routes", "topology", "request-evidence"]) {
    const el = document.getElementById(id);
    el.replaceChildren();
    el.dataset.state = authMode === "none" ? "loading" : "locked";
    el.setAttribute("aria-busy", "false");
    const message = document.createElement("p");
    message.textContent = authMode === "none" ? "Refresh to load shared starter data."
      : authMode === "oidc" ? "Sign in with SSO to view protected data." : "Select Use token to load protected data, or Refresh for public data.";
    el.append(message);
  }
  document.querySelectorAll("[data-sensitive]").forEach(el => el.replaceChildren());
  document.getElementById("c-secret").value = "";
  document.getElementById("run-id").value = "";
  document.getElementById("evidence-value").value = "";
  document.getElementById("toasts").replaceChildren();
  document.getElementById("auth-status").textContent = token ? "Token supplied." : "No token supplied.";
  refreshTokenState();
}
tokenEl.addEventListener("input", () => resetAuth(""));
document.getElementById("token-form").addEventListener("submit", event => {
  event.preventDefault();
  resetAuth(tokenEl.value.trim());
  refreshAll();
});

function toast(msg, ok) {
  const t = document.createElement("div");
  t.className = "toast " + (ok ? "ok" : "err");
  t.textContent = msg;
  document.getElementById("toasts").appendChild(t);
  setTimeout(() => t.remove(), 4000);
}

class ApiError extends Error {
  constructor(status, message, data = null) { super(message); this.status = status; this.data = data; }
}

// Reads and writes share exactly the same authentication/error path. The response never
// controls HTML, redirect destinations, or executable event-handler strings.
async function requestJSON(method, path, body) {
  const revision = authRevision;
  const controller = new AbortController();
  pending.add(controller);
  const timeout = setTimeout(() => controller.abort(), 15000);
  try {
    const resp = await fetch(path, {
      method, headers: {
        ...(authMode === "tokens" && activeToken ? { "Authorization": "Bearer " + activeToken } : {}),
        ...(authMode === "oidc" && ssoSession?.csrf && method !== "GET" ? { "X-Sandhi-CSRF": ssoSession.csrf } : {}),
        ...(body === undefined ? {} : { "Content-Type": "application/json" }),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
      credentials: "same-origin", cache: "no-store", redirect: "error", signal: controller.signal,
    });
    const data = await resp.json().catch(() => null);
    if (revision !== authRevision) throw new DOMException("Authentication changed", "AbortError");
    if (!resp.ok) {
      if (resp.status === 401 && authMode === "oidc") { ssoSession = null; resetAuth(""); }
      const message = typeof data?.error === "string" ? data.error : data?.error?.message;
      throw new ApiError(resp.status, message || `Request failed (${resp.status})`, data);
    }
    if (!data || typeof data !== "object") throw new ApiError(502, "Invalid response from server");
    return data;
  } finally {
    clearTimeout(timeout);
    pending.delete(controller);
  }
}

function failureMessage(error) {
  if (error.status === 401) return authMode === "oidc" ? "Session expired or missing. Sign in with SSO." : "Authentication required. Enter a valid admin token and select Use token.";
  if (error.status === 403) return "Access denied. This operation is not available with the current access configuration.";
  if (error.status === 404) return "Not configured or not found. " + error.message;
  return "Data unavailable. Retry with Refresh. " + (error.name === "AbortError" ? "Request timed out." : error.message);
}

async function adminCall(method, path, body, allowPartial = false) {
  if (!signedIn()) { toast(authMode === "oidc" ? "Sign in with SSO first." : "Enter the admin token and select Use token first.", false); return null; }
  const revision = authRevision;
  try { return await requestJSON(method, path, body); }
  catch (error) {
    if (revision === authRevision) toast(failureMessage(error), false);
    if (revision === authRevision && allowPartial && error.data?.ok === false) return error.data;
    return null;
  }
}

async function loadPanel(id, path, render, permission = null, options = {}) {
  const el = document.getElementById(id);
  const revision = authRevision;
  const previous = panels.get(id);
  const ticket = { html: previous?.html, updated: previous?.updated };
  panels.set(id, ticket);
  el.setAttribute("aria-busy", "true");
  el.dataset.state = "loading";
  el.textContent = "Loading…";
  try {
    if (permission && !permits(permission)) throw new ApiError(signedIn() ? 403 : 401, "Role does not allow this operation");
    const data = await requestJSON(options.method || "GET", path, options.body);
    if (revision !== authRevision || panels.get(id) !== ticket) return;
    // A renderer may join a bounded readiness probe. Recheck identity and panel
    // generation after every await before publishing protected data.
    const html = await render(data);
    if (revision !== authRevision || panels.get(id) !== ticket) return;
    ticket.html = html;
    ticket.updated = new Date().toLocaleTimeString();
    el.innerHTML = ticket.html;
    el.dataset.state = "ready";
    // Per-panel "Loaded at" lines are gone: the single header status line (#refresh-status,
    // set by refreshAll) carries the dashboard-wide refresh time. ticket.updated survives
    // only for the stale-data error message below.
  } catch (error) {
    if (revision !== authRevision || panels.get(id) !== ticket) return;
    const stale = options.retainStale !== false && ticket.html && error.status !== 401 && error.status !== 403 && error.status !== 404;
    el.innerHTML = stale ? ticket.html : "";
    el.dataset.state = stale ? "stale" : error.status === 401 ? "locked" : error.status === 403 ? "forbidden" : error.status === 404 ? "unconfigured" : "unavailable";
    if (!stale) { ticket.html = null; ticket.updated = null; }
    const message = document.createElement("p");
    message.className = "callout err";
    message.setAttribute("role", "status");
    message.textContent = failureMessage(error) + (stale ? " Showing stale data loaded at " + ticket.updated + "." : "");
    el.prepend(message);
  } finally {
    if (revision === authRevision && panels.get(id) === ticket) {
      el.setAttribute("aria-busy", "false");
      refreshTokenState();
    }
  }
}

function requireFields(data, arrays, objects = []) {
  if (arrays.some(key => !Array.isArray(data[key])) || objects.some(key => !data[key] || typeof data[key] !== "object")) {
    throw new ApiError(502, "Incomplete response from server");
  }
}

// Client-side pagination for the flood-prone tables. All rows are already in memory; the
// pager only slices what is painted. State is keyed by table id and re-created on every
// panel render, so a Refresh naturally lands back on page 1.
const PAGE_SIZE = 25;
const pagers = new Map();

function tableScroll(ariaLabel, head, body, wide = false, describedBy = null, pagerBodyId = null) {
  return `<div class="table-scroll${wide ? " wide" : ""}" role="region" aria-label="${esc(ariaLabel)}"${describedBy ? ` aria-describedby="${esc(describedBy)}"` : ""} tabindex="0"><table><thead>${head}</thead><tbody${pagerBodyId ? ` data-pager-body="${esc(pagerBodyId)}"` : ""}>${body}</tbody></table></div>`;
}

function pagedTable(id, ariaLabel, head, rows, rowHtml, empty, noun, wide = false, describedBy = null) {
  if (rows.length <= PAGE_SIZE) {
    return tableScroll(ariaLabel, head, rows.map(rowHtml).join("") || empty, wide, describedBy);
  }
  const state = { ariaLabel, head, rows, rowHtml, empty, noun, page: 0 };
  pagers.set(id, state);
  return tableScroll(ariaLabel, head, pageRows(state), wide, describedBy, id) + pagerChrome(id, state);
}

function pageRows(state) {
  const start = state.page * PAGE_SIZE;
  return state.rows.slice(start, start + PAGE_SIZE).map(state.rowHtml).join("");
}

function pagerChrome(id, state) {
  const pages = Math.ceil(state.rows.length / PAGE_SIZE);
  const start = state.page * PAGE_SIZE + 1;
  const end = Math.min(state.rows.length, start + PAGE_SIZE - 1);
  const btn = (label, target, disabled, aria) =>
    `<button class="btn" type="button" data-action="pager" data-pager="${esc(id)}" data-target="${target}"${disabled ? " disabled" : ""} aria-label="${aria}">${label}</button>`;
  return `<div class="pager" data-pager-bar="${esc(id)}"><span class="pager-count muted">`
    + `${fmt(start)}–${fmt(end)} of ${fmt(state.rows.length)} ${esc(state.noun)}</span>`
    + `<div class="pager-controls">${btn("«", 0, state.page === 0, "First page")}`
    + `${btn("‹", state.page - 1, state.page === 0, "Previous page")}`
    + `<span class="pager-status" aria-live="polite">page ${state.page + 1} of ${pages}</span>`
    + `${btn("›", state.page + 1, state.page >= pages - 1, "Next page")}`
    + `${btn("»", pages - 1, state.page >= pages - 1, "Last page")}</div></div>`;
}

function paintPager(id) {
  const state = pagers.get(id);
  if (!state) return;
  const body = document.querySelector(`[data-pager-body="${CSS.escape(id)}"]`);
  if (body) body.innerHTML = pageRows(state);
  const bar = document.querySelector(`[data-pager-bar="${CSS.escape(id)}"]`);
  if (bar) bar.outerHTML = pagerChrome(id, state);
}

const ATTRIBUTION_HEAD = `<tr><th>key</th><th class="num">calls</th>`
  + `<th class="num">fresh</th><th class="num">output</th><th class="num">cache write</th>`
  + `<th class="num">cache read</th><th>cache reporting coverage</th><th class="num" title="Normalized observed tokens under recorded or historical reasoning-overlap conventions. Not a frozen weighted charge or currency amount.">observed units`
  + `</th><th class="num" title="p50 / p95 milliseconds over the sampled calls that reported a `
  + `duration — approximate by design; tokens above are exact">latency</th></tr>`;

function attributionRow(r) {
  return `<tr><td>${esc(r.key)}</td><td class="num">${fmt(r.calls)}</td>`
    + `<td class="num">${fmt(r.tokens_in)}</td><td class="num">${fmt(r.tokens_out)}</td>`
    + `<td class="num">${fmt(r.cache_creation_tokens)}</td><td class="num">${cacheRead(r)}</td>`
    + `<td>${cacheCoverageLabel(r)}</td>`
    + `<td class="num">${fmt(r.billable_tokens)}</td>`
    + `<td class="num">${lat(r.latency)}</td></tr>`;
}

function tbl(id, title, noun, rows) {
  return `<h3>${esc(title)}</h3>`
    + pagedTable(id, `${title} attribution`, ATTRIBUTION_HEAD, rows, attributionRow,
      `<tr><td colspan=9>${attributionQuery ? "No matching rows" : "no data yet"}</td></tr>`, noun, true, "table-help");
}

function loadUsage() {
  return loadPanel("usage", "/dashboard/api/usage", d => {
    requireFields(d, ["by_subject", "by_group", "by_provider", "by_model"], ["total"]);
    usageData = d;
    const t = d.total;
    return '<div class="cards" id="cards">' +
      [["calls", fmt(t.calls)], ["observed units", fmt(t.billable_tokens)], ["fresh input", fmt(t.tokens_in)], ["output tokens", fmt(t.tokens_out)],
       ["cache read", cacheRead(t)], ["cache reporting coverage", cacheCoverageLabel(t)],
       ["latency p50/p95", lat(t.latency)]]
      .map(([l, n]) => `<div class="card${l === "cache reporting coverage" || l === "latency p50/p95" ? " card-detail" : ""}"><div class="n">${n}</div><div class="l">${l}</div></div>`).join("")
      + `</div>${tokenSplit(t)}<p class="muted">Cache read is reported cache-read input (origin or explicit caller observation), not proof of backend reuse; cache write is an explicit accounting category, not proof of new KV state. Counts are neutral tokens, not prices or savings. Reporting coverage counts calls in each displayed aggregate; it is not a cache hit rate. Historical rows retain their existing reasoning-overlap fallback. Aggregate observations do not establish per-request settlement or qualified source usage.</p><h3>Attribution</h3>
      <div class="table-toolbar"><label for="usage-filter">Filter attribution</label><input id="usage-filter" type="search" value="${esc(attributionQuery)}" placeholder="Find a user, team, provider or model" maxlength="256"><span class="muted">Totals include all calls.</span></div>
      <p id="table-help" class="muted">On narrow screens, scroll each table horizontally to see every column. Keyboard users can focus a table and use the arrow keys.</p><div id="tables">`
      + usageTables(d)
      + `</div>`;
  });
}

function usageTables(d) {
  const filter = rows => rows.filter(row => String(row.key).toLocaleLowerCase().includes(attributionQuery.toLocaleLowerCase()));
  return tbl("attr-subject", "By user (subject)", "subjects", filter(d.by_subject))
    + tbl("attr-group", "By team (group)", "groups", filter(d.by_group))
    + tbl("attr-provider", "By provider", "providers", filter(d.by_provider))
    + tbl("attr-model", "By model", "models", filter(d.by_model));
}

function tokenSplit(t) {
  const coverage = cacheCoverage(t);
  const parts = [t.tokens_in, t.cache_read_tokens, t.cache_creation_tokens];
  const total = parts.reduce((sum, count) => sum + count, 0);
  if (!coverage || coverage.reported !== t.calls || !t.calls
      || parts.some(count => !Number.isSafeInteger(count) || count < 0) || !Number.isSafeInteger(total) || !total) {
    return '<div class="token-insight"><h3>Input token composition</h3><p class="muted">A comparable split is available when every call reports cache-read usage. Missing measurements are not treated as zero or as cache savings.</p></div>';
  }
  const [fresh, read, write] = parts.map(count => count / total * 1000);
  return `<div class="token-insight"><div class="insight-heading"><h3>Input token composition</h3><span class="badge back-leg">${(t.cache_read_tokens / total * 100).toFixed(1)}% cache-read share</span></div>
    <svg class="token-bar" viewBox="0 0 1000 24" preserveAspectRatio="none" role="img" aria-label="${fmt(t.tokens_in)} fresh, ${fmt(t.cache_read_tokens)} cache-read and ${fmt(t.cache_creation_tokens)} cache-write input tokens">
      <rect class="token-fresh" width="${fresh}" height="24"/><rect class="token-read" x="${fresh}" width="${read}" height="24"/><rect class="token-write" x="${fresh + read}" width="${write}" height="24"/></svg>
    <div class="token-legend"><span><i class="token-fresh"></i>Fresh <strong>${fmt(t.tokens_in)}</strong></span><span><i class="token-read"></i>Cache read <strong>${fmt(t.cache_read_tokens)}</strong></span><span><i class="token-write"></i>Cache write <strong>${fmt(t.cache_creation_tokens)}</strong></span><span>Total input <strong>${fmt(total)}</strong></span></div>
    <p class="hint">Read share describes input accounting, not a percentage cost saving. Cache-write counts are retained separately; this chart does not establish write-field reporting coverage. Output and reasoning follow their own inclusion convention.</p></div>`;
}

// Readiness is a public, plain-text lifecycle probe, not provider health. Its
// result never grants authority when the authenticated accounting contract is absent.
async function executionReadiness() {
  const revision = authRevision;
  const controller = new AbortController();
  pending.add(controller);
  const timeout = setTimeout(() => controller.abort(), 5000);
  try {
    const response = await fetch("/readyz", {
      credentials: "same-origin", cache: "no-store", redirect: "error", signal: controller.signal,
    });
    const text = await response.text();
    if (revision !== authRevision) throw new DOMException("Authentication changed", "AbortError");
    if (response.status === 200 && text.trim() === "ready") return "ready";
    if (response.status === 503 && text.trim() === "draining") return "draining";
    return "unavailable";
  } finally {
    clearTimeout(timeout);
    pending.delete(controller);
  }
}
async function loadExecution() {
  return loadPanel("execution", "/admin/version", async data => {
    const initialized = data.capabilities?.execution_accounting === "paired_durable_v1";
    let readiness = "unavailable";
    try { readiness = await executionReadiness(); }
    catch { /* Unknown probe state never becomes readiness; loadPanel checks auth revision. */ }
    const ready = initialized && readiness === "ready";
    const title = ready ? "Readiness confirmed" : readiness === "draining" ? "Draining" : "Readiness unavailable";
    return `<div class="execution-heading"><div><span class="eyebrow">Execution contract</span><h3>${title}</h3></div>
      <span class="badge ${ready ? "active" : "revoked"}">${ready ? "Initialized owner · ready" : "Requires attention"}</span></div>
      <dl class="execution-facts"><div><dt>Durable accounting</dt><dd>${initialized ? "Paired owner initialized" : "Unavailable — contract not confirmed"}</dd></div>
      <div><dt>Lifecycle probe</dt><dd>${readiness === "ready" ? "Ready" : readiness === "draining" ? "Draining" : "Unavailable"}</dd></div>
      <div><dt>Settlement authority</dt><dd>Original source evidence + committed receipt</dd></div></dl>
      <p class="hint">A readiness confirmation requires both facts. A failed probe does not change configured generation policy. Neither fact establishes provider health, budget capacity, cache reuse or settlement for an individual call.</p>
      <a class="docs-link" href="#flow-section" data-view-link>How admission and evidence fit together →</a>`;
  }, null, { retainStale: false });
}
function budgetAuthority(record) {
  const scope = record.upstream_budget_scope;
  return typeof scope === "string" && scope.trim() && !/[\u0000-\u001f\u007f-\u009f]/.test(scope)
    ? `<code>${esc(scope)}</code><br><span class="hint">Immutable binding</span>`
    : '<span class="muted">Unavailable</span>';
}

function backendRow(record) {
  const d = record.definition;
  if (!d || typeof d.id !== "string" || !Array.isArray(d.models) || !d.endpoint) throw new ApiError(502, "Incomplete backend inventory");
  const policy = d.cache_policy;
  const mode = policy ? policy.default_intent?.mode || "preserve_native" : "endpoint default";
  const ready = record.status === "active" && record.ready === true;
  const detail = d.models.map(model => {
    const capabilities = policy?.models?.[model.id];
    return `<li><code>${esc(model.id)}</code> — ${esc(capabilities?.profile || "undeclared")}${capabilities?.lifetimes?.length ? `; TTL ${capabilities.lifetimes.map(esc).join(", ")}` : ""}</li>`;
  }).join("");
  return `<tr><td><strong>${esc(d.id)}</strong><br><span class="muted">revision ${fmt(record.revision)}</span></td>
    <td><span class="badge ${ready ? "active" : "revoked"}">${ready ? "Available" : record.status === "revoked" ? "Revoked" : "Unavailable"}</span></td>
    <td>${orDash(d.provider)}<br><span class="hint">Configured label</span></td>
    <td>${esc(d.endpoint.endpoint_family)}<br><span class="muted">${esc(d.endpoint.endpoint_profile || "standard")}</span></td>
    <td><code>${esc(d.credential_ref)}</code><br><span class="muted">${d.base_url ? esc(d.base_url) : "Inherited from credential"}</span></td>
    <td>${budgetAuthority(record)}</td>
    <td>${fmt(d.models.length)} models<br><span class="badge back-leg">${esc(mode)}</span><details><summary>Models &amp; cache policy</summary><ul class="model-list">${detail}</ul><p class="hint">Namespace: ${esc(policy?.namespace || "provider_default")}. Declared capabilities are operator assertions, not live qualification.</p></details></td></tr>`;
}
function loadBackends() {
  return loadPanel("backends", "/admin/backends", d => {
    requireFields(d, ["backends"]);
    return pagedTable("backend-inventory", "Backend endpoints", '<tr><th>Backend</th><th>Status</th><th>Provider label</th><th>Protocol / profile</th><th>Credential / endpoint</th><th>Budget authority</th><th>Models / caching</th></tr>', d.backends, backendRow,
      '<tr><td colspan="7">No explicit backends configured. Register an endpoint with the CLI or declarative configuration.</td></tr>', "backends");
  }, "read");
}
function loadRoutes() {
  return loadPanel("routes", "/admin/routes", d => {
    requireFields(d, ["routes"]);
    return pagedTable("route-inventory", "Logical model mappings", '<tr><th>Route</th><th>Selection</th><th>Affinity</th><th>Model mappings</th></tr>', d.routes, record => {
      const route = record.definition;
      if (!route || typeof route.id !== "string" || !Array.isArray(route.models)) throw new ApiError(502, "Incomplete route inventory");
      return `<tr><td><code>route/${esc(route.id)}</code><br><span class="muted">revision ${fmt(record.revision)} · ${esc(record.status)}</span></td>
        <td>${esc(route.selection)}</td><td>${esc(route.affinity)}<br><span class="muted">${fmt(route.affinity_ttl_seconds)} seconds</span></td>
        <td>${route.models.map(model => `<details><summary>${esc(model.id)}</summary><ul class="model-list">${model.targets.map(target => `<li><code>${esc(target.backend)}</code> → <code>${esc(target.model)}</code></li>`).join("")}</ul></details>`).join("")}</td></tr>`;
    }, '<tr><td colspan="4">No logical routes configured. Fixed backend keys remain available.</td></tr>', "routes")
      + '<p class="hint">An active definition does not guarantee a compatible target for every request. Availability, model grants and session binding are checked at admission.</p>';
  }, "read");
}

// Keys: masked virtual keys + vault entries. Never a secret. Revoke/add are admin-gated.
function vkeyRow(k) {
  const status = k.revoked_at ? "revoked" : "active";
  const disabled = status === "revoked" ? "disabled" : "";
  return `<tr><td><code>${esc(k.id)}</code></td><td>${orDash(k.subject)}</td><td>${orDash(k.group)}</td>`
    + `<td><code>${esc(k.upstream_ref)}</code></td><td>${(k.models||[]).map(esc).join(", ")||'<span class="muted">any</span>'}</td>`
    + `<td><span class="badge ${status}">${status}</span></td><td>${humanExpiry(k.expires_at)}</td>`
    + `<td><button class="btn danger" data-needs-token ${disabled} data-revoked="${!!k.revoked_at}" data-action="revokeVkey" data-id="${esc(k.id)}">Revoke</button></td></tr>`;
}
function vaultRow(e) {
  return `<tr><td><code>${esc(e.credential_id)}</code></td>`
    + `<td>${esc(e.scheme)}</td><td>${orDash(e.base_url)}</td>`
    + `<td><span class="badge ${e.status === "active" ? "active" : "revoked"}">${esc(e.status)}</span></td>`
    + `<td><button class="btn danger" data-needs-token data-action="revokeCred" data-provider="${esc(e.provider)}" data-label="${esc(e.label)}">Revoke</button></td></tr>`;
}
function keysView(d) {
  const vault = (d.vault || []).map(vaultRow).join("");
  return `<h3>Virtual keys (masked — secrets are never stored)</h3>`
    + pagedTable("vkeys", "Virtual keys", `<tr><th>id</th><th>subject</th><th>group</th><th>upstream</th><th>models</th><th>status</th><th>expires</th><th></th></tr>`,
      d.virtual_keys || [], vkeyRow, '<tr><td colspan=8>no virtual keys</td></tr>', "keys", true)
    + `<h3>Provider credentials (vault metadata)</h3>`
    + tableScroll("Provider credentials", `<tr><th>credential</th><th>scheme</th><th>base url</th><th>status</th><th></th></tr>`,
      vault || '<tr><td colspan=5>no provider credentials</td></tr>');
}
function loadKeys() {
  return loadPanel("keys", "/dashboard/api/keys", d => {
    requireFields(d, ["virtual_keys", "vault"]);
    return keysView(d);
  });
}
async function revokeVkey(id) {
  if (!(await adminCall("DELETE", `/admin/vkeys/${encodeURIComponent(id)}`))) return;
  toast("Virtual key revoked", true); loadKeys();
}
async function revokeCred(provider, label) {
  const result = await adminCall("DELETE", `/admin/keys/${encodeURIComponent(provider)}/${encodeURIComponent(label)}`);
  if (!result) return;
  toast(`Credential disabled locally; secret cleanup: ${result.secret_deletion || "unknown"}. Broker grant and provider key unchanged.`, true); loadKeys();
}
async function mintVkey() {
  if (inferenceAuthMode !== "tokens") { toast("Ordinary virtual keys are available only in token mode.", false); return; }
  const models = document.getElementById("v-models").value.trim();
  const rate = document.getElementById("v-rate").value;
  const body = {
    upstream: document.getElementById("v-upstream").value.trim(),
    subject: document.getElementById("v-subject").value.trim() || null,
    group: document.getElementById("v-group").value.trim() || null,
    models: models ? models.split(",").map(s => s.trim()).filter(Boolean) : null,
    rate_limit_per_min: rate ? Number(rate) : null,
  };
  const data = await adminCall("POST", "/admin/keys/share", body);
  if (!data) return;
  document.getElementById("v-result").innerHTML =
    `<div class="callout ok">Minted — copy now, shown once: <code>${esc(data.virtual_key)}</code></div>`;
  loadKeys();
}
async function addCredential() {
  const reference = document.getElementById("c-mode").value === "reference";
  const body = {
    provider: document.getElementById("c-provider").value.trim(),
    label: document.getElementById("c-label").value.trim() || null,
    scheme: document.getElementById("c-scheme").value,
    base_url: document.getElementById("c-baseurl").value.trim() || null,
    ...(reference ? {} : { secret: document.getElementById("c-secret").value }),
  };
  const data = await adminCall("POST", reference ? "/admin/keys/reference" : "/admin/keys", body);
  if (!data) return;
  document.getElementById("c-secret").value = "";
  document.getElementById("c-result").innerHTML = `<div class="callout ok">Registered ${esc(data.credential_id || "")}</div>`;
  loadKeys();
}

document.getElementById("c-mode").addEventListener("change", () => {
  const input = document.getElementById("c-secret");
  input.value = "";
  input.disabled = document.getElementById("c-mode").value === "reference";
});

// Budgets: spent-vs-limit bar + window + policy. Neutral tokens.
function budgetsView(d) {
  const rows = (d.budgets || []).map(b => {
    const limit = b.limit_tokens, spent = b.spent;
    const pct = limit > 0 ? Math.min(100, Math.round(spent * 100 / limit)) : 0;
    const cls = pct >= 100 ? "over" : (pct >= 80 ? "warn" : "");
    // Persisted `upstream:` scopes are back-leg authority, independent of display labels; distinct
    // from the front-leg `vk:`/`group:` rows — badge them so operators can tell at a glance.
    const backLeg = b.scope.startsWith("upstream:")
      ? ' <span class="badge back-leg" title="back-leg cap on the resolved upstream">back leg</span>'
      : "";
    return `<tr><td><code>${esc(b.scope)}</code>${backLeg}</td>`
      + `<td>${fmt(spent)} <span class="muted">/ ${fmt(limit)}</span></td>`
      + `<td style="min-width:8rem"><div class="bar ${cls}"><span style="width:${pct}%"></span></div></td>`
      + `<td>${esc(b.window)}</td><td>${esc(b.policy)}</td></tr>`;
  }).join("");
  return tableScroll("Budgets", `<tr><th>scope</th><th class="num">spent / limit (tokens)</th><th>utilization</th><th>window</th><th>policy</th></tr>`,
    rows || '<tr><td colspan=5>no budgets configured</td></tr>');
}
function loadBudgets() {
  return loadPanel("budgets", "/dashboard/api/budgets", d => {
    requireFields(d, ["budgets"]);
    return budgetsView(d);
  });
}
async function setBudget() {
  document.getElementById("b-result").replaceChildren();
  const body = {
    scope: document.getElementById("b-scope").value.trim(),
    limit_tokens: Number(document.getElementById("b-limit").value || 0),
    window: document.getElementById("b-window").value,
    policy: document.getElementById("b-policy").value,
  };
  const data = await adminCall("POST", "/admin/budget", body);
  if (!data) return;
  document.getElementById("b-result").innerHTML = `<div class="callout ok">Budget set for ${esc(body.scope)}</div>`;
  loadBudgets();
}

function alertRow(a) {
  const fired = a.last_fired_at
    ? `<span style="color:var(--warn)">${esc(a.last_fired_at)}</span>` : '<span class="muted">never</span>';
  const ackBtn = a.last_fired_at
    ? `<button class="btn" data-needs-token data-action="ackAlert" data-id="${esc(a.id)}">Ack</button>` : "";
  return `<tr ${a.last_fired_at ? 'class="fired"' : ''}><td><code>${esc(a.id)}</code></td>`
    + `<td><code>${esc(a.scope)}</code></td><td class="num">${esc(a.threshold_pct)}%</td>`
    + `<td>${esc(a.channel)}</td><td>${fired}</td><td>${ackBtn}</td></tr>`;
}
function alertsView(d) {
  const fired = (d.fired || []).map(alertRow).join("");
  const head = `<tr><th>id</th><th>scope</th><th class="num">threshold</th><th>channel</th><th>last fired</th><th></th></tr>`;
  return `<h3>Recently fired</h3>`
    + tableScroll("Recently fired alerts", head, fired || '<tr><td colspan=6>none fired</td></tr>')
    + `<h3 style="margin-top:1.5rem">All configured rules</h3>`
    + pagedTable("alert-rules", "All configured alert rules", head, d.rules || [], alertRow,
      '<tr><td colspan=6>no rules configured</td></tr>', "rules");
}
function loadAlerts() {
  return loadPanel("alerts", "/dashboard/api/alerts", d => {
    requireFields(d, ["fired", "rules"]);
    return alertsView(d);
  });
}
async function ackAlert(id) {
  if (!(await adminCall("POST", `/admin/alerts/${encodeURIComponent(id)}/ack`))) return;
  toast("Alert acknowledged", true); loadAlerts();
}

// Config and run queries always require admin access, including in public-dashboard mode.
// Apply is additive-only server-side — see config.rs.
const actionBadge = a => {
  const cls = a === "create" || a === "mint" ? "active" : (a === "update" ? "" : "revoked");
  return `<span class="badge ${cls}" style="${cls ? '' : 'color:var(--muted);background:var(--border-soft)'}">${esc(a)}</span>`;
};
function configPlanTable(title, rows, cols) {
  const body = rows.map(r => `<tr>${cols.map(c => `<td>${orDash(r[c])}</td>`).join("")}<td>${actionBadge(r.action)}</td></tr>`).join("");
  return `<h3>${esc(title)}</h3>`
    + tableScroll(title, `<tr>${cols.map(c => `<th>${c}</th>`).join("")}<th>plan</th></tr>`,
      body || `<tr><td colspan=${cols.length + 1}>none declared</td></tr>`);
}
function loadPolicy() {
  return loadPanel("policy", "/admin/policy", data => {
    if (data.enabled === false) return '<p class="callout">Content inspection is not configured.</p>';
    requireFields(data, ["rules"], ["receipts"]);
    if (data.enabled !== true || !Number.isSafeInteger(data.revision)
        || !Number.isSafeInteger(data.receipts.used) || !Number.isSafeInteger(data.receipts.capacity)
        || data.receipts.used < 0 || data.receipts.capacity < 1
        || data.rules.some(r => typeof r.id !== "string" || typeof r.evaluator !== "string"
          || !["audit", "block", "quarantine"].includes(r.effect))) {
      throw new ApiError(502, "Incomplete policy status");
    }
    return `<p>Active revision <strong>${fmt(data.revision)}</strong>. Evaluation budget: ${fmt(data.deadline_ms)} ms.</p>
      <p>Audit records: <strong>${fmt(data.receipts.used)} / ${fmt(data.receipts.capacity)}</strong>.
      ${data.receipts.full ? '<strong>Storage full — requests are denied.</strong>' : 'Requests are denied if evaluation or audit storage is unavailable.'}</p>
      <p class="hint">Audit rules record a finding and allow forwarding. Block and quarantine rules deny forwarding.
      Classifier findings are signals for review; they do not guarantee that all sensitive data is detected.
      Prompt text and matched values are not stored in these receipts.</p>
      <div class="table-scroll"><table><thead><tr><th>Rule</th><th>Action</th><th>Evaluator</th></tr></thead>
      <tbody>${data.rules.map(r => `<tr><td>${esc(r.id)}</td><td>${esc(r.effect)}</td><td>${esc(r.evaluator)}</td></tr>`).join("")}</tbody></table></div>`;
  }, "read");
}

function humanExpiry(iso) {
  if (!iso) return "—";
  const then = new Date(iso).getTime();
  if (!Number.isFinite(then)) return esc(String(iso).slice(0, 10));
  const days = Math.round((then - Date.now()) / 86400000);
  if (days > 1) return `in ${days}d`;
  if (days === 1) return "in 1d";
  if (days >= 0) return "soon";
  return new Date(then).toISOString().slice(0, 10);
}

function loadTopology() {
  return loadPanel("topology", "/dashboard/api/topology", data => {
    requireFields(data, ["peers"], ["deployment"]);
    const dep = data.deployment;
    if (typeof dep.gateway_host !== "string") throw new ApiError(502, "Incomplete topology");
    const peers = (data.peers || []).map(p =>
      `<tr><td>${esc(p.name)}</td><td>${esc(p.kind || "")}</td><td>${esc(p.endpoint || "")}</td></tr>`).join("");
    return `<p>Gateway <strong>${esc(dep.gateway_host)}</strong> — ${esc(dep.public_name || "no public name")}
      (TLS ${esc(dep.tls_port ?? "-")}, HTTP ${esc(dep.http_port ?? "-")})</p>
      <div class="table-scroll wide"><table><thead><tr><th>Peer</th><th>Reach</th><th>Endpoint</th></tr></thead>
      <tbody>${peers || `<tr><td colspan=3>no peers declared</td></tr>`}</tbody></table></div>`;
  }, "read");
}

function loadConfig() {
  return loadPanel("config", "/admin/config", data => {
    requireFields(data, ["providers", "budgets", "alerts", "vkeys"]);
    return `<p class="muted" style="margin:0 0 .75rem">${esc(data.path)}</p>`
    + configPlanTable("Providers", data.providers, ["credential_id", "base_url"])
    + configPlanTable("Backends", data.backends || [], ["id", "upstream_ref"])
    + configPlanTable("Logical routes", data.routes || [], ["id", "upstream_ref", "revision"])
    + configPlanTable("Budgets", data.budgets, ["scope", "limit_tokens"])
    + configPlanTable("Alerts", data.alerts, ["scope", "threshold_pct"])
    + configPlanTable("Virtual keys", data.vkeys, ["upstream", "subject", "group"])
    + `<div class="form-row" style="margin-top:1rem"><button class="btn primary" data-needs-token data-action="applyConfig">Apply config</button></div>`;
  }, "admin");
}
async function applyConfig() {
  document.getElementById("config-result").replaceChildren();
  const data = await adminCall("POST", "/admin/config/apply", undefined, true);
  if (!data) return;
  const n = (x) => (x || []).length;
  const complete = data.ok !== false;
  let summary = `<div class="callout ${complete ? "ok" : "err"}">${complete ? "Applied" : "Incomplete — committed items were not rolled back"} — providers: ${n(data.providers.applied)}, `
    + `backends: ${n(data.backends?.applied)}, routes: ${n(data.routes?.applied)}, `
    + `budgets: ${n(data.budgets.applied)}, alerts: ${n(data.alerts.created)} created / ${n(data.alerts.skipped)} already satisfied, `
    + `vkeys: ${n(data.vkeys.minted)} minted / ${n(data.vkeys.skipped)} already satisfied</div>`;
  if (!complete) summary += `<div class="callout err">Failed items — inspect before retrying:<br>`
    + (data.failures || []).map(f => `${esc(f.component)}: ${esc(f.scope || f.upstream || f.provider || "")} — ${esc(f.error)}`).join("<br>") + `</div>`;
  if (n(data.vkeys.minted)) {
    summary += `<div class="callout info">New virtual keys — copy now, shown once:<br>`
      + data.vkeys.minted.map(k => `<code>${esc(k.virtual_key)}</code> (${esc(k.upstream_ref)})`).join("<br>")
      + `</div>`;
  }
  document.getElementById("config-result").innerHTML = summary;
  loadConfig(); loadKeys(); loadBudgets(); loadAlerts(); loadBackends(); loadRoutes();
}

// Run usage tree: recursive own-vs-rollup neutral observations for one agentic run. Admin-gated (attribution
// across a whole run can span multiple subjects, so it is treated like any other admin query).
function renderNode(n) {
  if (!n || typeof n.step_id !== "string" || (n.children !== undefined && !Array.isArray(n.children))) {
    throw new ApiError(502, "Incomplete response from server");
  }
  requireRunCounts(n.own); requireRunCounts(n.rollup);
  const kids = (n.children || []).map(renderNode).join("");
  return `<li><div class="node"><span class="step">${esc(n.step_id)}</span>`
    + `<span class="stat">own ${fmt(n.own && n.own.billable_tokens)} · subtree ${fmt(n.rollup && n.rollup.billable_tokens)} tok</span></div>`
    + (kids ? `<ul>${kids}</ul>` : "") + `</li>`;
}
function requireRunCounts(value) {
  if (!value || ![value.calls, value.billable_tokens].every(n => Number.isSafeInteger(n) && n >= 0)) {
    throw new ApiError(502, "Incomplete response from server");
  }
}
function lookupRun() {
  const id = document.getElementById("run-id").value.trim();
  if (!id) return;
  // A failed lookup of another run must not present the previous run's data as its result.
  panels.delete("run-tree");
  return loadPanel("run-tree", `/admin/usage/run/${encodeURIComponent(id)}`, data => {
    requireFields(data, [], ["run"]);
    const run = data.run;
    requireFields(run, ["roots"], ["total"]);
    if (run.run_id !== id) throw new ApiError(502, "Mismatched run response from server");
    requireRunCounts(run.total);
    const roots = run.roots.map(renderNode).join("");
    return `<div class="callout info">Total: ${fmt(run.total.billable_tokens)} observed token units across ${fmt(run.total.calls)} calls</div>`
      + `<ul class="tree" style="margin-top:.6rem">${roots || '<li class="muted">no steps recorded for this run</li>'}</ul>`;
  }, "read");
}

function lookupEvidence() {
  const kind = document.getElementById("evidence-kind").value;
  const value = document.getElementById("evidence-value").value;
  if (!["request", "session", "run"].includes(kind) || !value.trim()
      || new TextEncoder().encode(value).length > 256 || /[\u0000-\u001f\u007f-\u009f]/.test(value)) {
    toast("Enter an exact identifier of at most 256 UTF-8 bytes, without control characters.", false);
    return;
  }
  panels.delete("request-evidence");
  return loadPanel("request-evidence", "/admin/usage/diagnostics", data => {
    requireFields(data, ["rows", "unavailable_evidence"]);
    if (data.schema_version !== "1" || data.returned_rows !== data.rows.length || typeof data.truncated !== "boolean"
        || data.rows.some(row => !row || typeof row !== "object")) throw new ApiError(502, "Incomplete usage evidence");
    const rowHtml = row => {
      const observed = row.cache_read_observation?.status;
      const read = observed === "reported" ? fmt(row.cache_read_tokens) : observed === "absent" ? "not reported" : observed || "unknown";
      return `<tr><td><code>${orDash(row.request_id)}</code><br><span class="muted">${orDash(row.occurred_at)}</span></td><td>${orDash(row.provider)}<br><code>${orDash(row.model)}</code></td>
        <td class="num">${fmt(row.tokens_in)}</td><td class="num">${esc(read)}</td><td class="num">${fmt(row.cache_creation_tokens)}</td><td class="num">${fmt(row.tokens_out)}</td>
        <td><details><summary>Details</summary><p>Reasoning: ${fmt(row.reasoning_tokens)} (${row.reasoning_included === true ? "included in output" : row.reasoning_included === false ? "separate from output" : "overlap not established"})</p>
        <p>Duration: ${fmt(row.duration_ms)} ms · ${orDash(row.duration_source)}</p><p>Step: <code>${orDash(row.step_id)}</code></p></details></td></tr>`;
    };
    return `<h3>Results for ${esc(kind)}: <code>${esc(value)}</code></h3>
      <p>${fmt(data.returned_rows)} persisted records${data.truncated ? ' · <strong>Result truncated; the query limit or byte limit was reached.</strong>' : ""}</p>`
      + pagedTable("usage-evidence", "Persisted usage records", '<tr><th>Request / time</th><th>Provider / model</th><th class="num">Fresh</th><th class="num">Cache read</th><th class="num">Cache write</th><th class="num">Output</th><th></th></tr>', data.rows, rowHtml,
        '<tr><td colspan="7">No persisted records matched. Active or unavailable observations may not have a row.</td></tr>', "records", true)
      + `<p class="hint">${esc(data.snapshot_notice)}</p><details><summary>Evidence limits</summary><p class="hint">${esc(data.request_id_notice)}</p><p>Unavailable fields: ${data.unavailable_evidence.map(esc).join(", ")}</p></details>`;
  }, "diagnostics", { method: "POST", body: { selector: { kind, value }, limit: 100 } });
}

async function refreshIdentity() {
  const status = await requestJSON("GET", "/auth/session");
  if (!["none", "oidc", "tokens"].includes(status.mode)) throw new ApiError(502, "Invalid authentication mode");
  const operatorMode = status.mode === "none" ? status.operator_mode : status.mode;
  if (status.mode === "none" && (!["none", "tokens"].includes(operatorMode)
      || status.authenticated !== false || !Array.isArray(status.permissions)
      || status.permissions.some(p => !["read", "operate", "admin", "diagnostics"].includes(p)))) {
    throw new ApiError(502, "Invalid starter access response");
  }
  if (status.mode === "oidc" && status.subject && (!Array.isArray(status.permissions) || typeof status.csrf !== "string" || !Number.isSafeInteger(status.expires_at))) {
    throw new ApiError(502, "Invalid session response");
  }
  const nextSubject = status.mode === "oidc" ? status.subject : undefined;
  const nextCsrf = status.mode === "oidc" ? status.csrf : undefined;
  const changed = authMode !== operatorMode || inferenceAuthMode !== status.mode
    || ssoSession?.subject !== nextSubject || ssoSession?.csrf !== nextCsrf;
  inferenceAuthMode = status.mode;
  authMode = operatorMode;
  starterPermissions = status.mode === "none" && operatorMode === "none" ? status.permissions : [];
  if (operatorMode === "none") tokenEl.value = "";
  ssoSession = status.mode === "oidc" && status.subject ? status : null;
  if (changed) resetAuth("");
  clearTimeout(expiryTimer);
  if (ssoSession) expiryTimer = setTimeout(() => { ssoSession = null; resetAuth(""); }, Math.max(0, ssoSession.expires_at * 1000 - Date.now()));
  refreshTokenState();
}
async function refreshAll() {
  const revision = authRevision;
  try { await refreshIdentity(); }
  catch {
    // A cancelled check belongs to its old authentication session. It must not
    // clear a newer token/session or cancel the newer session's pending reads.
    if (revision !== authRevision) return;
    ssoSession = null; starterPermissions = []; authMode = "loading"; inferenceAuthMode = "loading"; resetAuth("");
    document.getElementById("auth-status").textContent = "Access verification unavailable. Retry with Refresh.";
    return;
  }
  const loadRevision = authRevision;
  return Promise.all([loadExecution(), loadUsage(), loadKeys(), loadBudgets(), loadAlerts(), loadConfig(), loadPolicy(), loadTopology(), loadBackends(), loadRoutes()])
    .then(() => {
      if (authRevision !== loadRevision) return;
      const failed = [...document.querySelectorAll("[data-state]")].filter(el => ["unavailable", "stale"].includes(el.dataset.state)
        || el.id === "usage" && el.dataset.state === "unconfigured").length;
      document.getElementById("refresh-status").textContent = "Updated " + new Date().toLocaleTimeString()
        + (failed ? ` · ${failed} panels need attention` : " · data loaded");
    });
}

// Attribute values are inert data, never JavaScript source. Only this fixed action table
// dispatches events, including buttons created when tables are refreshed.
const actions = {
  refresh: refreshAll,
  logout: async () => {
    await requestJSON("POST", "/auth/logout", {});
    ssoSession = null; resetAuth(""); await refreshIdentity();
  },
  clearToken: () => { tokenEl.value = ""; resetAuth(""); refreshAll(); tokenEl.focus(); },
  revokeVkey: button => revokeVkey(button.dataset.id),
  revokeCred: button => revokeCred(button.dataset.provider, button.dataset.label),
  ackAlert: button => ackAlert(button.dataset.id),
  mintVkey, addCredential, setBudget, applyConfig, lookupRun, lookupEvidence,
  pager: button => {
    const id = button.dataset.pager;
    const state = pagers.get(id);
    if (!state) return;
    const pages = Math.max(1, Math.ceil(state.rows.length / PAGE_SIZE));
    state.page = Math.min(pages - 1, Math.max(0, Number(button.dataset.target) || 0));
    paintPager(id);
  },
};
const busyButtons = new WeakSet();
document.addEventListener("click", async event => {
  const button = event.target.closest("button[data-action]");
  if (!button || button.disabled || busyButtons.has(button) || !Object.hasOwn(actions, button.dataset.action)) return;
  busyButtons.add(button);
  button.disabled = true;
  try { await actions[button.dataset.action](button); }
  catch { toast("Action could not be completed. Refresh to check the current state.", false); }
  finally { busyButtons.delete(button); button.disabled = false; refreshTokenState(); }
});
window.addEventListener("pagehide", () => { tokenEl.value = ""; ssoSession = null; clearTimeout(expiryTimer); resetAuth(""); });
window.addEventListener("pageshow", event => { if (event.persisted) refreshAll(); });

// Views are allowlisted by existing nav links. Hashes carry navigation only, never
// credentials. Back/forward and old section deep links work without a routing dependency.
const sectionLinks = new Map();
for (const link of document.querySelectorAll(".section-nav a")) sectionLinks.set(link.hash.slice(1), link);
function showView(id, focus = false) {
  if (!sectionLinks.has(id)) id = "overview";
  for (const view of document.querySelectorAll(".workspace-view")) view.hidden = view.dataset.view !== id;
  for (const [key, link] of sectionLinks) {
    const on = key === id;
    link.classList.toggle("active", on);
    if (on) link.setAttribute("aria-current", "page"); else link.removeAttribute("aria-current");
  }
  const name = sectionLinks.get(id).textContent;
  document.getElementById("current-view").textContent = name;
  document.title = `Sandhi — ${name}`;
  if (focus) {
    const heading = document.getElementById(id);
    heading.tabIndex = -1;
    heading.focus({ preventScroll: true });
    window.scrollTo({ top: 0, behavior: "instant" });
  }
}
for (const link of document.querySelectorAll(".section-nav a, [data-view-link]")) link.addEventListener("click", event => {
  const id = link.hash.slice(1);
  if (!sectionLinks.has(id)) return;
  event.preventDefault();
  if (location.hash !== `#${id}`) history.pushState(null, "", `#${id}`);
  showView(id, true);
});
window.addEventListener("hashchange", () => showView(location.hash.slice(1)));
window.addEventListener("popstate", () => showView(location.hash.slice(1)));
document.addEventListener("input", event => {
  if (event.target.id !== "usage-filter" || !usageData) return;
  attributionQuery = event.target.value.slice(0, 256);
  document.getElementById("tables").innerHTML = usageTables(usageData);
});
if (typeof ResizeObserver === "function") new ResizeObserver(entries => {
  document.documentElement.style.setProperty("--header-h", `${entries[0].target.getBoundingClientRect().height}px`);
}).observe(document.querySelector("header"));
showView(location.hash.slice(1));
refreshTokenState();
refreshAll();
