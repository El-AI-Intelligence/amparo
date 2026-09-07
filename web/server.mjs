#!/usr/bin/env node
// Amparo web surface — thin bridge over the shipped `amparo` binary.
//
// Binding contract: docs/web-surface.md v4. The gate chain
// (registry → trust ceiling → policy → human approval) lives entirely in
// the spawned `amparo` process; this app is a face, not a trust boundary.
// It holds no secrets on disk, never logs task bodies or approval
// arguments, and has no auto-approve path.
//
// Zero npm dependencies (Node >= 18 stdlib only).
//
// Env:
//   AMPARO_WEB_TOKEN        required — bearer token for /api/* and SSE
//   AMPARO_PORT             default 47910 (binds 127.0.0.1 only)
//   AMPARO_BIN              default "amparo" (PATH); /srv/amparo/bin/amparo in prod
//   AMPARO_WORKSPACES       default /srv/amparo/workspaces
//   AMPARO_OPERATOR         default "operator" — selects workspaces/<operator>/
//   AMPARO_APPROVAL_ENDPOINT when "1", append
//                           --approval-endpoint http://127.0.0.1:<port>/approvals
//                           to spawned runs (shipped in amparo v0.9.0)
//   AMPARO_APPROVAL_TOKEN   approval-scoped bearer for remote publishers
//                           (the fan-out gate) and receivers (R3) — absent,
//                           non-loopback approval traffic is refused
//   AMPARO_TASK_TIMEOUT_MS  default 1800000 (30 min) — queue guard, kills a
//                           wedged run so later queued tasks can proceed
//   AMPARO_INFERENCE_* / AMPARO_POLICY_KEY are inherited from the unit's
//   EnvironmentFile and passed through to the spawned process untouched.

import http from 'node:http';
import { spawn, spawnSync } from 'node:child_process';
import fsp from 'node:fs/promises';
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';

const PUBLIC_DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), 'public');

const PORT = parseInt(process.env.AMPARO_PORT || '47910', 10);
const TOKEN = process.env.AMPARO_WEB_TOKEN || '';
const AMPARO_BIN = process.env.AMPARO_BIN || 'amparo';
const WORKSPACES = process.env.AMPARO_WORKSPACES || '/srv/amparo/workspaces';
const OPERATOR = process.env.AMPARO_OPERATOR || 'operator';
const PASS_APPROVAL_ENDPOINT = process.env.AMPARO_APPROVAL_ENDPOINT === '1';
// R3: the approval-scoped secret. Remote publishers (the fan-out gate)
// and receivers (the Telegram bot) present it where the operator token
// is not theirs to hold; absent, remote approval traffic is refused.
const APPROVAL_TOKEN = process.env.AMPARO_APPROVAL_TOKEN || '';
const TASK_TIMEOUT_MS = parseInt(process.env.AMPARO_TASK_TIMEOUT_MS || '1800000', 10);
const APPROVAL_TTL_MS = 60_000; // contract §3: 60-second timeout, fail closed
const EVENT_CAP = 2000; // per-task ring buffer
const BODY_CAP = 64 * 1024;
const TICKET_TTL_MS = 30_000; // one-shot SSE tickets: short-lived by design
const MAX_TICKETS = 128; // bound the ticket map (audit MED-10)
const MAX_PENDING_APPROVALS = 64; // bound the pending queue (audit MED-11/LOW-13)
const RATE_WINDOW_MS = 60_000; // fixed-window rate limit (audit LOW-13)
const RATE_LIMIT_PER_WINDOW = 120; // POSTs per source address per window
const MAX_RATE_BUCKETS = 4096; // bound the rate-limit map

const TRUST_TIERS = new Set([
  'observational',
  'local_mutating',
  'external_effector',
  'system_control',
]);

if (!TOKEN) {
  console.error('amparo-web: AMPARO_WEB_TOKEN is required; refusing to start');
  process.exit(2);
}

// The shipped binary's own version, probed once at boot (public /version
// route). `amparo version` prints "amparo X.Y.Z"; the probe keeps the
// trailing token. Absent binary → null → the update banner stays hidden.
const BINARY_VERSION = (() => {
  try {
    const out = spawnSync(AMPARO_BIN, ['version'], { timeout: 5000, encoding: 'utf8' });
    if (out.status === 0) {
      const m = out.stdout.trim().match(/amparo\s+(\S+)\s*$/);
      if (m) return m[1];
    }
  } catch {
    // the banner degrades to hidden
  }
  return null;
})();

const workspaceRoot = () => path.join(WORKSPACES, OPERATOR);
const stateDir = (...parts) => path.join(workspaceRoot(), '.amparo', ...parts);

