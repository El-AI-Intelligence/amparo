#!/usr/bin/env node
// Mock LLM for amparo e2e — mirrors the scripted mock in
// crates/amparo-cli/tests/cli_e2e.rs (lines 126-330) exactly:
//   - POST <base>/v1/chat/completions
//   - body with "stream":true → scripted SSE `data:` frames + `data: [DONE]`
//   - non-stream (the verification call) → fixed "VERIFIED" completion
//
// Env:
//   MOCK_PORT      default 45199
//   MOCK_MODE      "content" (default): one content frame "Hello from the mock."
//                  "tool": first stream request asks for
//                  run_command {"command":"echo mock-tool"}; everything after
//                  answers with a content frame
//   MOCK_SCRIPTS   path to a JSON file: an array of scripts (each script an
//                  array of SSE frame objects) — overrides MOCK_MODE
//   MOCK_DELAY_MS  delay before the first response (checkpoint testing)
import http from 'node:http';
import fs from 'node:fs';

const PORT = parseInt(process.env.MOCK_PORT || '45199', 10);
const MODE = process.env.MOCK_MODE || 'content';
const DELAY = parseInt(process.env.MOCK_DELAY_MS || '0', 10);

const contentFrame = (text) => ({
  choices: [{ index: 0, delta: { content: text }, finish_reason: 'stop' }],
});

const toolScript = (tool, args) => [
  {
    choices: [{
      index: 0,
      delta: { tool_calls: [{ index: 0, id: 'call_1', type: 'function',
        function: { name: tool, arguments: '' } }] },
      finish_reason: null,
    }],
  },
  {
    choices: [{
      index: 0,
      delta: { tool_calls: [{ index: 0, function: { arguments: args } }] },
      finish_reason: 'tool_calls',
    }],
  },
];

// Scripts are consumed in order; the last one repeats (cli_e2e.rs semantics).
const scripts = process.env.MOCK_SCRIPTS
  ? JSON.parse(fs.readFileSync(process.env.MOCK_SCRIPTS, 'utf8'))
  : MODE === 'tool'
    ? [toolScript('run_command', '{"command":"echo mock-tool"}'),
       [contentFrame('Hello from the mock.')]]
    : [[contentFrame('Hello from the mock.')]];

let scriptIdx = 0;
let delayed = false;

const server = http.createServer((req, res) => {
  let body = '';
  req.on('data', (c) => (body += c));
  req.on('end', () => {
    const respond = () => {
      if (body.includes('"stream":true')) {
        const script = scripts[Math.min(scriptIdx, scripts.length - 1)];
        if (scriptIdx < scripts.length - 1) scriptIdx += 1;
        const out = script.map((f) => `data: ${JSON.stringify(f)}\n\n`).join('') +
          'data: [DONE]\n\n';
        res.writeHead(200, { 'Content-Type': 'text/event-stream' });
        res.end(out);
      } else {
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({
          choices: [{ index: 0, message: { role: 'assistant', content: 'VERIFIED' },
            finish_reason: 'stop' }],
          usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
        }));
      }
    };
    if (DELAY && !delayed) {
      delayed = true;
      setTimeout(respond, DELAY);
    } else {
      respond();
    }
  });
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`mock-inference: 127.0.0.1:${PORT} mode=${MODE} delay=${DELAY}`);
});
