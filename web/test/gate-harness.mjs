#!/usr/bin/env node
// Gate harness — simulates the shipped `--approval-endpoint` gate against the
// app's approval surface (contract §3). Exercises the round-trip the real
// gate performs: POST registration, poll, decide, double-press, expiry.
//
// Usage: AMPARO_WEB_TOKEN=… node gate-harness.mjs <base> <mode>
//   base: e.g. http://127.0.0.1:47911
//   mode: approve | deny | expire | double
//
// Exit 0 when the observed outcome matches the mode's expectation.
const [base, mode] = process.argv.slice(2);
const TOKEN = process.env.AMPARO_WEB_TOKEN || '';
if (!base || !mode) {
  console.error('usage: gate-harness.mjs <base> <approve|deny|expire|double>');
  process.exit(2);
}

const request = {
  call_id: `call-harness-${mode}-${Date.now()}`,
  tool_name: 'run_command',
  arguments: { command: 'git push origin main' },
  reasons: ['policy escalated: needs human review'],
  blast_radius: 'destructive',
  session_label: 'sub-agent sess-123.1 of task sess-123',
};

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function decide(decision) {
  const res = await fetch(`${base}/api/approvals/${request.call_id}/decide`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${TOKEN}` },
    body: JSON.stringify({ decision }),
  });
  return res.status;
}

async function poll(timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const res = await fetch(`${base}/approvals/${request.call_id}`);
    const body = await res.json();
    if (body.status === 'decided') return body;
    await sleep(1000);
  }
  return null;
}

const post = await fetch(`${base}/approvals`, {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify(request),
});
const ack = await post.json();
if (post.status !== 200 || ack.status !== 'pending' || ack.call_id !== request.call_id) {
  console.error(`FAIL: POST ack ${post.status} ${JSON.stringify(ack)}`);
  process.exit(1);
}

let expected;
if (mode === 'approve' || mode === 'deny') {
  await sleep(500);
  const status = await decide(mode === 'approve');
  if (status !== 200) {
    console.error(`FAIL: decide returned ${status}`);
    process.exit(1);
  }
  expected = mode === 'approve';
} else if (mode === 'double') {
  await sleep(500);
  const first = await decide(true);
  const second = await decide(false);
  if (first !== 200 || second !== 409) {
    console.error(`FAIL: double press: first=${first} second=${second} (want 200/409)`);
    process.exit(1);
  }
  expected = true;
} else if (mode === 'expire') {
  expected = false; // no decision: 60s fail-closed
} else {
  console.error(`unknown mode ${mode}`);
  process.exit(2);
}

const outcome = await poll(mode === 'expire' ? 75_000 : 15_000);
if (!outcome) {
  console.error('FAIL: poll timed out');
  process.exit(1);
}
if (outcome.decision !== expected) {
  console.error(`FAIL: decision=${outcome.decision}, want ${expected}`);
  process.exit(1);
}
console.log(`ok: ${mode} → decided ${outcome.decision}`);
