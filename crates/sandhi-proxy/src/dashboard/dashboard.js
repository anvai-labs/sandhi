"use strict";

const fmt = n => typeof n === "number" && Number.isFinite(n) ? n.toLocaleString() : "—";
const esc = s => String(s ?? "").replace(/[&<>"']/g, c =>
  ({ "&":"&amp;", "<":"&lt;", ">":"&gt;", '"':"&quot;", "'":"&#39;" }[c]));
const orDash = s => (s === null || s === undefined || s === "") ? "—" : esc(s);
// No latency is "—", never "0 ms": a call that never reported a duration is unknown, not fast.
const lat = l => (!l || !l.samples) ? "—"
  : `${fmt(l.p50_ms)} / ${fmt(l.p95_ms)} ms <span class="muted">(n=${fmt(l.samples)})</span>`;

// The token lives only in memory. Editing/clearing it cancels reads and hides all prior
// privileged data immediately. Old responses cannot repopulate a new authentication session.
const tokenEl = document.getElementById("admin-token");
const dotEl = document.getElementById("token-dot");
let activeToken = "";
let authRevision = 0;
const pending = new Set();
const panels = new Map();
try { sessionStorage.removeItem("sandhi_admin_token"); } catch { /* Storage may be disabled. */ }
function refreshTokenState() {
  const on = activeToken.length > 0;
  dotEl.classList.toggle("on", on);
  dotEl.title = on ? "Token supplied; each request is authorized by the server" : "No token supplied";
  document.querySelectorAll("[data-needs-token]").forEach(b => b.disabled = !on || b.dataset.revoked === "true");
}
function resetAuth(token) {
  activeToken = token;
  authRevision++;
  for (const controller of pending) controller.abort();
  pending.clear();
  panels.clear();
  for (const id of ["usage", "keys", "budgets", "alerts", "config", "run-tree"]) {
    const el = document.getElementById(id);
    el.replaceChildren();
    el.dataset.state = "locked";
    el.setAttribute("aria-busy", "false");
    const message = document.createElement("p");
    message.textContent = "Select Use token to load protected data, or Refresh for public data.";
    el.append(message);
  }
  document.querySelectorAll("[data-sensitive]").forEach(el => el.replaceChildren());
  document.getElementById("c-secret").value = "";
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
        ...(activeToken ? { "Authorization": "Bearer " + activeToken } : {}),
        ...(body === undefined ? {} : { "Content-Type": "application/json" }),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
      cache: "no-store", redirect: "error", signal: controller.signal,
    });
    const data = await resp.json().catch(() => null);
    if (revision !== authRevision) throw new DOMException("Authentication changed", "AbortError");
    if (!resp.ok) {
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
  if (error.status === 401) return "Authentication required. Enter a valid admin token and select Use token.";
  if (error.status === 403) return "Access denied. This operation is not available with the current access configuration.";
  if (error.status === 404) return "Not configured or not found. " + error.message;
  return "Data unavailable. Retry with Refresh. " + (error.name === "AbortError" ? "Request timed out." : error.message);
}

async function adminCall(method, path, body, allowPartial = false) {
  if (!activeToken) { toast("Enter the admin token and select Use token first.", false); return null; }
  const revision = authRevision;
  try { return await requestJSON(method, path, body); }
  catch (error) {
    if (revision === authRevision) toast(failureMessage(error), false);
    if (revision === authRevision && allowPartial && error.data?.ok === false) return error.data;
    return null;
  }
}

