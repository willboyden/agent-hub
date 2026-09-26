// Memory inbox helpers. Inbox text comes from an untrusted agent: it is only ever displayed as text, and this heuristic flags entries that read
// like instructions to a future agent (the classic memory-poisoning shape). It is advisory: a human still decides.
const SECRET_WORDS = '(ssh|id_rsa|\\.aws|credentials?|api[_ -]?key|token|password|secret)s?';
const VERBS = '(send|upload|post|export|read|cat|print)';
const PATTERNS = [
  [/ignore (all |any |the )?(previous|prior|above|earlier) (instructions|rules|messages)/i, 'override'],
  [/\b(always|never) (send|upload|post|export|exfiltrate|email|leak)\b/i, 'exfil'],
  [new RegExp(`\\b${SECRET_WORDS}\\b.*\\b${VERBS}\\b`, 'i'), 'secrets'],
  [new RegExp(`\\b${VERBS}\\b.*\\b${SECRET_WORDS}\\b`, 'i'), 'secrets'],
  [/https?:\/\/[^\s]+/i, 'url'],
  [/\b(curl|wget)\b.*\|\s*(ba)?sh\b/i, 'pipe_shell'],
  [/\bdisable (the )?(sandbox|guard|hook|egress|firewall|audit)/i, 'disable_guard'],
  [/\bwhen (asked|the user asks)\b.*\b(anything|everything)\b/i, 'standing_order'],
];
export function flagSuspicious(text) {
  const s = String(text ?? '');
  const hits = new Set();
  for (const [re, tag] of PATTERNS) if (re.test(s)) hits.add(tag);
  // A bare URL is only informative; it becomes a warning together with another signal.
  if (hits.size === 1 && hits.has('url')) return { suspicious: false, reasons: ['url'] };
  return { suspicious: hits.size > 0, reasons: [...hits] };
}
export const filterInbox = (items, client) => (client ? items.filter((i) => i.client === client) : items);