// ── helpers ────────────────────────────────────────────────────────────────

function sendJson(res, code, obj, cacheControl = 'no-cache') {
  const body = JSON.stringify(obj);
  res.writeHead(code, {
    'Content-Type': 'application/json',
    'Content-Length': Buffer.byteLength(body),
    'Cache-Control': cacheControl,
  });
  res.end(body);
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    req.on('data', (c) => {
      size += c.length;
      if (size > BODY_CAP) {
        reject(new Error('body too large'));
        req.destroy();
        return;
      }
      chunks.push(c);
    });
    req.on('end', () => resolve(Buffer.concat(chunks)));
    req.on('error', reject);
  });
}

async function readJsonBody(req) {
  const buf = await readBody(req);
  try {
    return JSON.parse(buf.toString('utf8') || '{}');
  } catch {
    throw new Error('invalid JSON body');
  }
}

function tokenOk(candidate) {
  if (typeof candidate !== 'string' || candidate.length !== TOKEN.length) return false;
  return crypto.timingSafeEqual(Buffer.from(candidate), Buffer.from(TOKEN));
}

function authed(req) {
  const header = req.headers.authorization || '';
  return header.startsWith('Bearer ') && tokenOk(header.slice(7));
}

// R3: the approval-scoped bearer check, timing-safe like the operator's.
function approvalAuthed(req) {
  if (!APPROVAL_TOKEN) return false;
  const header = req.headers.authorization || '';
  const candidate = header.startsWith('Bearer ') ? header.slice(7) : '';
  return (
    typeof candidate === 'string' &&
    candidate.length === APPROVAL_TOKEN.length &&
    crypto.timingSafeEqual(Buffer.from(candidate), Buffer.from(APPROVAL_TOKEN))
  );
}

// The seam's loopback exemption must survive the Caddy proxy: proxied
// requests arrive from 127.0.0.1 with the client's real address as the
// LAST X-Forwarded-For hop (Caddy appends it; Caddy does not trust an
// inbound XFF). The backend binds loopback-only, so a direct peer is
// either the spawned gate (trusted) or Caddy itself — take the appended
// hop when it is there, fall back to the socket address otherwise.
function remoteAddress(req) {
  const xff = req.headers['x-forwarded-for'];
  if (typeof xff === 'string' && xff.trim() !== '') {
    const last = xff.split(',').pop().trim();
    if (/^[0-9a-fA-F:.]+$/.test(last)) return last;
  }
  return req.socket.remoteAddress;
}

// Loopback stays exempt from approval-token checks: the spawned web
// gate posts from 127.0.0.1 and holds no secrets by contract.
function isLoopback(req) {
  const a = remoteAddress(req);
  return a === '127.0.0.1' || a === '::1' || a === '::ffff:127.0.0.1';
}

// ── one-shot SSE tickets (audit 2026-08-31 MED-10) ─────────────────────────
//
// EventSource cannot set headers. The operator token must therefore never
// ride a URL (access logs, browser history): the UI fetches a one-shot
// ticket over the authed API and the SSE endpoint consumes exactly one.

const tickets = new Map(); // ticket -> expiresAt

function issueTicket() {
  const now = Date.now();
  for (const [t, exp] of tickets) {
    if (exp <= now) tickets.delete(t);
  }
  while (tickets.size >= MAX_TICKETS) {
    tickets.delete(tickets.keys().next().value);
  }
  const ticket = crypto.randomBytes(24).toString('base64url');
  tickets.set(ticket, now + TICKET_TTL_MS);
  return ticket;
}

// Consumed by the check itself — one-shot whether or not the stream
// actually opens: a dropped connection re-issues, never replays.
function consumeTicket(candidate) {
  if (typeof candidate !== 'string') return false;
  const expiresAt = tickets.get(candidate);
  tickets.delete(candidate);
  return expiresAt !== undefined && expiresAt > Date.now();
}

// ── rate limiting (audit 2026-08-31 LOW-13) ────────────────────────────────
//
// Fixed-window limiter over POSTs only: GETs stay unlimited because the
// gate polls and the UI polls by design. Bounds queue spam and decide
// hammering; token brute force was already infeasible (~190 bits).

const rateBuckets = new Map(); // source address -> { windowStart, count }

function tooMany(address) {
  const now = Date.now();
  let b = rateBuckets.get(address);
  if (!b || now - b.windowStart >= RATE_WINDOW_MS) {
    b = { windowStart: now, count: 0 };
    rateBuckets.set(address, b);
  }
  b.count += 1;
  if (rateBuckets.size > MAX_RATE_BUCKETS) {
    for (const [k, v] of rateBuckets) {
      if (now - v.windowStart >= RATE_WINDOW_MS) rateBuckets.delete(k);
    }
  }
  return b.count > RATE_LIMIT_PER_WINDOW;
}

