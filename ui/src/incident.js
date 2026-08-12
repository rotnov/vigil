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

// Whether this page load was something a human actually asked for.
// `navigate_to_incident` (src-tauri/src/lib.rs) sets `auto=1` for every
// user-initiated arrival -- a `vigil://` deep link, a menu-bar dropdown
// click routed through single-instance, a cold start launched by URL --
// and `auto=0` for the incidents poller's silent pre-navigation, which
// happens with nobody looking. Investigation spends real agent tokens, so
// only `auto=1` may start one on its own; anything else (including a
// hand-built URL with no `auto` at all) defers until the user is
// demonstrably here. See AGENTS.md: "Investigation is opt-in, not
// automatic ... No agent process spawns until the user explicitly runs
// that command."
function isUserInitiated() {
  const params = new URLSearchParams(window.location.search);
  return params.get("auto") === "1";
}

function getIncidentsDir(path) {
  // The incidents directory is the path's parent directory.
  const idx = path.lastIndexOf("/");
  return idx === -1 ? "." : path.slice(0, idx);
}

function basename(path) {
  const idx = path.lastIndexOf("/");
  return idx === -1 ? path : path.slice(idx + 1);
}

async function loadIncident(path, userInitiated) {
  const incidentsDir = getIncidentsDir(path);
  let incident;
  try {
    incident = await invoke("read_incident_json", { incidentsDir, path });
  } catch (err) {
    showError(`Failed to read incident: ${err}`);
    return;
  }

  if (incident.diagnosis) {
    await render(incident, path);
    return;
  }

  if (userInitiated) {
    setOriginNote("Investigation runs automatically when this window opens — no extra steps.");
    const updated = await runInvestigation(incident, incidentsDir, path);
    if (updated) await render(updated, path);
    return;
  }

  // Poller pre-navigation: the window's content is prepared ahead of the
  // notification (macOS notifications can't carry a click payload), but
  // nobody has looked at it yet. Render everything that costs nothing,
  // and wait for real user attention before spending tokens. The triggers
  // are armed *before* the process-tree query is awaited, so a click or
  // focus landing during that few-hundred-millisecond query isn't lost.
  setOriginNote("This window opened in the background — investigation starts once you bring it forward, or click Investigate now.");
  renderOrigin(incident);
  renderDiagnosis(incident);
  setDiagnosisText("New incident — nothing has been investigated yet. Bring this window forward, or start it here:");
  showInvestigateButton();
  armDeferredInvestigation(async () => {
    const updated = await runInvestigation(incident, incidentsDir, path);
    if (updated) await render(updated, path);
  });
  await renderTree(incident);
}

// The single place an investigation is actually started -- both the
// immediate (user-initiated arrival) and deferred (poller arrival, user
// showed up later) paths call this rather than duplicating the
// invoke/re-read/error handling. Returns the re-read incident on success,
// or `null` when it already reported the failure itself.
async function runInvestigation(incident, incidentsDir, path) {
  hideInvestigateButton();
  setThinking(true);
  let updated;
  try {
    await invoke("investigate", { alertKey: incident.alert_key, incidentsDir });
    updated = await invoke("read_incident_json", { incidentsDir, path });
  } catch (err) {
    setThinking(false);
    showError(`Investigation failed: ${err}`);
    // Give the user a way to retry instead of leaving a dead end -- once
    // `armDeferredInvestigation`'s triggers fire, they're torn down for
    // good (one-shot by design), so without this the only recovery from a
    // failed investigation is closing and reopening the window.
    showInvestigateButton();
    return null;
  }
  setThinking(false);

  if (!updated.diagnosis) {
    // `vigil investigate` takes an *alert key*, but this window is
    // addressed by *file path*, and the key resolves to whichever
    // incident file with that key was modified most recently. For a
    // repeated key (`high_load`, `high_process_count:<name>`, or
    // `cpu_hog:<pid>` after pid reuse) that can be a different, newer
    // file than the one on screen -- in which case the re-read above
    // finds no diagnosis. Say so explicitly instead of falling through
    // to `renderDiagnosis`'s generic "No diagnosis yet.", which would
    // look identical to never having investigated at all despite having
    // just spent real tokens.
    showError(
      `The investigation finished but ${basename(path)} still has no diagnosis. ` +
        `\`vigil investigate\` targets an alert key (${incident.alert_key}), not a file, so it may have written its answer to a more recent incident with the same key. ` +
        `Check \`vigil incidents\` for the latest one.`
    );
    showInvestigateButton();
    return null;
  }
  return updated;
}