async function loadPanel(id, path, render, adminOnly = false) {
  const el = document.getElementById(id);
  const revision = authRevision;
  const previous = panels.get(id);
  const ticket = { html: previous?.html, updated: previous?.updated };
  panels.set(id, ticket);
  el.setAttribute("aria-busy", "true");
  el.dataset.state = "loading";
  el.textContent = "Loading…";
  try {
    if (adminOnly && !activeToken) throw new ApiError(401, "Authentication required");
    const data = await requestJSON("GET", path);
    if (revision !== authRevision || panels.get(id) !== ticket) return;
    // Validate required fields before treating a response as a successful empty dataset.
    ticket.html = render(data);
    ticket.updated = new Date().toLocaleTimeString();
    el.innerHTML = ticket.html;
    el.dataset.state = "ready";
    const updated = document.createElement("p");
    updated.className = "muted";
    updated.textContent = "Loaded at " + ticket.updated + ". Use Refresh to update.";
    el.prepend(updated);
  } catch (error) {
    if (revision !== authRevision || panels.get(id) !== ticket) return;
    const stale = ticket.html && error.status !== 401 && error.status !== 403 && error.status !== 404;
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

function tbl(title, rows) {
  const body = rows.map(r => `<tr><td>${esc(r.key)}</td><td class="num">${fmt(r.calls)}</td>`
    + `<td class="num">${fmt(r.tokens_in)}</td><td class="num">${fmt(r.tokens_out)}</td>`
    + `<td class="num">${fmt(r.cache_creation_tokens)}</td><td class="num">${fmt(r.cache_read_tokens)}</td>`
    + `<td class="num">${fmt(r.billable_tokens)}</td>`
    + `<td class="num">${lat(r.latency)}</td></tr>`).join("");
  return `<h3>${title}</h3><table><thead><tr><th>key</th><th class="num">calls</th>`
    + `<th class="num">in</th><th class="num">out</th><th class="num">cache write</th>`
    + `<th class="num">cache read</th><th class="num" title="ADR-0005 D4: the quantity budgets `
    + `are enforced on — fresh input + cache split + output (+ unfolded reasoning)">billable`
    + `</th><th class="num" title="p50 / p95 milliseconds over the sampled calls that reported a `
    + `duration — approximate by design; tokens above are exact">latency</th></tr></thead>`
    + `<tbody>${body || '<tr><td colspan=8>no data yet</td></tr>'}</tbody></table>`;
}

function loadUsage() {
  return loadPanel("usage", "/dashboard/api/usage", d => {
    requireFields(d, ["by_subject", "by_group", "by_provider", "by_model"], ["total"]);
    const t = d.total;
    return '<div class="cards" id="cards">' +
      [["calls", fmt(t.calls)], ["tokens in", fmt(t.tokens_in)], ["tokens out", fmt(t.tokens_out)],
       ["cache read", fmt(t.cache_read_tokens)], ["billable", fmt(t.billable_tokens)],
       ["latency p50/p95", lat(t.latency)]]
      .map(([l, n]) => `<div class="card"><div class="n">${n}</div><div class="l">${l}</div></div>`).join("")
      + `</div><h3>Attribution</h3><div id="tables">`
      + tbl("By user (subject)", d.by_subject || [])
      + tbl("By team (group)", d.by_group || [])
      + tbl("By provider", d.by_provider || [])
      + tbl("By model", d.by_model || [])
      + `</div>`;
  });
}

// Keys: masked virtual keys + vault entries. Never a secret. Revoke/add are admin-gated.
function keysView(d) {
  const vkeys = (d.virtual_keys || []).map(k => {
    const status = k.revoked_at ? "revoked" : "active";
    const disabled = status === "revoked" ? "disabled" : "";
    return `<tr><td><code>${esc(k.id)}</code></td><td>${orDash(k.subject)}</td><td>${orDash(k.group)}</td>`
      + `<td><code>${esc(k.upstream_ref)}</code></td><td>${(k.models||[]).map(esc).join(", ")||'<span class="muted">any</span>'}</td>`
      + `<td><span class="badge ${status}">${status}</span></td><td>${orDash(k.expires_at)}</td>`
      + `<td><button class="btn danger" data-needs-token ${disabled} data-revoked="${!!k.revoked_at}" data-action="revokeVkey" data-id="${esc(k.id)}">Revoke</button></td></tr>`;
  }).join("");
  const vault = (d.vault || []).map(e => `<tr><td><code>${esc(e.credential_id)}</code></td>`
    + `<td>${esc(e.scheme)}</td><td>${orDash(e.base_url)}</td>`
    + `<td><span class="badge ${e.status === "active" ? "active" : "revoked"}">${esc(e.status)}</span></td>`
    + `<td><button class="btn danger" data-needs-token data-action="revokeCred" data-provider="${esc(e.provider)}" data-label="${esc(e.label)}">Revoke</button></td></tr>`).join("");
  return `<h3>Virtual keys (masked — secrets are never stored)</h3>`
    + `<table><thead><tr><th>id</th><th>subject</th><th>group</th><th>upstream</th><th>models</th><th>status</th><th>expires</th><th></th></tr></thead>`
    + `<tbody>${vkeys || '<tr><td colspan=8>no virtual keys</td></tr>'}</tbody></table>`
    + `<h3>Provider credentials (vault metadata)</h3>`
    + `<table><thead><tr><th>credential</th><th>scheme</th><th>base url</th><th>status</th><th></th></tr></thead>`
    + `<tbody>${vault || '<tr><td colspan=5>no provider credentials</td></tr>'}</tbody></table>`;

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
    return `<tr><td><code>${esc(b.scope)}</code></td>`
      + `<td>${fmt(spent)} <span class="muted">/ ${fmt(limit)}</span></td>`
      + `<td style="min-width:8rem"><div class="bar ${cls}"><span style="width:${pct}%"></span></div></td>`
      + `<td>${esc(b.window)}</td><td>${esc(b.policy)}</td></tr>`;
  }).join("");
  return `<table><thead><tr><th>scope</th><th class="num">spent / limit (tokens)</th><th>utilization</th><th>window</th><th>policy</th></tr></thead>`
    + `<tbody>${rows || '<tr><td colspan=5>no budgets configured</td></tr>'}</tbody></table>`;
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
  const rules = (d.rules || []).map(alertRow).join("");
  return `<h3>Recently fired</h3>`
    + `<table><thead><tr><th>id</th><th>scope</th><th class="num">threshold</th><th>channel</th><th>last fired</th><th></th></tr></thead>`
    + `<tbody>${fired || '<tr><td colspan=6>none fired</td></tr>'}</tbody></table>`
    + `<h3 style="margin-top:1.5rem">All configured rules</h3>`
    + `<table><thead><tr><th>id</th><th>scope</th><th class="num">threshold</th><th>channel</th><th>last fired</th><th></th></tr></thead>`
    + `<tbody>${rules || '<tr><td colspan=6>no rules configured</td></tr>'}</tbody></table>`;
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
  return `<h3>${title}</h3><table><thead><tr>${cols.map(c => `<th>${c}</th>`).join("")}<th>plan</th></tr></thead>`
    + `<tbody>${body || `<tr><td colspan=${cols.length + 1}>none declared</td></tr>`}</tbody></table>`;
}
function loadConfig() {
  return loadPanel("config", "/admin/config", data => {
    requireFields(data, ["providers", "budgets", "alerts", "vkeys"]);
    return `<p class="muted" style="margin:0 0 .75rem">${esc(data.path)}</p>`
    + configPlanTable("Providers", data.providers, ["credential_id", "base_url"])
    + configPlanTable("Budgets", data.budgets, ["scope", "limit_tokens"])
    + configPlanTable("Alerts", data.alerts, ["scope", "threshold_pct"])
    + configPlanTable("Virtual keys", data.vkeys, ["upstream", "subject", "group"])
    + `<div class="form-row" style="margin-top:1rem"><button class="btn primary" data-needs-token data-action="applyConfig">Apply config</button></div>`;
  }, true);
}
async function applyConfig() {
  document.getElementById("config-result").replaceChildren();
  const data = await adminCall("POST", "/admin/config/apply", undefined, true);
  if (!data) return;
  const n = (x) => (x || []).length;
  const complete = data.ok !== false;
  let summary = `<div class="callout ${complete ? "ok" : "err"}">${complete ? "Applied" : "Incomplete — committed items were not rolled back"} — providers: ${n(data.providers.applied)}, `
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
  loadConfig(); loadKeys(); loadBudgets(); loadAlerts();
}