// Tail-parse a .jsonl file: at most `n` parsed objects from the end.
// Corrupt lines are skipped (same posture as amparo's own readers).
async function tailJsonl(file, n) {
  let text;
  try {
    text = await fsp.readFile(file, 'utf8');
  } catch (e) {
    if (e.code === 'ENOENT') return null;
    throw e;
  }
  const lines = text.split('\n').filter((l) => l.trim() !== '');
  const tail = lines.slice(-n);
  const rows = [];
  for (const line of tail) {
    try {
      rows.push(JSON.parse(line));
    } catch {
      /* skip corrupt line */
    }
  }
  return rows;
}

// ── task runner (serialized per operator workspace) ───────────────────────

const tasks = new Map(); // id -> record
let queue = Promise.resolve();
let taskCounter = 0;

function pushEvent(rec, line) {
  rec.events.push({ seq: rec.events.length, ts: Date.now(), line });
  if (rec.events.length > EVENT_CAP) rec.events.shift();
  for (const res of rec.listeners) {
    res.write(`event: line\ndata: ${JSON.stringify(rec.events[rec.events.length - 1])}\n\n`);
  }
}

function finishTask(rec, exitCode, answer, timedOut) {
  rec.status = exitCode === 0 ? 'done' : exitCode === 2 ? 'usage_error' : 'failed';
  rec.exitCode = exitCode;
  rec.answer = answer;
  rec.timedOut = timedOut === true;
  rec.finishedAt = Date.now();
  const done = { exitCode, answer, timedOut: rec.timedOut, status: rec.status };
  for (const res of rec.listeners) {
    res.write(`event: done\ndata: ${JSON.stringify(done)}\n\n`);
    res.end();
  }
  rec.listeners.clear();
  // Contract: no server logs of task bodies — log lifecycle by id only.
  console.log(`amparo-web: task ${rec.id} exited ${exitCode} (${rec.status})`);
}

function buildArgv(spec) {
  const argv = ['run', '--workspace', workspaceRoot() + path.sep];
  if (spec.policyUrl) argv.push('--policy-url', spec.policyUrl);
  if (spec.trustCeiling) argv.push('--trust-ceiling', spec.trustCeiling);
  if (spec.maxSubAgents !== undefined) argv.push('--max-sub-agents', String(spec.maxSubAgents));
  if (spec.growth) argv.push('--growth');
  if (PASS_APPROVAL_ENDPOINT) {
    argv.push('--approval-endpoint', `http://127.0.0.1:${PORT}/approvals`);
  }
  if (spec.resume) {
    argv.push('--resume'); // takes no task — the prompt comes from the checkpoint
  } else {
    argv.push(spec.task);
  }
  return argv;
}

function runTask(rec) {
  return new Promise((resolve) => {
    rec.status = 'running';
    rec.startedAt = Date.now();
    for (const res of rec.listeners) {
      res.write(`event: status\ndata: ${JSON.stringify({ status: 'running' })}\n\n`);
    }

    const child = spawn(AMPARO_BIN, buildArgv(rec.spec), {
      cwd: workspaceRoot(),
      env: process.env, // AMPARO_INFERENCE_* / AMPARO_POLICY_KEY pass through untouched
      stdio: ['ignore', 'pipe', 'pipe'], // stdin closed: the web gate decides via the approval endpoint
    });

    let stdout = '';
    let stderrBuf = '';
    let timedOut = false;
    const killer = setTimeout(() => {
      timedOut = true;
      pushEvent(rec, '[web] task timeout — killing process');
      child.kill('SIGKILL');
    }, TASK_TIMEOUT_MS);

    child.stdout.on('data', (c) => {
      if (stdout.length < 1024 * 1024) stdout += c.toString('utf8');
    });
    child.stderr.on('data', (c) => {
      stderrBuf += c.toString('utf8');
      let idx;
      while ((idx = stderrBuf.indexOf('\n')) !== -1) {
        const line = stderrBuf.slice(0, idx).replace(/\r$/, '');
        stderrBuf = stderrBuf.slice(idx + 1);
        if (line !== '') pushEvent(rec, line);
      }
    });
    child.on('error', (err) => {
      clearTimeout(killer);
      pushEvent(rec, `[web] spawn failed: ${err.message}`);
      finishTask(rec, 1, stdout.trim(), timedOut);
      resolve();
    });
    child.on('close', (code) => {
      clearTimeout(killer);
      if (stderrBuf.trim() !== '') pushEvent(rec, stderrBuf.trim());
      finishTask(rec, code === null ? 1 : code, stdout.trim(), timedOut);
      resolve();
    });
  });
}

