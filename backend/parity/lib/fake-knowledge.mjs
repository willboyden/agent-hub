// A deterministic stand-in for the knowledge service, used as the hub's upstream (HUB_KNOWLEDGE_URL) so the proxy
// routes can be compared without a vector store. It checks the bearer the hub forwards.
import { createServer } from 'node:http';

export const FAKE_KNOWLEDGE_KEY = 'parity-fake-knowledge-admin-key-0123456789';

export function startFakeKnowledge() {
  const server = createServer((req, res) => {
    const url = new URL(req.url, 'http://x');
    const send = (status, body, type = 'application/json') => {
      res.writeHead(status, { 'content-type': type });
      res.end(typeof body === 'string' ? body : JSON.stringify(body));
    };
    let body = '';
    req.on('data', (c) => { body += c; });
    req.on('end', () => {
      if (url.pathname === '/health') return send(200, { ok: true, version: 'fake' });
      if (req.headers.authorization !== `Bearer ${FAKE_KNOWLEDGE_KEY}`) return send(401, { code: 'unauthorized' });
      if (url.pathname === '/backends') return send(200, { backends: { lancedb: { ok: true } } });
      if (url.pathname === '/namespaces' && req.method === 'GET') return send(200, { items: [{ name: 'docs', backend: 'lancedb' }], next_cursor: null });
      if (url.pathname === '/namespaces/docs/query' && req.method === 'POST') return send(200, { hits: [], echo_bytes: body.length });
      if (url.pathname === '/tokens') return send(200, { items: [] });
      if (url.pathname === '/jobs/j1/stream') {
        res.writeHead(200, { 'content-type': 'text/event-stream' });
        res.write('event: progress\ndata: {"done": 1}\n\n');
        res.end('event: end\ndata: {}\n\n');
        return undefined;
      }
      if (url.pathname === '/jobs/j2') return send(200, 'plain upstream text', 'text/plain');
      return send(404, { code: 'not_found', detail: `fake has no ${req.method} ${url.pathname}` });
    });
  });
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve({ server, port: server.address().port })));
}
