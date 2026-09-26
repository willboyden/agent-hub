// SSE over fetch (EventSource cannot send an Authorization header). The parser is pure and unit-tested.
export class SSEParser {
  constructor() { this.buf = ''; this.cur = { event: 'message', data: [], id: undefined }; }
  feed(chunk) {
    this.buf += chunk;
    const out = [];
    let idx;
    while ((idx = this.buf.search(/\r\n|\n|\r/)) >= 0) {
      const m = /^(\r\n|\n|\r)/.exec(this.buf.slice(idx));
      const line = this.buf.slice(0, idx);
      // A bare trailing \r may be the first half of \r\n: wait for more input.
      if (m[1] === '\r' && idx + 1 === this.buf.length) break;
      this.buf = this.buf.slice(idx + m[1].length);
      if (line === '') {
        if (this.cur.data.length) out.push({ event: this.cur.event, data: this.cur.data.join('\n'), id: this.cur.id });
        this.cur = { event: 'message', data: [], id: this.cur.id };
        continue;
      }
      if (line.startsWith(':')) continue;
      const c = line.indexOf(':');
      const field = c < 0 ? line : line.slice(0, c);
      let value = c < 0 ? '' : line.slice(c + 1);
      if (value.startsWith(' ')) value = value.slice(1);
      if (field === 'data') this.cur.data.push(value);
      else if (field === 'event') this.cur.event = value;
      else if (field === 'id') this.cur.id = value;
    }
    return out;
  }
}

export const backoff = (attempt, base = 500, cap = 15000) => Math.min(cap, base * 2 ** Math.min(attempt, 10));

/**
 * openStream(url, opts) -> { close() }
 * opts: headers, method, body, json (default true), reconnect (default true), onEvent(ev), onOpen, onError(err), onClose,
 *       fetchImpl, sleep. Stops (no retry) on 4xx, on `data: [DONE]`, or on close().
 */
export function openStream(url, opts = {}) {
  const { headers = {}, method = 'GET', body, json = true, reconnect = true, onEvent = () => {}, onOpen, onError, onClose,
    fetchImpl = globalThis.fetch?.bind(globalThis), sleep = (ms) => new Promise((r) => setTimeout(r, ms)) } = opts;
  const ctl = new AbortController();
  let closed = false;
  const run = async () => {
    let attempt = 0, lastId, minWait = 0;
    while (!closed) {
      let fatal = false;
      try {
        const h = { Accept: 'text/event-stream', ...headers, ...(lastId ? { 'Last-Event-ID': lastId } : {}) };
        const res = await fetchImpl(url, { method, headers: h, body, signal: ctl.signal });
        if (!res.ok) {
          let detail = `stream ${res.status}`, code;
          // Surface the RFC 7807 detail (e.g. "instance X is loading") instead of a bare status.
          try { const p = JSON.parse(await res.text()); detail = p.detail || p.title || detail; code = p.code; } catch { /* not json */ }
          const err = Object.assign(new Error(detail), { status: res.status, code });
          onError?.(err);
          // 429 too_many_streams (server cap of 8) is not retryable: reconnecting would just hold another slot.
          if (res.status >= 400 && res.status < 500 && (res.status !== 429 || code === 'too_many_streams')) fatal = true;
          else { if (res.status === 429) minWait = 5000; throw err; }
        } else {
          attempt = 0; onOpen?.();
          const parser = new SSEParser();
          const dec = new TextDecoder();
          const reader = res.body.getReader();
          for (;;) {
            const { value, done } = await reader.read();
            if (done) break;
            for (const ev of parser.feed(dec.decode(value, { stream: true }))) {
              if (ev.id) lastId = ev.id;
              if (ev.data === '[DONE]') { onEvent({ event: 'done', data: null }); closed = true; break; }
              let data = ev.data;
              if (json) { try { data = JSON.parse(ev.data); } catch { /* keep raw text */ } }
              onEvent({ event: ev.event, data, id: ev.id });
            }
            if (closed) { try { await reader.cancel(); } catch { /* ignore */ } break; }
          }
        }
      } catch (e) {
        if (closed || e?.name === 'AbortError') break;
        if (!fatal && !(e && e.status)) onError?.(e);
      }
      if (fatal || closed || !reconnect) break;
      await sleep(Math.max(minWait, backoff(attempt++))); minWait = 0;
    }
    closed = true;
    onClose?.();
  };
  run();
  return { close() { closed = true; ctl.abort(); } };
}
