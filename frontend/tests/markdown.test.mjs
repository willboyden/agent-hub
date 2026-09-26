import test from 'node:test';
import assert from 'node:assert/strict';
import { renderMarkdown, escapeHtml } from '../js/markdown.js';

test('escapes raw HTML before anything else', () => {
  const out = renderMarkdown('<script>alert(1)</script> & <img src=x onerror=alert(1)>');
  assert.ok(!out.includes('<script'));
  assert.ok(!/<img/i.test(out));
  assert.match(out, /&lt;script&gt;/);
});

test('XSS corpus never yields live script/handler/js-url markup', () => {
  const corpus = [
    '<script>alert(1)</script>', '<img src=x onerror=alert(1)>', '<svg onload=alert(1)>', '"><script>x</script>',
    '[x](javascript:alert(1))', '[x](JaVaScRiPt:alert(1))', '[x]( javascript:alert(1))', '[x](data:text/html;base64,PHNjcmlwdD4=)',
    '[x](vbscript:msgbox)', '[x](//evil.example/x)', '[x](&#106;avascript:alert(1))', '[x](java\tscript:alert(1))',
    '`<script>`', '```html\n<script>alert(1)</script>\n```', '![x](javascript:alert(1))', '**<b onmouseover=alert(1)>x</b>**',
    '[a](https://e.com" onmouseover="alert(1))', '> <iframe src=javascript:alert(1)>', '| <script> |\n|---|\n| <img onerror=x> |',
    '# <script>', '- <script>', '[<img src=x onerror=alert(1)>](https://e.com)', '\u0000<script>\u0000',
  ];
  for (const c of corpus) {
    const out = renderMarkdown(c);
    assert.ok(!/<script/i.test(out), `script tag leaked for: ${JSON.stringify(c)} -> ${out}`);
    assert.ok(!/<(img|svg|iframe)\b/i.test(out), `raw element leaked for: ${JSON.stringify(c)} -> ${out}`);
    // every attribute we emit must be from our fixed set; no inline handlers
    assert.ok(!/<[a-z][^>]*\son\w+\s*=/i.test(out), `handler attr for: ${JSON.stringify(c)} -> ${out}`);
    assert.ok(!/href="\s*(javascript|data|vbscript):/i.test(out), `unsafe href for: ${JSON.stringify(c)} -> ${out}`);
    assert.ok(!/href="\/\//.test(out), `protocol-relative href for ${JSON.stringify(c)}`);
  }
});

test('safe links become anchors with rel=noopener', () => {
  const out = renderMarkdown('[docs](https://example.com/a?b=1&c=2)');
  assert.match(out, /<a href="https:\/\/example.com\/a\?b=1&amp;c=2" target="_blank" rel="noopener noreferrer">docs<\/a>/);
});

test('attribute-breaking quotes in URLs are escaped', () => {
  const out = renderMarkdown('[a](https://e.com/"onmouseover="x)');
  assert.ok(!out.includes('" onmouseover'));
  assert.ok(!/onmouseover="/.test(out));
});

test('inline formatting', () => {
  assert.equal(renderMarkdown('**b** *i* `c` ~~s~~'), '<p><strong>b</strong> <em>i</em> <code>c</code> <del>s</del></p>');
});

test('inline code content is not formatted and stays escaped', () => {
  assert.equal(renderMarkdown('`**x** <b>`'), '<p><code>**x** &lt;b&gt;</code></p>');
});

test('headings, lists, blockquote, hr', () => {
  const out = renderMarkdown('# T\n\n- a\n- b\n\n1. x\n2. y\n\n> q\n\n---');
  assert.match(out, /<h3 class="md-h">T<\/h3>/);
  assert.match(out, /<ul><li>a<\/li><li>b<\/li><\/ul>/);
  assert.match(out, /<ol><li>x<\/li><li>y<\/li><\/ol>/);
  assert.match(out, /<blockquote><p>q<\/p><\/blockquote>/);
  assert.match(out, /<hr>/);
});

test('nested lists', () => {
  const out = renderMarkdown('- a\n  - b\n- c');
  assert.match(out, /<li>a<ul><li>b<\/li><\/ul><\/li><li>c<\/li>/);
});

test('fenced code is escaped and labelled; unterminated fence (streaming) is tolerated', () => {
  const out = renderMarkdown('```js\nconst a = "<b>";\n```');
  assert.match(out, /<code>const a = &quot;&lt;b&gt;&quot;;<\/code>/);
  assert.match(out, /<span>js<\/span>/);
  const open = renderMarkdown('```py\nprint(1)');
  assert.match(open, /<code>print\(1\)<\/code>/);
});

test('code fence language label cannot inject markup', () => {
  const out = renderMarkdown('```a"><script>\nx\n```');
  assert.ok(!/<script/i.test(out));
});

test('tables', () => {
  const out = renderMarkdown('| a | b |\n|---|--:|\n| 1 | 2 |');
  assert.match(out, /<th>a<\/th><th class="al-right">b<\/th>/);
  assert.match(out, /<td>1<\/td><td class="al-right">2<\/td>/);
});

test('soft line breaks and paragraphs', () => {
  assert.equal(renderMarkdown('a\nb\n\nc'), '<p>a<br>b</p><p>c</p>');
});

test('empty / non-string input', () => {
  assert.equal(renderMarkdown(''), '');
  assert.equal(renderMarkdown(null), '');
  assert.equal(escapeHtml(`<>&"'`), '&lt;&gt;&amp;&quot;&#39;');
});

test('copy label is escaped', () => {
  assert.ok(!renderMarkdown('```\nx\n```', { copyLabel: '<b>' }).includes('<b>'));
});
