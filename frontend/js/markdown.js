// Small, safe markdown renderer. SAFETY MODEL: every character of user text is HTML-escaped BEFORE any
// markup is generated, and the only tags emitted are the fixed ones below. Link targets are allow-listed
// by scheme. Never pass the output of anything else to innerHTML.
export function escapeHtml(s) {
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}

const SAFE_URL = /^(https?:\/\/|mailto:|#|\/(?!\/))[^\s\u0000-\u001f]*$/i;

function inline(src) {
  const codes = [];
  let s = src.replace(/\u0000/g, '');
  s = s.replace(/(`+)([^`]|[^`][\s\S]*?[^`])\1(?!`)/g, (_, __, body) => {
    codes.push(`<code>${escapeHtml(body.trim())}</code>`);
    return `\u0000${codes.length - 1}\u0000`;
  });
  s = escapeHtml(s);
  s = s.replace(/\[([^\]\n]+)\]\(([^)\s]+)\)/g, (m, text, url) => (SAFE_URL.test(url)
    ? `<a href="${url}" target="_blank" rel="noopener noreferrer">${text}</a>` : m));
  s = s.replace(/\*\*([^\s*](?:[\s\S]*?[^\s*])?)\*\*/g, '<strong>$1</strong>')
    .replace(/(^|[^\w])__([^\s_](?:[\s\S]*?[^\s_])?)__(?!\w)/g, '$1<strong>$2</strong>')
    .replace(/(^|[^*\w])\*([^\s*](?:[^*]*?[^\s*])?)\*(?!\*)/g, '$1<em>$2</em>')
    .replace(/(^|[^\w])_([^\s_](?:[^_]*?[^\s_])?)_(?!\w)/g, '$1<em>$2</em>')
    .replace(/~~([^\s~](?:[\s\S]*?[^\s~])?)~~/g, '<del>$1</del>');
  return s.replace(/\u0000(\d+)\u0000/g, (_, i) => codes[+i]);
}

const RE = {
  fence: /^\s{0,3}(`{3,}|~{3,})\s*([\w+#.-]*)\s*$/,
  heading: /^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$/,
  hr: /^\s{0,3}([-*_])(?:\s*\1){2,}\s*$/,
  quote: /^\s{0,3}>\s?(.*)$/,
  item: /^(\s*)([-*+]|\d{1,9}[.)])\s+(.*)$/,
  tableSep: /^\s*\|?\s*:?-{2,}:?\s*(\|\s*:?-{2,}:?\s*)*\|?\s*$/,
};

function splitRow(line) {
  let s = line.trim();
  if (s.startsWith('|')) s = s.slice(1);
  if (s.endsWith('|') && !s.endsWith('\\|')) s = s.slice(0, -1);
  return s.split(/(?<!\\)\|/).map((c) => c.trim().replace(/\\\|/g, '|'));
}

function startsBlock(line) {
  return RE.fence.test(line) || RE.heading.test(line) || RE.hr.test(line) || RE.quote.test(line) || RE.item.test(line);
}

function blocks(lines, opts) {
  const out = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) { i++; continue; }
    let m = RE.fence.exec(line);
    if (m) {
      const fence = m[1], lang = m[2];
      const body = [];
      i++;
      // Unterminated fence (common while streaming) runs to the end of input.
      while (i < lines.length && !(lines[i].trim().startsWith(fence[0].repeat(fence.length)) && lines[i].trim().replace(/[`~]/g, '') === '')) body.push(lines[i++]);
      i++;
      const label = lang ? escapeHtml(lang) : 'text';
      out.push(`<div class="codeblock"><div class="codehead"><span>${label}</span><button type="button" class="btn ghost sm" data-action="copy-code">${escapeHtml(opts.copyLabel)}</button></div><pre><code>${escapeHtml(body.join('\n'))}</code></pre></div>`);
      continue;
    }
    if ((m = RE.heading.exec(line))) { out.push(`<h${m[1].length + 2} class="md-h">${inline(m[2])}</h${m[1].length + 2}>`); i++; continue; }
    if (RE.hr.test(line)) { out.push('<hr>'); i++; continue; }
    if (RE.quote.test(line)) {
      const q = [];
      while (i < lines.length && RE.quote.test(lines[i])) q.push(RE.quote.exec(lines[i++])[1]);
      out.push(`<blockquote>${blocks(q, opts)}</blockquote>`);
      continue;
    }
    if (line.includes('|') && i + 1 < lines.length && RE.tableSep.test(lines[i + 1]) && lines[i + 1].includes('-')) {
      const head = splitRow(line);
      const aligns = splitRow(lines[i + 1]).map((c) => (/^:-+:$/.test(c) ? 'center' : /-:$/.test(c) ? 'right' : ''));
      i += 2;
      const rows = [];
      while (i < lines.length && lines[i].trim() && lines[i].includes('|')) rows.push(splitRow(lines[i++]));
      const cell = (tag, c, k) => `<${tag}${aligns[k] ? ` class="al-${aligns[k]}"` : ''}>${inline(c)}</${tag}>`;
      out.push(`<div class="tablewrap"><table><thead><tr>${head.map((c, k) => cell('th', c, k)).join('')}</tr></thead><tbody>${rows.map((r) => `<tr>${head.map((_, k) => cell('td', r[k] ?? '', k)).join('')}</tr>`).join('')}</tbody></table></div>`);
      continue;
    }
    if ((m = RE.item.exec(line))) {
      const base = m[1].length;
      const ordered = /\d/.test(m[2]);
      const lis = [];
      while (i < lines.length) {
        const im = RE.item.exec(lines[i]);
        if (!im || im[1].length !== base || /\d/.test(im[2]) !== ordered) break;
        const body = [im[3]];
        i++;
        const sub = [];
        while (i < lines.length && lines[i].trim() && (lines[i].match(/^\s*/)[0].length > base)) sub.push(lines[i++].slice(Math.min(base + 2, lines[i - 1].match(/^\s*/)[0].length)));
        lis.push(`<li>${inline(body[0])}${sub.length ? blocks(sub, opts) : ''}</li>`);
        while (i < lines.length && !lines[i].trim() && i + 1 < lines.length && RE.item.test(lines[i + 1]) && RE.item.exec(lines[i + 1])[1].length === base) i++;
      }
      out.push(`<${ordered ? 'ol' : 'ul'}>${lis.join('')}</${ordered ? 'ol' : 'ul'}>`);
      continue;
    }
    const para = [];
    while (i < lines.length && lines[i].trim() && !(para.length && startsBlock(lines[i]))) para.push(lines[i++]);
    out.push(`<p>${para.map(inline).join('<br>')}</p>`);
  }
  return out.join('');
}

export function renderMarkdown(md, opts = {}) {
  const o = { copyLabel: 'Copy', ...opts };
  return blocks(String(md ?? '').replace(/\r\n?/g, '\n').split('\n'), o);
}
