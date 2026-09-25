// A deliberately small protocol peer for Rust integration tests. No npm dependencies.
const fs = require('node:fs');
const readline = require('node:readline');
const http = require('node:http');

const transport = process.argv[2];
const era = process.argv[3];
const meta = { 'io.modelcontextprotocol/serverInfo': { name: 'fake', version: '1' } };
const modern = era === 'modern';

function response(request) {
  if (request.method === 'notifications/cancelled') {
    if (process.env.CANCEL_LOG) fs.appendFileSync(process.env.CANCEL_LOG, 'cancelled\n');
    return null;
  }
  if (request.method === 'notifications/initialized') return null;
  const id = request.id;
  if (request.method === 'server/discover') {
    if (!modern) return { jsonrpc: '2.0', id, error: { code: -32601, message: 'method not found' } };
    return { jsonrpc: '2.0', id, result: {
      resultType: 'complete', supportedVersions: ['2026-07-28'],
      capabilities: { tools: {} }, ttlMs: 0, cacheScope: 'private', _meta: meta,
    } };
  }
  if (request.method === 'initialize') return { jsonrpc: '2.0', id, result: {
    protocolVersion: '2025-11-25', capabilities: { tools: {} }, serverInfo: { name: 'fake', version: '1' },
  } };
  if (request.method === 'tools/list') return { jsonrpc: '2.0', id, result: {
    ...(modern ? { resultType: 'complete' } : {}),
    tools: ['echo', 'deny', 'hang'].map(name => ({ name, inputSchema: { type: 'object' } })),
    ...(modern ? { _meta: meta } : {}),
  } };
  if (request.method === 'tools/call') {
    if (request.params.name === 'hang') return null;
    const denied = request.params.name === 'deny';
    return { jsonrpc: '2.0', id, result: {
      ...(modern ? { resultType: 'complete' } : {}),
      content: [{ type: 'text', text: denied ? 'denied' : request.params.arguments?.message ?? '' }],
      isError: denied,
      ...(modern ? { _meta: meta } : {}),
    } };
  }
  return { jsonrpc: '2.0', id, error: { code: -32601, message: 'method not found' } };
}

if (transport === 'stdio') {
  readline.createInterface({ input: process.stdin }).on('line', line => {
    let reply;
    try { reply = response(JSON.parse(line)); }
    catch { reply = { jsonrpc: '2.0', id: null, error: { code: -32700, message: 'parse error' } }; }
    if (reply) process.stdout.write(JSON.stringify(reply) + '\n');
  });
} else if (transport === 'http') {
  const server = http.createServer((req, res) => {
    if (req.method !== 'POST') { res.writeHead(405).end(); return; }
    let body = '';
    req.on('data', chunk => { body += chunk; });
    req.on('end', () => {
      let reply;
      try { reply = response(JSON.parse(body)); }
      catch { reply = { jsonrpc: '2.0', id: null, error: { code: -32700, message: 'parse error' } }; }
      if (!reply) { res.writeHead(202).end(); return; }
      res.writeHead(200, { 'content-type': 'application/json', 'mcp-session-id': 'fake-session' });
      res.end(JSON.stringify(reply));
    });
  });
  server.listen(0, '127.0.0.1', () => process.stdout.write(String(server.address().port) + '\n'));
}