function submitTask(spec) {
  const id = `task-${Date.now()}-${++taskCounter}`;
  const rec = {
    id,
    status: 'queued',
    createdAt: Date.now(),
    startedAt: null,
    finishedAt: null,
    exitCode: null,
    answer: null,
    timedOut: false,
    events: [],
    listeners: new Set(),
    spec, // in-memory only; never logged
  };
  tasks.set(id, rec);
  // Serialize: one `amparo run` at a time per operator workspace (contract §2).
  queue = queue.then(() => runTask(rec));
  return rec;
}

function taskView(rec) {
  return {
    id: rec.id,
    status: rec.status,
    createdAt: rec.createdAt,
    startedAt: rec.startedAt,
    finishedAt: rec.finishedAt,
    exitCode: rec.exitCode,
    timedOut: rec.timedOut,
    answer: rec.answer,
    resume: rec.spec.resume === true,
  };
}

// ── approval seam (contract §3 — the shipped --approval-endpoint's endpoint) ──

const approvals = new Map(); // call_id -> record

function approvalView(a) {
  // Lazily enforce the 60s fail-closed timeout.
  if (a.status === 'pending' && Date.now() > a.expiresAt) {
    a.status = 'decided';
    a.decision = false;
    a.auto = true;
    a.decidedAt = a.expiresAt;
  }
  return {
    call_id: a.call_id,
    tool_name: a.tool_name,
    arguments: a.arguments,
    reasons: a.reasons,
    blast_radius: a.blast_radius,
    session_label: a.session_label,
    receivedAt: a.receivedAt,
    expiresAt: a.expiresAt,
    status: a.status,
    decision: a.decision,
    auto: a.auto,
  };
}

function gatePost(body) {
  if (!body || typeof body.call_id !== 'string' || typeof body.tool_name !== 'string') {
    const err = new Error('call_id and tool_name are required');
    err.code = 400;
    throw err;
  }
  // The poll route only accepts [A-Za-z0-9._-] as one path segment; a
  // call_id outside it could never be polled. Reject at registration so
  // the gate fails closed immediately rather than queueing a ghost
  // (audit 2026-08-31 MED-11).
  if (!/^[A-Za-z0-9._-]{1,256}$/.test(body.call_id)) {
    const err = new Error('call_id must match [A-Za-z0-9._-] (max 256)');
    err.code = 400;
    throw err;
  }
  // One registration per call_id (MED-11): a duplicate POST is rejected —
  // a loopback process cannot swap display copy over a pending decision,
  // and the gate fails closed on the non-2xx.
  if (approvals.has(body.call_id)) {
    const err = new Error('duplicate call_id');
    err.code = 409;
    throw err;
  }
  const now = Date.now();
  // Lazy-expire pending entries, then bound pending capacity (MED-11 /
  // LOW-13): beyond the cap the registration is refused and the gate
  // auto-denies instead of the queue growing without bound.
  let pending = 0;
  for (const v of approvals.values()) {
    if (v.status === 'pending' && now > v.expiresAt) {
      v.status = 'decided';
      v.decision = false;
      v.auto = true;
      v.decidedAt = v.expiresAt;
    }
    if (v.status === 'pending') pending += 1;
  }
  if (pending >= MAX_PENDING_APPROVALS) {
    const err = new Error('approval queue full');
    err.code = 429;
    throw err;
  }
  const rec = {
    call_id: body.call_id,
    tool_name: body.tool_name,
    arguments: body.arguments ?? null,
    reasons: Array.isArray(body.reasons) ? body.reasons : [],
    blast_radius: body.blast_radius ?? null,
    session_label: body.session_label ?? null,
    receivedAt: now,
    expiresAt: now + APPROVAL_TTL_MS,
    status: 'pending',
    decision: null,
    auto: false,
  };
  approvals.set(rec.call_id, rec);
  // Bound the map: drop decided records older than an hour.
  for (const [k, v] of approvals) {
    if (v.status === 'decided' && now - (v.decidedAt || v.receivedAt) > 3600_000) {
      approvals.delete(k);
    }
  }
  return { call_id: rec.call_id, status: 'pending' };
}

function gateGet(callId) {
  const rec = approvals.get(callId);
  if (!rec) return null;
  const view = approvalView(rec); // applies lazy expiry
  return view.status === 'pending'
    ? { status: 'pending' }
    : { status: 'decided', decision: view.decision };
}

