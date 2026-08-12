// Data-driven rendering for vigil-ui's incident window. Loaded fresh on
// every full page navigation (see `open_incident_window` in
// `src-tauri/src/lib.rs`, which does a `window.location.replace(...)` for
// every incident, including ones detected while this window is already
// open) -- so nothing here may assume it only ever runs once per app
// lifetime. There is no module-level mutable state that needs to survive
// across incidents; `DOMContentLoaded` firing again after a full reload is
// sufficient on its own to reset everything.

const { invoke } = window.__TAURI__.core;

function getIncidentPath() {
  const params = new URLSearchParams(window.location.search);
  return params.get("path");
}

function getIncidentsDir(path) {
  // The incidents directory is the path's parent directory.
  const idx = path.lastIndexOf("/");
  return idx === -1 ? "." : path.slice(0, idx);
}

async function loadIncident(path) {
  const incidentsDir = getIncidentsDir(path);
  let incident = await invoke("read_incident_json", { incidentsDir, path });

  if (!incident.diagnosis) {
    setThinking(true);
    try {
      await invoke("investigate", { alertKey: incident.alert_key, incidentsDir });
      incident = await invoke("read_incident_json", { incidentsDir, path });
    } catch (err) {
      showError(`Investigation failed: ${err}`);
      setThinking(false);
      return;
    }
    setThinking(false);
  }

  renderOrigin(incident);
  renderDiagnosis(incident);

  if (incident.alert_key) {
    const tree = await invoke("process_tree", { alertKey: incident.alert_key });
    renderProcessTree(tree);
  } else {
    document.getElementById("process-tree-card").style.display = "none";
  }

  if (incident.proposed_fix) {
    renderFixCard(incident.proposed_fix, path);
  }
}

function setThinking(isThinking) {
  document.getElementById("diagnosis-body").textContent = isThinking
    ? "Investigating…"
    : "";
}

function showError(message) {
  document.getElementById("diagnosis-body").textContent = message;
}

// Fills in the "what notification led here" inset. The four Tauri commands
// this window consumes don't carry the exact native-notification strings
// that were shown at fire time (those are ephemeral, owned by the OS) --
// `incident.title`/`incident.rule_message` are the closest available
// equivalents, already fetched via `read_incident_json`, so this reuses
// them rather than leaving the mockup's hardcoded example text in place.
// `incident.command` is deliberately NOT used for the suggested command
// span -- that field is the *target process's* argv (see
// `incidents::extract_command`'s doc comment), not a vigil subcommand.
function renderOrigin(incident) {
  document.getElementById("notif-title").textContent = incident.title ?? "";
  const body = document.getElementById("notif-body");
  body.textContent = "";
  if (incident.rule_message) {
    body.append(incident.rule_message + " ");
  }
  if (incident.alert_key) {
    const cmd = document.createElement("span");
    cmd.className = "cmd mono";
    cmd.textContent = `vigil investigate ${incident.alert_key}`;
    body.append(cmd);
  }
}

function renderDiagnosis(incident) {
  document.getElementById("diagnosis-title").textContent = incident.title ?? "";
  document.getElementById("alert-key-badge").textContent = incident.alert_key ?? "";
  document.getElementById("diagnosis-body").textContent = incident.diagnosis ?? "No diagnosis yet.";

  const eyebrow = document.getElementById("tree-eyebrow");
  eyebrow.textContent = incident.alert_key ? `Process tree — ${incident.alert_key}` : "Process tree";
}

