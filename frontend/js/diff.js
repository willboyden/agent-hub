// Unified-diff line classification and parsing (pure). The renderer never uses innerHTML: it builds text nodes per line.
// Before the first hunk, '---'/'+++' are file headers; inside a hunk a line starting '---' is a real deletion.
export function classifyLine(line, inHunk = true) {
  if (line.startsWith('@@')) return 'hunk';
  if (!inHunk && (line.startsWith('---') || line.startsWith('+++'))) return 'meta';
  if (line.startsWith('diff ') || line.startsWith('index ') || line.startsWith('new file') || line.startsWith('deleted file') || line.startsWith('similarity ') || line.startsWith('rename ')) return inHunk ? 'ctx' : 'meta';
  if (line.startsWith('\\')) return 'note'; // "\ No newline at end of file"
  if (line.startsWith('+')) return 'add';
  if (line.startsWith('-')) return 'del';
  return 'ctx';
}

export function parseUnified(text) {
  const out = [];
  let inHunk = false, o = 0, n = 0;
  const lines = String(text ?? '').replace(/\r\n?/g, '\n').split('\n');
  if (lines.length && lines[lines.length - 1] === '') lines.pop();
  for (const line of lines) {
    const type = classifyLine(line, inHunk);
    if (type === 'hunk') {
      inHunk = true;
      const m = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line);
      if (m) { o = +m[1]; n = +m[2]; }
      out.push({ type, text: line, oldNo: null, newNo: null });
    } else if (type === 'meta' || type === 'note') out.push({ type, text: line, oldNo: null, newNo: null });
    else if (type === 'add') out.push({ type, text: line.slice(1), oldNo: null, newNo: n++ });
    else if (type === 'del') out.push({ type, text: line.slice(1), oldNo: o++, newNo: null });
    else out.push({ type, text: inHunk ? line.slice(1) : line, oldNo: inHunk ? o++ : null, newNo: inHunk ? n++ : null });
  }
  return out;
}
export function diffStats(text) {
  let add = 0, del = 0;
  for (const l of parseUnified(text)) { if (l.type === 'add') add++; else if (l.type === 'del') del++; }
  return { add, del };
}