// R3 deny-wins latch, shared by the operator decide route and the gate-seam
// decision POST (the fan-out gate's local presses land there): a deny is
// permanent — an approve press can never override it, and a late deny
// overrides an earlier approve. One decision per call_id wins (contract
// §4.2); the expiry check runs first so an overdue entry latches auto-deny
// before any press reads it. Returns `{ code, body }` for sendJson.
function applyDecision(rec, decision) {
  approvalView(rec); // applies lazy expiry first
  if (rec.status !== 'pending') {
    if (rec.decision === true && decision === false) {
      rec.decision = false;
      rec.auto = false;
      rec.decidedAt = Date.now();
      return { code: 200, body: { call_id: rec.call_id, status: 'decided', decision: false } };
    }
    return {
      code: 409,
      body: { error: 'already decided', decision: rec.decision, auto: rec.auto },
    };
  }
  rec.status = 'decided';
  rec.decision = decision;
  rec.decidedAt = Date.now();
  return { code: 200, body: { call_id: rec.call_id, status: 'decided', decision: rec.decision } };
}

// ── workspace readers (contract §4 views) ─────────────────────────────────

async function ledgerView(n) {
  const rows = await tailJsonl(stateDir('privacy', 'ledger.jsonl'), n);
  return { path: stateDir('privacy', 'ledger.jsonl'), rows: rows ?? [] };
}

async function sessionsView() {
  const base = stateDir('sessions');
  const out = [];
  let tenants = [];
  try {
    tenants = await fsp.readdir(base, { withFileTypes: true });
  } catch (e) {
    if (e.code === 'ENOENT') return out;
    throw e;
  }
  for (const tenant of tenants.filter((t) => t.isDirectory())) {
    const dir = path.join(base, tenant.name);
    for (const file of await fsp.readdir(dir)) {
      if (!file.endsWith('.json')) continue;
      try {
        const cp = JSON.parse(await fsp.readFile(path.join(dir, file), 'utf8'));
        // Contract §4: show status, started_at, task id — not the prompt.
        out.push({
          tenant: tenant.name,
          task_id: cp.task_id ?? null,
          status: cp.status ?? null,
          started_at: cp.started_at ?? null,
          parent_task_id: cp.parent_task_id ?? null,
        });
      } catch {
        /* corrupt checkpoint — skip */
      }
    }
  }
  out.sort((a, b) => (b.started_at || 0) - (a.started_at || 0));
  return out;
}

async function scheduleView() {
  const dir = stateDir('schedule');
  let files = [];
  try {
    files = await fsp.readdir(dir);
  } catch (e) {
    if (e.code === 'ENOENT') return [];
    throw e;
  }
  const out = [];
  for (const file of files.filter((f) => f.endsWith('.json'))) {
    try {
      out.push(JSON.parse(await fsp.readFile(path.join(dir, file), 'utf8')));
    } catch {
      /* skip corrupt */
    }
  }
  return out;
}

async function scheduleCancel(id) {
  if (!/^[A-Za-z0-9._-]+$/.test(id)) {
    const err = new Error('invalid schedule id');
    err.code = 400;
    throw err;
  }
  const file = path.join(stateDir('schedule'), `${id}.json`);
  let task;
  try {
    task = JSON.parse(await fsp.readFile(file, 'utf8'));
  } catch (e) {
    const err = new Error(e.code === 'ENOENT' ? 'not found' : 'corrupt schedule file');
    err.code = e.code === 'ENOENT' ? 404 : 500;
    throw err;
  }
  if (task.status !== 'pending') {
    const err = new Error(`cannot cancel: status is ${task.status}`);
    err.code = 409;
    throw err;
  }
  // Cancel = status change, never deletion (contract §4.5). Atomic write.
  task.status = 'cancelled';
  const tmp = `${file}.tmp-${process.pid}`;
  await fsp.writeFile(tmp, JSON.stringify(task, null, 2) + '\n');
  await fsp.rename(tmp, file);
  return task;
}

async function notebookRecords(n) {
  // records.jsonl is the cold archive — read-only here, always.
  const rows = await tailJsonl(stateDir('notebook', 'records.jsonl'), n);
  if (rows === null) return [];
  return rows.map((entry) => {
    let record = null;
    try {
      record = typeof entry.content === 'string' ? JSON.parse(entry.content) : entry.content;
    } catch {
      /* leave null */
    }
    return { id: entry.id ?? null, created_at: entry.created_at ?? null, record };
  });
}

