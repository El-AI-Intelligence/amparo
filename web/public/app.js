/* Amparo operator surface — no-build SPA.
 * Contract: docs/web-surface.md v4. Token lives in a JS variable only —
 * never localStorage/sessionStorage, and approval arguments are never
 * persisted client-side (invariant I6, contract §5).
 */
'use strict';

let token = null; // memory only
let currentSource = null; // active EventSource
let streamGen = 0; // supersession guard: an in-flight stream open is stale
let pollTimer = null;

const view = document.getElementById('view');
const authGate = document.getElementById('auth-gate');

// ── tiny DOM helpers (no innerHTML with live data) ─────────────────────────

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

function clear(node) {
  while (node.firstChild) node.removeChild(node.firstChild);
}

function ts(ms) {
  if (!ms) return '—';
  return new Date(ms).toISOString().replace('T', ' ').replace(/\..*/, 'Z');
}

// ── API ────────────────────────────────────────────────────────────────────

async function api(method, path, body) {
  const res = await fetch(path, {
    method,
    headers: {
      Authorization: `Bearer ${token}`,
      ...(body !== undefined ? { 'Content-Type': 'application/json' } : {}),
    },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const data = await res.json().catch(() => ({}));
  if (res.status === 401) {
    showAuth();
    throw new Error('unauthorized');
  }
  if (!res.ok) throw new Error(data.error || `HTTP ${res.status}`);
  return data;
}

function showAuth() {
  token = null;
  authGate.classList.remove('hidden');
  view.classList.add('hidden');
}

document.getElementById('token-form').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const candidate = document.getElementById('token-input').value;
  document.getElementById('token-input').value = '';
  try {
    const res = await fetch('/api/health', {
      headers: { Authorization: `Bearer ${candidate}` },
    });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    token = candidate;
    authGate.classList.add('hidden');
    view.classList.remove('hidden');
    document.getElementById('token-error').textContent = '';
    route();
  } catch {
    document.getElementById('token-error').textContent = 'token rejected';
  }
});

// ── router ─────────────────────────────────────────────────────────────────

const routes = { run: viewRun, approvals: viewApprovals, privacy: viewPrivacy,
  sessions: viewSessions, schedule: viewSchedule, notebook: viewNotebook };