// One-shot triggers for "the user is actually here now". Two kinds:
//
//  1. The "Investigate now" button. A click is unambiguous user action and
//     cannot misfire; it's also the only trigger verifiable without a real
//     user at a real display, so the waiting state always offers it.
//  2. The window being brought forward -- but only a focus the *user*
//     caused. `navigate_to_incident` calls `window.show()` for the poller
//     too, and this task's smoke run showed that a just-shown window can
//     come up focused on its own when the app happens to be active,
//     firing an investigation with nobody involved (exactly the bug this
//     whole gate exists to prevent, re-entering through the back door).
//     So a window that already has focus when this arms doesn't count:
//     it only counts once focus has been lost and the user comes back.
//
// Whichever fires first wins; every other listener is torn down so a later
// focus can't start a second agent session on top of the first.
function armDeferredInvestigation(start) {
  let fired = false;
  let unlistenTauriFocus = null;
  // False while the window holds a focus it was handed by `show()` rather
  // than by the user; flipped true the moment that focus is lost.
  let focusCounts = !document.hasFocus();
  const button = document.getElementById("investigate-now");
  const listeners = [];

  const run = () => {
    if (fired) return;
    fired = true;
    for (const [target, event, handler] of listeners) {
      target.removeEventListener(event, handler);
    }
    if (unlistenTauriFocus) {
      unlistenTauriFocus();
      unlistenTauriFocus = null;
    }
    start();
  };

  const onFocus = () => {
    if (focusCounts) run();
  };

  const on = (target, event, handler) => {
    target.addEventListener(event, handler);
    listeners.push([target, event, handler]);
  };

  on(button, "click", run);
  on(window, "blur", () => {
    focusCounts = true;
  });
  on(window, "focus", onFocus);
  on(document, "visibilitychange", () => {
    if (document.visibilityState === "visible") onFocus();
  });

  // Tauri's own window-focus event, as a belt to the DOM `focus` event's
  // braces: `tauri://focus` is a real emitted window event (verified in
  // tauri-2.11.5/src/manager/window.rs) and `listen` is permitted by this
  // app's `core:default` capability (which includes `core:event:default`).
  const listen = window.__TAURI__?.event?.listen;
  if (listen) {
    listen("tauri://blur", () => {
      focusCounts = true;
    }).catch(() => {});
    listen("tauri://focus", onFocus)
      .then((unlisten) => {
        if (fired) unlisten();
        else unlistenTauriFocus = unlisten;
      })
      .catch(() => {});
  }
}

// Renders a fully-diagnosed incident: origin, diagnosis, live process
// tree, and the proposed-fix card when there is one. Reached three ways --
// arrived with a diagnosis already present, a user-initiated arrival that
// just investigated, or a deferred (poller) arrival that just investigated
// via `armDeferredInvestigation` -- and in every one of them the
// investigation is already done, so the origin note settles on one fixed
// statement rather than whichever arrival-path text `loadIncident` set
// earlier (which would otherwise keep describing a state that's over).
async function render(incident, path) {
  setOriginNote("This incident has been investigated — see the diagnosis below.");
  renderOrigin(incident);
  renderDiagnosis(incident);
  hideInvestigateButton();
  await renderTree(incident);
  if (incident.proposed_fix) {
    renderFixCard(incident.proposed_fix, path);
  }
}

async function renderTree(incident) {
  if (incident.alert_key) {
    try {
      const tree = await invoke("process_tree", { alertKey: incident.alert_key });
      renderProcessTree(tree);
    } catch (err) {
      // Deliberately NOT `showError(...)` here -- by this point
      // `renderDiagnosis` has already written the real diagnosis into
      // `#diagnosis-body`, and `showError` targets that same element, so
      // reusing it here would silently replace correct, already-visible
      // diagnosis text with an unrelated process-tree error. The card
      // stays visible with an inline error in its own section instead, so
      // a process-tree failure surfaces without destroying content the
      // user already has.
      const card = document.getElementById("process-tree-card");
      card.style.display = "";
      document.getElementById("tree-container").textContent = `Failed to load process tree: ${err}`;
    }
  } else {
    document.getElementById("process-tree-card").style.display = "none";
  }
}

function showInvestigateButton() {
  document.getElementById("investigate-now-wrap").style.display = "";
}

function hideInvestigateButton() {
  document.getElementById("investigate-now-wrap").style.display = "none";
}

// The diagnosis card's body is this page's one status surface: the
// diagnosis itself, the "investigating…" placeholder, the deferred-start
// note, and any error all land here.
function setDiagnosisText(message) {
  document.getElementById("diagnosis-body").textContent = message;
}

// The origin card's static caption ("Investigation runs automatically...")
// is only true for a user-initiated arrival -- for the poller's deferred
// path it must say what actually happens (wait for attention, or click the
// button), or it directly contradicts the "Investigate now" state right
// below it.
function setOriginNote(message) {
  document.getElementById("origin-note").textContent = message;
}

function setThinking(isThinking) {
  setDiagnosisText(isThinking ? "Investigating…" : "");
}

function showError(message) {
  setDiagnosisText(message);
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
      <div class="proc-cell"><span class="proc-name-sm mono">${escapeHtml(node.name)}</span><span class="pid mono">${node.pid}</span></div>
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
    loadIncident(path, isUserInitiated());
  } else {
    showError("No incident path provided.");
  }
});