async function notebookSkills() {
  const dir = stateDir('skills');
  const adoptedRows = (await tailJsonl(path.join(dir, 'adopted.jsonl'), 10_000)) ?? [];
  const proposals = (await tailJsonl(path.join(dir, 'proposals.jsonl'), 500)) ?? [];
  // Last event per (tenant, name) wins.
  const adopted = {};
  for (const row of adoptedRows) {
    const key = `${row.tenant_id ?? ''}/${row.name ?? ''}`;
    adopted[key] = row;
  }
  const candidates = [];
  try {
    for (const file of await fsp.readdir(path.join(dir, 'candidates'))) {
      if (!file.endsWith('.toml')) continue;
      candidates.push({
        name: file.replace(/\.toml$/, ''),
        toml: await fsp.readFile(path.join(dir, 'candidates', file), 'utf8'),
      });
    }
  } catch (e) {
    if (e.code !== 'ENOENT') throw e;
  }
  return { adopted: Object.values(adopted), proposals, candidates };
}

async function notebookRollup() {
  try {
    return JSON.parse(await fsp.readFile(stateDir('notebook', 'rollup.json'), 'utf8'));
  } catch (e) {
    if (e.code === 'ENOENT') return null;
    throw e;
  }
}

// ── statics ────────────────────────────────────────────────────────────────

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
};

async function serveStatic(req, res, pathname) {
  const rel = pathname === '/' ? 'landing.html' : pathname === '/app' ? 'index.html' : pathname.slice(1);
  const file = path.normalize(path.join(PUBLIC_DIR, rel));
  if (!file.startsWith(PUBLIC_DIR + path.sep)) {
    res.writeHead(403).end('forbidden');
    return;
  }
  try {
    const data = await fsp.readFile(file);
    res.writeHead(200, {
      'Content-Type': MIME[path.extname(file)] || 'application/octet-stream',
      'Content-Length': data.length,
      'Cache-Control': 'no-cache',
    });
    res.end(data);
  } catch {
    res.writeHead(404).end('not found');
  }
}

// ── router ─────────────────────────────────────────────────────────────────