function route() {
  if (currentSource) { currentSource.close(); currentSource = null; }
  if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
  const name = (location.hash.replace(/^#\//, '') || 'run').split('/')[0];
  for (const a of document.querySelectorAll('#nav a')) {
    a.classList.toggle('active', a.dataset.route === name);
  }
  if (!token) return; // wait for auth
  clear(view);
  (routes[name] || viewRun)();
}

window.addEventListener('hashchange', route);

// ── Run view (contract §4.1) ───────────────────────────────────────────────

function viewRun() {
  const panel = el('section', 'panel');
  panel.appendChild(el('h2', null, 'task runner — one amparo run at a time per workspace'));

  const form = el('form');
  const taskLabel = el('label', null, 'task');
  const taskInput = el('textarea');
  taskInput.required = true;
  taskLabel.appendChild(taskInput);

  const row = el('div', 'row');
  const policyDiv = el('div');
  policyDiv.appendChild(el('label', null, 'policy url (absent = deny-all policy)'));
  const policyInput = el('input');
  policyInput.type = 'text';
  policyInput.placeholder = 'https://policy.example/…';
  policyDiv.appendChild(policyInput);

  const tierDiv = el('div');
  tierDiv.appendChild(el('label', null, 'trust ceiling'));
  const tierSel = el('select');
  for (const t of ['system_control', 'external_effector', 'local_mutating', 'observational']) {
    const o = el('option', null, t);
    o.value = t;
    tierSel.appendChild(o);
  }
  tierDiv.appendChild(tierSel);

  const subDiv = el('div');
  subDiv.appendChild(el('label', null, 'max sub-agents (0 disables swarms)'));
  const subInput = el('input');
  subInput.type = 'number';
  subInput.min = '0';
  subInput.max = '64';
  subInput.value = '4';
  subDiv.appendChild(subInput);

  row.append(policyDiv, tierDiv, subDiv);

  const flagsRow = el('div', 'row');
  const growthLabel = el('label');
  const growthBox = el('input');
  growthBox.type = 'checkbox';
  growthLabel.append(growthBox, document.createTextNode(' --growth (record to notebook)'));
  const resumeLabel = el('label');
  const resumeBox = el('input');
  resumeBox.type = 'checkbox';
  resumeLabel.append(resumeBox, document.createTextNode(' --resume (newest Running checkpoint; no task text)'));
  flagsRow.append(growthLabel, resumeLabel);

  const submit = el('button', 'primary', 'start task');
  submit.type = 'submit';
  const err = el('p', 'error');

  form.append(taskLabel, row, flagsRow, submit, err);
  panel.appendChild(form);

  const listPanel = el('section', 'panel');
  listPanel.appendChild(el('h2', null, 'tasks (this server process)'));
  const listDiv = el('div', 'tasklist');
  listPanel.appendChild(listDiv);

  const streamPanel = el('section', 'panel');
  streamPanel.appendChild(el('h2', null, 'event stream — stderr [tag] lines, live'));
  const stream = el('div');
  stream.id = 'stream';
  const answerBox = el('div');
  streamPanel.append(stream, answerBox);

  view.append(panel, listPanel, streamPanel);

  resumeBox.addEventListener('change', () => { taskInput.disabled = resumeBox.checked; });

  form.addEventListener('submit', async (ev) => {
    ev.preventDefault();
    err.textContent = '';
    const body = {};
    if (resumeBox.checked) body.resume = true;
    else body.task = taskInput.value;
    if (policyInput.value.trim()) body.policyUrl = policyInput.value.trim();
    body.trustCeiling = tierSel.value;
    body.maxSubAgents = parseInt(subInput.value, 10);
    if (growthBox.checked) body.growth = true;
    try {
      const rec = await api('POST', '/api/tasks', body);
      attachStream(rec.id, stream, answerBox);
      refreshTasks();
    } catch (e2) {
      err.textContent = e2.message;
    }
  });

  async function refreshTasks() {
    try {
      const { tasks } = await api('GET', '/api/tasks');
      clear(listDiv);
      for (const t of tasks.slice(0, 20)) {
        const b = el('button', null,
          `${t.id} — ${t.status}${t.exitCode !== null ? ` (exit ${t.exitCode})` : ''}`);
        b.addEventListener('click', () => attachStream(t.id, stream, answerBox));
        listDiv.appendChild(b);
      }
      if (tasks.length === 0) listDiv.appendChild(el('p', 'note', 'no tasks yet'));
    } catch { /* surfaced on next interaction */ }
  }
  refreshTasks();
}

function attachStream(taskId, stream, answerBox) {
  if (currentSource) currentSource.close();
  clear(stream);
  clear(answerBox);
  stream.appendChild(el('div', 'note', `— attached to ${taskId} —`));
  const gen = ++streamGen;
  let retries = 0;

  async function open() {
    // The token never rides a URL (access logs, browser history): the
    // SSE endpoint takes a one-shot ticket issued over the authed API
    // (contract v4).
    let ticket;
    try {
      ({ ticket } = await api('POST', '/api/tickets'));
    } catch (e) {
      stream.appendChild(el('div', 'error', e.message));
      return;
    }
    const src = new EventSource(`/api/tasks/${taskId}/events?ticket=${encodeURIComponent(ticket)}`);
    if (gen !== streamGen) { src.close(); return; } // superseded while fetching
    currentSource = src;
    src.addEventListener('line', (ev) => {
      const { line } = JSON.parse(ev.data);
      const m = line.match(/^\[([a-z]+)/);
      const div = el('div', m ? `tag-${m[1]}` : null, line);
      stream.appendChild(div);
      stream.scrollTop = stream.scrollHeight;
    });
    src.addEventListener('status', () => {
      stream.appendChild(el('div', 'note', '— running —'));
    });
    src.addEventListener('done', (ev) => {
      const done = JSON.parse(ev.data);
      src.close();
      const h = el('h2', null,
        `final answer — stdout, exit ${done.exitCode}${done.timedOut ? ' (killed: timeout)' : ''}`);
      const pre = el('pre', null, done.answer || '(empty)');
      answerBox.append(h, pre);
    });
    // A dropped connection cannot auto-retry: the ticket is one-shot and
    // the browser's replay would 401. Re-issue and re-open instead, a
    // bounded number of times, then stop and say so.
    src.onerror = () => {
      src.close();
      if (gen !== streamGen) return;
      retries += 1;
      if (retries >= 3) {
        stream.appendChild(el('div', 'error', 'stream lost — pick the task again to re-attach'));
        return;
      }
      open();
    };
  }
  open();
}

// ── Approvals view (contract §4.2, §3) ─────────────────────────────────────

function viewApprovals() {
  const panel = el('section', 'panel');
  panel.appendChild(el('h2', null,
    'approval queue — a human decides; 60s without a decision fails closed'));
  const list = el('div');
  panel.appendChild(list);
  view.appendChild(panel);

  async function refresh() {
    let data;
    try {
      data = await api('GET', '/api/approvals');
    } catch { return; }
    const pending = data.approvals.filter((a) => a.status === 'pending');
    const badge = document.getElementById('approval-badge');
    badge.classList.toggle('hidden', pending.length === 0);
    badge.textContent = String(pending.length);

    clear(list);
    if (data.approvals.length === 0) {
      list.appendChild(el('p', 'note',
        'queue empty — requests arrive from running tasks via --approval-endpoint'));
      return;
    }
    for (const a of data.approvals) {
      const card = el('section', 'panel');
      // Delegation chain copy (contract §3): "[session] <label> wants to run:"
      if (a.session_label) card.appendChild(el('p', null, `[session] ${a.session_label} wants to run:`));
      // Preflight copy: "[preflight] blast radius: <r>"
      if (a.blast_radius) {
        card.appendChild(el('p', null, `[preflight] blast radius: ${a.blast_radius}`));
      }
      card.appendChild(el('p', null, `[approval] ${a.tool_name}: ${(a.reasons || []).join(', ')}`));
      const argsLabel = el('p', 'note', 'arguments (verbatim — you approve the concrete consequence):');
      const args = el('pre', null, JSON.stringify(a.arguments, null, 2));
      card.append(argsLabel, args);

      if (a.status === 'pending') {
        const cd = el('span', 'countdown');
        const tick = () => {
          const left = Math.max(0, a.expiresAt - Date.now());
          cd.textContent = `auto-deny in ${(left / 1000).toFixed(0)}s`;
        };
        tick();
        const timer = setInterval(tick, 1000);
        setTimeout(() => clearInterval(timer), a.expiresAt - Date.now() + 500);
        const approve = el('button', 'approve', 'approve');
        const deny = el('button', 'deny', 'deny');
        const decide = async (decision) => {
          approve.disabled = deny.disabled = true;
          try {
            await api('POST', `/api/approvals/${a.call_id}/decide`, { decision });
          } catch { /* already decided — double press is safe */ }
          refresh();
        };
        approve.addEventListener('click', () => decide(true));
        deny.addEventListener('click', () => decide(false));
        card.append(cd, document.createTextNode(' '), approve, document.createTextNode(' '), deny);
      } else {
        card.appendChild(el('p', a.decision ? 'status-done' : 'status-failed',
          `decided: ${a.decision ? 'approved' : 'denied'}${a.auto ? ' (auto — 60s elapsed, fail closed)' : ''}`));
      }
      list.appendChild(card);
    }
  }
  refresh();
  pollTimer = setInterval(refresh, 2000);
}

// ── Privacy ledger view (contract §4.3) ────────────────────────────────────

function viewPrivacy() {
  const panel = el('section', 'panel');
  panel.appendChild(el('h2', null,
    'privacy ledger — counts never values; sites are scheme+host only'));
  const holder = el('div');
  panel.appendChild(holder);
  view.appendChild(panel);

  api('GET', '/api/privacy-ledger?n=200').then(({ rows }) => {
    clear(holder);
    if (!rows.length) {
      holder.appendChild(el('p', 'note', 'no ledger rows yet — run a task first'));
      return;
    }
    const table = el('table');
    const head = el('tr');
    for (const h of ['ts', 'tenant', 'task', 'kind', 'tool', 'site', 'outcome', 'gate', 'pii strips']) {
      head.appendChild(el('th', null, h));
    }
    table.appendChild(head);
    for (const r of rows.slice().reverse()) {
      const tr = el('tr');
      const counts = (r.pii_counts || []).map(([c, n]) => `${c} x${n}`).join(', ');
      for (const v of [r.ts, r.tenant, r.task_id || '—', r.kind, r.tool || '—',
        r.site || '—', r.outcome || '—', r.gate || '—', counts || '—']) {
        tr.appendChild(el('td', null, String(v)));
      }
      table.appendChild(tr);
    }
    holder.appendChild(table);
  }).catch((e) => holder.appendChild(el('p', 'error', e.message)));
}

// ── Sessions view (contract §4.4) ──────────────────────────────────────────

function viewSessions() {
  const panel = el('section', 'panel');
  panel.appendChild(el('h2', null, 'sessions — checkpoints under .amparo/sessions; a Running one can resume'));
  const holder = el('div');
  panel.appendChild(holder);
  view.appendChild(panel);

  api('GET', '/api/sessions').then(({ sessions }) => {
    clear(holder);
    if (!sessions.length) {
      holder.appendChild(el('p', 'note', 'no checkpoints yet'));
      return;
    }
    const table = el('table');
    const head = el('tr');
    for (const h of ['task id', 'tenant', 'status', 'started at', '']) head.appendChild(el('th', null, h));
    table.appendChild(head);
    for (const s of sessions) {
      const tr = el('tr');
      tr.appendChild(el('td', null, s.task_id || '—'));
      tr.appendChild(el('td', null, s.tenant));
      const st = el('td');
      st.appendChild(el('span', `status-pill status-${s.status}`, s.status || '?'));
      tr.appendChild(st);
      tr.appendChild(el('td', null, s.started_at ? ts(s.started_at * 1000) : '—'));
      const action = el('td');
      if (s.status === 'running') {
        const b = el('button', null, 'resume newest Running');
        b.addEventListener('click', async () => {
          b.disabled = true;
          try {
            await api('POST', '/api/tasks', { resume: true });
            location.hash = '#/run';
          } catch (e) {
            b.disabled = false;
            action.appendChild(el('p', 'error', e.message));
          }
        });
        action.appendChild(b);
      }
      tr.appendChild(action);
      table.appendChild(tr);
    }
    holder.appendChild(table);
  }).catch((e) => holder.appendChild(el('p', 'error', e.message)));
}

// ── Schedule view (contract §4.5) ──────────────────────────────────────────

function viewSchedule() {
  const panel = el('section', 'panel');
  panel.appendChild(el('h2', null,
    'schedule queue — cancel is a status change, never a deletion'));
  const holder = el('div');
  panel.appendChild(holder);
  view.appendChild(panel);

  async function refresh() {
    let data;
    try {
      data = await api('GET', '/api/schedule');
    } catch (e) {
      clear(holder);
      holder.appendChild(el('p', 'error', e.message));
      return;
    }
    clear(holder);
    if (!data.schedule.length) {
      holder.appendChild(el('p', 'note',
        'queue empty — promises are registered by the chat surfaces, not amparo run'));
      return;
    }
    const table = el('table');
    const head = el('tr');
    for (const h of ['id', 'platform', 'requester', 'task (stripped)', 'at', 'status', '']) {
      head.appendChild(el('th', null, h));
    }
    table.appendChild(head);
    for (const s of data.schedule) {
      const tr = el('tr');
      for (const v of [s.id, s.platform, s.requester, s.task, s.at]) {
        tr.appendChild(el('td', null, v == null ? '—' : String(v)));
      }
      const st = el('td');
      st.appendChild(el('span', `status-pill status-${s.status}`, s.status));
      tr.appendChild(st);
      const action = el('td');
      if (s.status === 'pending') {
        const b = el('button', null, 'cancel');
        b.addEventListener('click', async () => {
          b.disabled = true;
          try { await api('POST', `/api/schedule/${s.id}/cancel`); } catch { /* shown on refresh */ }
          refresh();
        });
        action.appendChild(b);
      }
      tr.appendChild(action);
      table.appendChild(tr);
    }
    holder.appendChild(table);
  }
  refresh();
}

// ── Notebook views (contract §4.6) ─────────────────────────────────────────

function viewNotebook() {
  const recordsPanel = el('section', 'panel');
  recordsPanel.appendChild(el('h2', null, 'run records — cold archive (read-only; --growth runs only)'));
  const recordsDiv = el('div');
  recordsPanel.appendChild(recordsDiv);

  const skillsPanel = el('section', 'panel');
  skillsPanel.appendChild(el('h2', null, 'skills'));
  const skillsDiv = el('div');
  skillsPanel.appendChild(skillsDiv);

  const rollupPanel = el('section', 'panel');
  rollupPanel.appendChild(el('h2', null, 'rollup state'));
  const rollupDiv = el('div');
  rollupPanel.appendChild(rollupDiv);

  view.append(recordsPanel, skillsPanel, rollupPanel);

  api('GET', '/api/notebook/records?n=50').then(({ records }) => {
    if (!records.length) {
      recordsDiv.appendChild(el('p', 'note', 'no records — start a task with --growth'));
      return;
    }
    const table = el('table');
    const head = el('tr');
    for (const h of ['started', 'task (stripped, ≤200)', 'calls', 'verification', 'status', 'cost est.']) {
      head.appendChild(el('th', null, h));
    }
    table.appendChild(head);
    for (const r of records.slice().reverse()) {
      const rec = r.record || {};
      const tr = el('tr');
      tr.appendChild(el('td', null, rec.started_at || r.created_at || '—'));
      tr.appendChild(el('td', null, rec.task_text || '—'));
      tr.appendChild(el('td', null, String((rec.tool_calls || []).length)));
      tr.appendChild(el('td', null, (rec.verification && rec.verification.decision) || '—'));
      tr.appendChild(el('td', null, rec.status || '—'));
      tr.appendChild(el('td', null,
        rec.token_cost_estimate != null ? `~$${Number(rec.token_cost_estimate).toFixed(4)}` : '—'));
      table.appendChild(tr);
    }
    recordsDiv.appendChild(table);
  }).catch((e) => recordsDiv.appendChild(el('p', 'error', e.message)));

  api('GET', '/api/notebook/skills').then((skills) => {
    if (!skills.adopted.length && !skills.proposals.length && !skills.candidates.length) {
      skillsDiv.appendChild(el('p', 'note', 'no skill activity yet'));
      return;
    }
    if (skills.adopted.length) {
      skillsDiv.appendChild(el('p', 'note', 'adopted (last event per skill):'));
      skillsDiv.appendChild(el('pre', null, JSON.stringify(skills.adopted, null, 2)));
    }
    if (skills.proposals.length) {
      skillsDiv.appendChild(el('p', 'note', 'proposals:'));
      skillsDiv.appendChild(el('pre', null, JSON.stringify(skills.proposals, null, 2)));
    }
    for (const c of skills.candidates) {
      skillsDiv.appendChild(el('p', 'note', `candidate: ${c.name}`));
      skillsDiv.appendChild(el('pre', null, c.toml));
    }
  }).catch((e) => skillsDiv.appendChild(el('p', 'error', e.message)));

  api('GET', '/api/notebook/rollup').then(({ rollup }) => {
    rollupDiv.appendChild(rollup
      ? el('pre', null, JSON.stringify(rollup, null, 2))
      : el('p', 'note', 'no rollup yet'));
  }).catch((e) => rollupDiv.appendChild(el('p', 'error', e.message)));
}

// ── update banner ──────────────────────────────────────────────────────────
// Compares the site's published version (/version.json — generated at
// deploy time, Caddy-served) against the shipped binary the app spawns
// (/version — public). Shows a dismissible strip when the site is newer;
// any failure (offline app, stale probe) just hides it. The dismissal is
// a bare version flag — the operator token invariant (I6) is untouched.

function versionTriple(v) {
  const m = String(v || '').split('-')[0].split('+')[0].match(/^(\d+)\.(\d+)\.(\d+)$/);
  return m ? [Number(m[1]), Number(m[2]), Number(m[3])] : null;
}

function tripleNewer(latest, current) {
  const l = versionTriple(latest);
  const c = versionTriple(current);
  if (!l || !c) return false;
  return l[0] > c[0] || (l[0] === c[0] && (l[1] > c[1] || (l[1] === c[1] && l[2] > c[2])));
}

const UPDATE_DISMISS_KEY = 'amparo-update-dismissed';

function showUpdateBanner(version, changelogUrl) {
  const banner = el('div', 'update-banner', null);
  const text = el('span', null, `amparo ${version} is available — `);
  const link = el('a', null, 'see the changelog');
  link.href = changelogUrl || '/changelog';
  link.rel = 'noopener';
  text.appendChild(link);
  const dismiss = el('button', 'update-dismiss', '✕');
  dismiss.setAttribute('aria-label', 'dismiss');
  dismiss.addEventListener('click', () => {
    try { localStorage.setItem(UPDATE_DISMISS_KEY, version); } catch { /* private mode */ }
    banner.remove();
  });
  banner.appendChild(text);
  banner.appendChild(dismiss);
  document.body.prepend(banner);
}

function checkForUpdate() {
  Promise.all([
    fetch('/version.json', { cache: 'no-store' }).then((r) => (r.ok ? r.json() : null)),
    fetch('/version', { cache: 'no-store' }).then((r) => (r.ok ? r.json() : null)),
  ])
    .then(([site, local]) => {
      if (!site || site.product !== 'amparo' || !local || local.product !== 'amparo') return;
      if (!tripleNewer(site.version, local.version)) return;
      let dismissed = null;
      try { dismissed = localStorage.getItem(UPDATE_DISMISS_KEY); } catch { /* private mode */ }
      if (dismissed === site.version) return;
      showUpdateBanner(site.version, site.changelog_url);
    })
    .catch(() => { /* the banner is best-effort; never block the surface */ });
}

// ── boot ───────────────────────────────────────────────────────────────────

showAuth();
checkForUpdate();
