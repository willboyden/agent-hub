// Runs before first paint (classic script) so there is no theme flash. Storage may throw; default is dark.
(function () {
  var t = 'dark';
  try { t = localStorage.getItem('ah.theme') || 'dark'; } catch (e) { /* ignore */ }
  // ?theme=light|dark overrides for screenshots/tests; it is not persisted.
  var q = /[?&]theme=(light|dark)/.exec(location.search); if (q) t = q[1];
  if (t === 'system') t = window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
  document.documentElement.dataset.theme = t === 'light' ? 'light' : 'dark';
})();