// The SSE path pattern, shared by the auth guard (ticket fallback) and
// the events route itself — one definition, no drift.
const EVENTS_RE = /^\/api\/tasks\/([A-Za-z0-9-]+)\/events$/;

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, 'http://127.0.0.1');
  const p = url.pathname;

  try {
    // ── gate-facing approval seam (contract §3): loopback by convention
    // (the spawned web gate holds no secrets); Caddy blocks /approvals*
    // from the public vhost. R3: a remote fan-out gate publishes here
    // too — off loopback it must present the approval-scoped token.
    // Covers both path readings of the contract: POST /approvals and
    // POST <endpoint=.../approvals>/approvals.
    if (req.method === 'POST' && (p === '/approvals' || p === '/approvals/approvals')) {
      if (tooMany(remoteAddress(req))) {
        sendJson(res, 429, { error: 'rate limited' });
        return;
      }
      if (!isLoopback(req) && !approvalAuthed(req)) {
        sendJson(res, 401, { error: 'approval token required' });
        return;
      }
      const body = await readJsonBody(req);
      sendJson(res, 200, gatePost(body));
      return;
    }
    const gateMatch = p.match(/^\/approvals\/(?:approvals\/)?([A-Za-z0-9._-]+)$/);
    if (req.method === 'GET' && gateMatch) {
      // The poll rides the same wire as the publish: off loopback it
      // carries the same approval-scoped token.
      if (!isLoopback(req) && !approvalAuthed(req)) {
        sendJson(res, 401, { error: 'approval token required' });
        return;
      }
      const status = gateGet(gateMatch[1]);
      if (!status) sendJson(res, 404, { error: 'unknown call_id' });
      else sendJson(res, 200, status);
      return;
    }

    // R3: the fan-out gate's local presses land on the seam too —
    // POST /approvals/{call_id}/decision, mirroring the /api decide
    // route's deny-wins latch under the same loopback-or-token guard.
    const seamDecide = p.match(/^\/approvals\/(?:approvals\/)?([A-Za-z0-9._-]+)\/decision$/);
    if (req.method === 'POST' && seamDecide) {
      if (tooMany(remoteAddress(req))) {
        sendJson(res, 429, { error: 'rate limited' });
        return;
      }
      if (!isLoopback(req) && !approvalAuthed(req)) {
        sendJson(res, 401, { error: 'approval token required' });
        return;
      }
      const rec = approvals.get(seamDecide[1]);
      if (!rec) {
        sendJson(res, 404, { error: 'unknown call_id' });
        return;
      }
      const body = await readJsonBody(req);
      if (typeof body.decision !== 'boolean') {
        sendJson(res, 400, { error: 'decision must be a boolean' });
        return;
      }
      const { code, body: out } = applyDecision(rec, body.decision);
      sendJson(res, code, out);
      return;
    }

    // Public version probe (no token): the update banner compares the
    // shipped binary's version against the site's /version.json (served
    // by Caddy from /srv/amparo/site — never reaching this app).
    if (req.method === 'GET' && p === '/version') {
      sendJson(res, 200, { product: 'amparo', version: BINARY_VERSION });
      return;
    }

    // ── everything below requires the operator token ──
    if (p.startsWith('/api/')) {
      // Queue spam / brute-force throttle: POSTs are rate-limited per
      // source address before auth (audit LOW-13); GETs stay unlimited
      // because the gate polls and the UI polls by design.
      if (req.method === 'POST' && tooMany(remoteAddress(req))) {
        sendJson(res, 429, { error: 'rate limited' });
        return;
      }
      // R3: the decision route additionally accepts the approval-scoped
      // token — remote receivers (the Telegram bot) post presses here
      // without the operator token. R3b: the pending listing does too
      // (the receiver polls GET /api/approvals with the same token).
      const decideRoute = p.match(/^\/api\/approvals\/([A-Za-z0-9._-]+)\/decide$/);
      const listingRoute = req.method === 'GET' && p === '/api/approvals';
      if (!(approvalAuthed(req) && ((req.method === 'POST' && decideRoute) || listingRoute)) && !authed(req)) {
        // EventSource cannot set headers: the SSE endpoint accepts a
        // one-shot ?ticket= issued at POST /api/tickets — the operator
        // token itself never rides a URL (audit MED-10).
        const ticketOk =
          req.method === 'GET' &&
          EVENTS_RE.test(p) &&
          consumeTicket(url.searchParams.get('ticket'));
        if (!ticketOk) {
          sendJson(res, 401, { error: 'unauthorized' });
          return;
        }
      }

      if (req.method === 'GET' && p === '/api/health') {
        sendJson(res, 200, {
          ok: true,
          contract: 'web-surface.md v4',
          amparoBin: AMPARO_BIN,
          operator: OPERATOR,
          approvalEndpointPassthrough: PASS_APPROVAL_ENDPOINT,
        });
        return;
      }

      // One-shot SSE ticket for the event stream (audit MED-10): issued
      // over the authed API, consumed by exactly one stream open.
      if (req.method === 'POST' && p === '/api/tickets') {
        sendJson(res, 200, { ticket: issueTicket() }, 'no-store');
        return;
      }

      if (req.method === 'POST' && p === '/api/tasks') {
        const body = await readJsonBody(req);
        const spec = {};
        if (body.resume === true) {
          spec.resume = true;
        } else {
          if (typeof body.task !== 'string' || body.task.trim() === '') {
            sendJson(res, 400, { error: 'task is required (or resume: true)' });
            return;
          }
          if (body.task.length > 10_000) {
            sendJson(res, 400, { error: 'task too long' });
            return;
          }
          spec.task = body.task;
        }
        if (body.policyUrl !== undefined) {
          if (typeof body.policyUrl !== 'string' || !/^https?:\/\//.test(body.policyUrl)) {
            sendJson(res, 400, { error: 'policyUrl must be an http(s) URL' });
            return;
          }
          spec.policyUrl = body.policyUrl;
        }
        if (body.trustCeiling !== undefined) {
          if (!TRUST_TIERS.has(body.trustCeiling)) {
            sendJson(res, 400, {
              error: `trustCeiling must be one of ${[...TRUST_TIERS].join(', ')}`,
            });
            return;
          }
          spec.trustCeiling = body.trustCeiling;
        }
        if (body.maxSubAgents !== undefined) {
          const n = Number(body.maxSubAgents);
          if (!Number.isInteger(n) || n < 0 || n > 64) {
            sendJson(res, 400, { error: 'maxSubAgents must be an integer 0..64' });
            return;
          }
          spec.maxSubAgents = n;
        }
        if (body.growth === true) spec.growth = true;
        const rec = submitTask(spec);
        sendJson(res, 202, taskView(rec));
        return;
      }

      if (req.method === 'GET' && p === '/api/tasks') {
        sendJson(res, 200, { tasks: [...tasks.values()].map(taskView).reverse() });
        return;
      }

      const taskMatch = p.match(/^\/api\/tasks\/([A-Za-z0-9-]+)$/);
      if (req.method === 'GET' && taskMatch) {
        const rec = tasks.get(taskMatch[1]);
        if (!rec) sendJson(res, 404, { error: 'unknown task' });
        else sendJson(res, 200, taskView(rec));
        return;
      }

      const eventsMatch = p.match(EVENTS_RE);
      if (req.method === 'GET' && eventsMatch) {
        const rec = tasks.get(eventsMatch[1]);
        if (!rec) {
          sendJson(res, 404, { error: 'unknown task' });
          return;
        }
        res.writeHead(200, {
          'Content-Type': 'text/event-stream',
          'Cache-Control': 'no-cache',
          Connection: 'keep-alive',
        });
        res.write(': ok\n\n');
        for (const ev of rec.events) {
          res.write(`event: line\ndata: ${JSON.stringify(ev)}\n\n`);
        }
        if (rec.finishedAt) {
          res.write(
            `event: done\ndata: ${JSON.stringify({
              exitCode: rec.exitCode,
              answer: rec.answer,
              timedOut: rec.timedOut,
              status: rec.status,
            })}\n\n`
          );
          res.end();
          return;
        }
        if (rec.status === 'running') {
          res.write(`event: status\ndata: ${JSON.stringify({ status: 'running' })}\n\n`);
        }
        rec.listeners.add(res);
        const heartbeat = setInterval(() => res.write(': ping\n\n'), 25_000);
        req.on('close', () => {
          clearInterval(heartbeat);
          rec.listeners.delete(res);
        });
        return;
      }

      if (req.method === 'GET' && p === '/api/approvals') {
        sendJson(res, 200, {
          approvals: [...approvals.values()].map(approvalView).reverse().slice(0, 100),
        });
        return;
      }

      const decideMatch = p.match(/^\/api\/approvals\/([A-Za-z0-9._-]+)\/decide$/);
      if (req.method === 'POST' && decideMatch) {
        const rec = approvals.get(decideMatch[1]);
        if (!rec) {
          sendJson(res, 404, { error: 'unknown call_id' });
          return;
        }
        const body = await readJsonBody(req);
        if (typeof body.decision !== 'boolean') {
          sendJson(res, 400, { error: 'decision must be a boolean' });
          return;
        }
        // R3 deny-wins latch (shared with the gate-seam decision POST).
        const { code, body: out } = applyDecision(rec, body.decision);
        sendJson(res, code, out);
        return;
      }

      if (req.method === 'GET' && p === '/api/privacy-ledger') {
        const n = Math.min(parseInt(url.searchParams.get('n') || '200', 10) || 200, 1000);
        sendJson(res, 200, await ledgerView(n));
        return;
      }

      if (req.method === 'GET' && p === '/api/sessions') {
        sendJson(res, 200, { sessions: await sessionsView() });
        return;
      }

      if (req.method === 'GET' && p === '/api/schedule') {
        sendJson(res, 200, { schedule: await scheduleView() });
        return;
      }

      const cancelMatch = p.match(/^\/api\/schedule\/([A-Za-z0-9._-]+)\/cancel$/);
      if (req.method === 'POST' && cancelMatch) {
        sendJson(res, 200, await scheduleCancel(cancelMatch[1]));
        return;
      }

      if (req.method === 'GET' && p === '/api/notebook/records') {
        const n = Math.min(parseInt(url.searchParams.get('n') || '50', 10) || 50, 500);
        sendJson(res, 200, { records: await notebookRecords(n) });
        return;
      }

      if (req.method === 'GET' && p === '/api/notebook/skills') {
        sendJson(res, 200, await notebookSkills());
        return;
      }

      if (req.method === 'GET' && p === '/api/notebook/rollup') {
        sendJson(res, 200, { rollup: await notebookRollup() });
        return;
      }

      sendJson(res, 404, { error: 'not found' });
      return;
    }

    // ── statics (no auth: the shell holds no data; every API call does) ──
    if (req.method === 'GET') {
      await serveStatic(req, res, p);
      return;
    }
    sendJson(res, 405, { error: 'method not allowed' });
  } catch (err) {
    if (!res.headersSent) {
      sendJson(res, err.code && Number.isInteger(err.code) ? err.code : 500, {
        error: err.message,
      });
    } else {
      res.end();
    }
  }
});

await fsp.mkdir(workspaceRoot(), { recursive: true });
server.listen(PORT, '127.0.0.1', () => {
  console.log(
    `amparo-web: listening on 127.0.0.1:${PORT} ` +
      `(operator=${OPERATOR}, workspace=${workspaceRoot()}, amparo=${AMPARO_BIN}, ` +
      `approval-endpoint passthrough=${PASS_APPROVAL_ENDPOINT})`
  );
});
