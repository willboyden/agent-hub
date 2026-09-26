// Tiny i18n: flat dotted keys, {var} interpolation, and _one/_other plural selection via vars.count.
let messages = {};
export function setMessages(m) { messages = m || {}; }
export async function loadMessages(url = 'i18n/en.json') {
  const r = await fetch(url);
  if (!r.ok) throw new Error(`i18n load failed: ${r.status}`);
  setMessages(await r.json());
}
export function t(key, vars) {
  let k = key;
  if (vars && typeof vars.count === 'number') {
    const pk = `${key}_${vars.count === 1 ? 'one' : 'other'}`;
    if (pk in messages) k = pk;
  }
  const s = messages[k];
  if (s == null) return key;
  return vars ? s.replace(/\{(\w+)\}/g, (m, n) => (n in vars ? String(vars[n]) : m)) : s;
}