function renderProcessTree(nodes) {
  const card = document.getElementById("process-tree-card");
  // `process_tree.rs::query_process_tree`'s own doc comment: for alert keys
  // that don't name a specific process/group (`Scope::None`, e.g.
  // `high_load`/`battery_low`), "the caller should skip rendering a tree
  // section entirely rather than dumping every process on the machine" --
  // an empty result here is exactly that case, not a loading/error state.
  if (nodes.length === 0) {
    card.style.display = "none";
    return;
  }
  card.style.display = "";

  const container = document.getElementById("tree-container");
  container.innerHTML = "";
  for (const node of nodes) {
    const row = document.createElement("div");
    row.className = "row";
    const statusChip = node.is_zombie ? '<span class="chip leak">zombie</span>' : node.ppid === null ? '<span class="chip idle">orphan</span>' : '<span class="chip idle">child</span>';
    row.innerHTML = `
      <div class="proc-cell"><span class="proc-name-sm mono">${escapeHtml(node.name)}</span></div>
      <span class="parent mono">${node.ppid ?? "—"}</span>
      <span>${statusChip}</span>
      <span class="age mono">${formatDuration(node.run_time_secs)}</span>
      <span class="cpu mono">${node.cpu_pct.toFixed(1)}%</span>
      <span class="ram mono">${formatBytes(node.mem_bytes)}</span>
    `;
    container.appendChild(row);
  }

  // "Flagged group" is the one vitals tile this task can honestly populate
  // from data already fetched -- the process count this incident's tree
  // scoped to. The other three vitals tiles (swap/free RAM/load avg) have
  // no data source among the four Tauri commands this window consumes (no
  // live-snapshot command exists yet), so they stay as placeholder "—"
  // text; see the task report for that scope note.
  document.getElementById("vital-flagged-value").innerHTML = `${nodes.length}<span class="unit">procs</span>`;
}

function renderFixCard(plan, path) {
  const card = document.getElementById("fix-card");
  card.style.display = "";
  const steps = plan.plan;
  document.getElementById("fix-step-label").textContent = `${steps.length} step${steps.length === 1 ? "" : "s"}`;

  const body = document.getElementById("fix-body");
  const stepsContainer = document.createElement("div");
  const approvals = new Array(steps.length).fill(false);

  steps.forEach((step, i) => {
    const stepEl = document.createElement("div");
    stepEl.className = "fix-step";
    stepEl.innerHTML = `
      <div class="fix-category"><span class="dot"></span>${escapeHtml(step.category)}</div>
      <p class="fix-desc">${escapeHtml(step.description)}</p>
      <div class="fix-target"><span class="k">Target</span> <span class="v mono">${escapeHtml(step.target_hint)}</span></div>
      <label><input type="checkbox" data-step="${i}"> Approve this step</label>
    `;
    stepEl.querySelector("input").addEventListener("change", (e) => {
      approvals[i] = e.target.checked;
    });
    stepsContainer.appendChild(stepEl);
  });
  // Inserted before the card's existing static children (fix-verify,
  // fix-actions, the two result banners) so the rendered order matches the
  // mockup: category/description/target first, then the verify note and
  // approve/reject actions -- `appendChild` would instead put the steps
  // last, after those static elements.
  body.insertBefore(stepsContainer, body.firstChild);

  document.querySelector("button.approve").addEventListener("click", async () => {
    try {
      const result = await invoke("run_fix", { path, approvals });
      document.getElementById("fix-actions").style.display = "none";
      document.getElementById("result-approve-text").textContent = result;
      document.getElementById("result-approve").classList.add("show");
    } catch (err) {
      document.getElementById("fix-actions").style.display = "none";
      document.getElementById("result-approve-text").textContent = `vigil fix failed: ${err}`;
      document.getElementById("result-approve").classList.add("show");
    }
  });
  document.querySelector("button.reject").addEventListener("click", () => {
    document.getElementById("fix-actions").style.display = "none";
    document.getElementById("result-reject").classList.add("show");
  });
}

function escapeHtml(s) {
  const div = document.createElement("div");
  div.textContent = s;
  return div.innerHTML;
}

function formatDuration(secs) {
  const days = Math.floor(secs / 86400);
  const hours = Math.floor((secs % 86400) / 3600);
  if (days > 0) return `${days}d ${hours}h`;
  const mins = Math.floor((secs % 3600) / 60);
  if (hours > 0) return `${hours}h ${mins}m`;
  return `${mins}m`;
}

function formatBytes(bytes) {
  const mb = bytes / (1024 * 1024);
  if (mb > 1024) return `${(mb / 1024).toFixed(1)} GB`;
  return `${Math.round(mb)} MB`;
}

window.addEventListener("DOMContentLoaded", () => {
  const path = getIncidentPath();
  if (path) {
    loadIncident(path);
  } else {
    showError("No incident path provided.");
  }
});