// Run cost tree: recursive own-vs-rollup breakdown for one agentic run. Admin-gated (attribution
// across a whole run can span multiple subjects, so it is treated like any other admin query).
function renderNode(n) {
  const kids = (n.children || []).map(renderNode).join("");
  return `<li><div class="node"><span class="step">${esc(n.step_id)}</span>`
    + `<span class="stat">own ${fmt(n.own && n.own.billable_tokens)} · subtree ${fmt(n.rollup && n.rollup.billable_tokens)} tok</span></div>`
    + (kids ? `<ul>${kids}</ul>` : "") + `</li>`;
}
function lookupRun() {
  const id = document.getElementById("run-id").value.trim();
  if (!id) return;
  // A failed lookup of another run must not present the previous run's data as its result.
  panels.delete("run-tree");
  return loadPanel("run-tree", `/admin/usage/run/${encodeURIComponent(id)}`, data => {
    requireFields(data, ["roots"], ["total"]);
    const roots = data.roots.map(renderNode).join("");
    return `<div class="callout info">Total: ${fmt(data.total.billable_tokens)} billable tokens across ${fmt(data.total.calls)} calls</div>`
      + `<ul class="tree" style="margin-top:.6rem">${roots || '<li class="muted">no steps recorded for this run</li>'}</ul>`;
  }, true);
}

function refreshAll() {
  return Promise.all([loadUsage(), loadKeys(), loadBudgets(), loadAlerts(), loadConfig()]);
}

// Attribute values are inert data, never JavaScript source. Only this fixed action table
// dispatches events, including buttons created when tables are refreshed.
const actions = {
  refresh: refreshAll,
  clearToken: () => { tokenEl.value = ""; resetAuth(""); refreshAll(); tokenEl.focus(); },
  revokeVkey: button => revokeVkey(button.dataset.id),
  revokeCred: button => revokeCred(button.dataset.provider, button.dataset.label),
  ackAlert: button => ackAlert(button.dataset.id),
  mintVkey, addCredential, setBudget, applyConfig, lookupRun,
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
window.addEventListener("pagehide", () => { tokenEl.value = ""; resetAuth(""); });
window.addEventListener("pageshow", event => { if (event.persisted) refreshAll(); });
refreshTokenState();
refreshAll();
