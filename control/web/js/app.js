import { api } from "./api.js";

const $ = (id) => document.getElementById(id);

const viewDashboard = $("view-dashboard");
const viewRun = $("view-run");
const runsBody = $("runs-body");
const runMeta = $("run-meta");
const eventsList = $("events-list");

let currentRunId = null;
let statsTimer = null;

function badge(status) {
  const s = (status || "unknown").toLowerCase();
  return `<span class="badge ${s}">${s}</span>`;
}

function showError(el, msg) {
  el.textContent = msg;
  el.classList.remove("hidden");
}

function hideError(el) {
  el.classList.add("hidden");
}

function route() {
  const hash = location.hash.slice(1);
  const match = hash.match(/^\/run\/(.+)$/);
  if (match) {
    showRunView(match[1]);
  } else {
    showDashboard();
  }
}

function showDashboard() {
  currentRunId = null;
  viewDashboard.classList.remove("hidden");
  viewRun.classList.add("hidden");
  hideError($("run-error"));
  startStatsPoll();
  loadRuns();
}

async function showRunView(runId) {
  currentRunId = runId;
  viewDashboard.classList.add("hidden");
  viewRun.classList.remove("hidden");
  stopStatsPoll();
  hideError($("run-error"));
  runMeta.innerHTML = "";
  eventsList.innerHTML = "<li>Loading…</li>";

  try {
    const [run, events] = await Promise.all([
      api.getRun(runId),
      api.getEvents(runId),
    ]);

    runMeta.innerHTML = `
      <div><dt>Run ID</dt><dd>${run.run_id}</dd></div>
      <div><dt>Status</dt><dd>${badge(run.status)}</dd></div>
      <div><dt>Outcome</dt><dd>${run.outcome || "—"}</dd></div>
      <div><dt>Scope</dt><dd>${run.scope_id}</dd></div>
      <div><dt>Test pack</dt><dd>${run.test_pack}</dd></div>
      <div><dt>Created</dt><dd>${run.created_at || "—"}</dd></div>
    `;

    if (events.length === 0) {
      eventsList.innerHTML = "<li>No events yet.</li>";
    } else {
      eventsList.innerHTML = events
        .map(
          (e) => `
        <li>
          <span class="type">${e.event_type || e.type || "?"}</span>
          ${e.severity ? `<span class="badge">${e.severity}</span> ` : ""}
          ${e.message || ""}
          ${e.target ? `<span class="mono"> (${e.target})</span>` : ""}
        </li>`
        )
        .join("");
    }
  } catch (err) {
    eventsList.innerHTML = "";
    showError($("run-error"), err.message);
  }
}

async function loadStats() {
  try {
    const s = await api.stats();
    $("stat-queued").textContent = s.queued ?? 0;
    $("stat-running").textContent = s.running ?? 0;
    $("stat-completed").textContent = s.completed ?? 0;
    $("stat-cancelled").textContent = s.cancelled ?? 0;
    $("stat-total").textContent = s.total ?? 0;
  } catch {
    /* ignore transient errors */
  }
}

async function loadRuns() {
  hideError($("runs-error"));
  try {
    const runs = await api.listRuns();
    const recent = runs.slice(0, 100);
    runsBody.innerHTML = recent
      .map(
        (r) => `
      <tr data-run-id="${r.run_id}">
        <td class="mono">${r.run_id}</td>
        <td>${badge(r.status)}</td>
        <td>${r.outcome || "—"}</td>
        <td>${r.test_pack}</td>
        <td>${r.created_at || "—"}</td>
      </tr>`
      )
      .join("");
  } catch (err) {
    runsBody.innerHTML = "";
    showError($("runs-error"), err.message);
  }
}

function startStatsPoll() {
  stopStatsPoll();
  loadStats();
  statsTimer = setInterval(loadStats, 2000);
}

function stopStatsPoll() {
  if (statsTimer) {
    clearInterval(statsTimer);
    statsTimer = null;
  }
}

$("create-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  hideError($("create-error"));

  const targets = $("targets")
    .value.split("\n")
    .map((t) => t.trim())
    .filter(Boolean);

  if (targets.length === 0) {
    showError($("create-error"), "At least one target is required.");
    return;
  }

  try {
    const result = await api.createRun({
      scope_id: $("scope").value,
      test_pack: $("test-pack").value,
      targets,
    });
    loadRuns();
    loadStats();
    location.hash = `#/run/${result.run_id}`;
  } catch (err) {
    showError($("create-error"), err.message);
  }
});

runsBody.addEventListener("click", (e) => {
  const row = e.target.closest("tr[data-run-id]");
  if (row) {
    location.hash = `#/run/${row.dataset.runId}`;
  }
});

$("refresh-runs").addEventListener("click", () => {
  loadRuns();
  loadStats();
});

$("back-dashboard").addEventListener("click", () => {
  location.hash = "#/";
});

$("cancel-run").addEventListener("click", async () => {
  if (!currentRunId) return;
  try {
    await api.cancelRun(currentRunId);
    showRunView(currentRunId);
  } catch (err) {
    showError($("run-error"), err.message);
  }
});

$("delete-run").addEventListener("click", async () => {
  if (!currentRunId) return;
  if (!confirm(`Delete ${currentRunId}?`)) return;
  try {
    await api.deleteRun(currentRunId);
    location.hash = "#/";
  } catch (err) {
    showError($("run-error"), err.message);
  }
});

$("clear-runs").addEventListener("click", async () => {
  if (!confirm("Clear all runs?")) return;
  await api.clearRuns();
  loadRuns();
  loadStats();
});

$("clear-queue").addEventListener("click", async () => {
  await api.clearQueue();
});

window.addEventListener("hashchange", route);
route();
