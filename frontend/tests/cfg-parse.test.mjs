// cfg-parse.js: TOML 1.0, the block-YAML subset and env files. Expected values are what tomllib and PyYAML 6 load
// (ported from Nemo Console, where they were checked by hand against both). The hub's own check is the authority.
import test from 'node:test';
import assert from 'node:assert/strict';
import { parseConfig, parseToml, parseYaml, yamlPlainType, entryOf } from '../js/cfg-parse.js';

const val = (n) =>
  n.kind === 'map'
    ? Object.fromEntries(n.entries.map((e) => [e.key, val(e.node)]))
    : n.kind === 'seq'
      ? n.items.map(val)
      : n.value;
const ok = (fmt, text) => {
  const r = parseConfig(fmt, text);
  assert.equal(r.errors.length, 0, JSON.stringify(r.errors));
  assert.equal(r.unsupported, null);
  return val(r.root);
};
const err = (fmt, text) => {
  const r = parseConfig(fmt, text);
  assert.equal(r.root, null, 'expected a syntax error');
  assert.equal(r.errors.length, 1);
  return r.errors[0];
};
const unsup = (text) => {
  const r = parseYaml(text);
  assert.equal(r.root, null);
  assert.equal(r.errors.length, 0, 'a valid file must never be reported broken');
  return r.unsupported;
};

test('TOML: tables, dotted keys, arrays of tables, inline tables, every scalar kind', () => {
  assert.deepEqual(
    ok(
      'toml',
      `title = "x"\n[a.b]\nc = 1\nd.e = true\n[[r]]\nn = 1\n[[r]]\nn = 2\n[t]\ni = { x = 1, y = "z" }\narr = [ 1, 2,\n  3, # c\n]\n`
    ),
    {
      title: 'x',
      a: { b: { c: 1, d: { e: true } } },
      r: [{ n: 1 }, { n: 2 }],
      t: { i: { x: 1, y: 'z' }, arr: [1, 2, 3] },
    }
  );
  assert.deepEqual(
    ok('toml', `s = "tab\\there \\u00e9"\nl = 'C:\\\\no\\\\escape'\nm = """\nline1\nline2"""\nml = '''\nraw\\n'''\n`),
    { s: 'tab\there é', l: 'C:\\\\no\\\\escape', m: 'line1\nline2', ml: 'raw\\n' }
  );
  assert.deepEqual(
    ok(
      'toml',
      `i = 1_000\nh = 0xff\no = 0o17\nb = 0b101\nf = 3.5e2\nn = -0.5\ninf = inf\nd = 1979-05-27\ndt = 1979-05-27T07:32:00Z\n`
    ),
    { i: 1000, h: 255, o: 15, b: 5, f: 350, n: -0.5, inf: Infinity, d: '1979-05-27', dt: '1979-05-27T07:32:00Z' }
  );
  const r = parseToml('[x]\nk = "v"  # note\n');
  const k = entryOf(entryOf(r.root, 'x').node, 'k').node;
  assert.equal(r.root.entries[0].node.line, 1);
  assert.equal(k.line, 2);
  assert.equal(k.raw, '"v"');
  assert.equal(k.style, 'basic');
  ok('toml', '[a.b.c]\nx = 1\n[a]\ny = 2\n'); // an implicit table may be defined later
});

test('TOML: errors carry the line and say what to do', () => {
  assert.match(err('toml', 'a = 1\na = 2\n').message, /duplicate key 'a' \(already defined on line 1\)/);
  assert.equal(err('toml', 'a = 1\na = 2\n').line, 2);
  assert.match(err('toml', '[t]\nx = 1\n[t]\n').message, /defined twice/);
  assert.match(err('toml', 'name = hello\n').message, /strings need quotes: "hello"/);
  assert.match(err('toml', 's = "open\n').message, /never closed on its line/);
  assert.match(err('toml', 'n = 012\n').message, /leading zeros/);
  assert.match(err('toml', 'k 1\n').message, /expected = after the key 'k'/);
  assert.match(err('toml', 'a = 1 b = 2\n').message, /one key = value per line/);
  assert.match(err('toml', 'a.b = 1\n[a]\n').message, /defined twice/);
  assert.match(err('toml', 'x = { a = 1,\n b = 2 }\n').message, /close with } on the same line/);
  assert.equal(err('toml', '\n\n[x\n').line, 3);
});

test('YAML: block mappings and lists, list-of-mappings, flow collections, quoting', () => {
  assert.deepEqual(ok('yaml', 'a: 1\nb:\n  c: x\n  d: [1, two, "3"]\nlist:\n- p\n- q: 1\n  r: 2\nnested:\n  - 1\n'), {
    a: 1,
    b: { c: 'x', d: [1, 'two', '3'] },
    list: ['p', { q: 1, r: 2 }],
    nested: [1],
  });
  assert.deepEqual(ok('yaml', `s: 'it''s'\nd: "a\\tb \\u00e9"\ne:\nf: {k: v, n: 1}\nurl: http://x:1/y # c\n`), {
    s: "it's",
    d: 'a\tb é',
    e: null,
    f: { k: 'v', n: 1 },
    url: 'http://x:1/y',
  });
  assert.deepEqual(ok('yaml', '---\nk: v\n'), { k: 'v' });
  assert.deepEqual(ok('yaml', '# only a comment\n'), {});
});

test('YAML: block scalars keep their lines; chomping as PyYAML', () => {
  assert.deepEqual(ok('yaml', 'a: |\n  one\n  two\nb: |-\n  x\nc: >\n  folded\n  text\n\n  para\nd: x\n'), {
    a: 'one\ntwo\n',
    b: 'x',
    c: 'folded text\npara\n',
    d: 'x',
  });
  const n = entryOf(parseYaml('k: |\n    deep\n    text\nz: 1\n').root, 'k').node;
  assert.equal(n.style, 'block');
  assert.equal(n.block.indent, 4);
  assert.deepEqual(n.block.lines, ['deep', 'text']);
});

test('YAML: plain scalars are typed the way PyYAML (YAML 1.1) types them', () => {
  const T = (s) => yamlPlainType(s);
  assert.deepEqual(
    [T('yes'), T('Off'), T('true')].map((x) => x.value),
    [true, false, true]
  );
  assert.equal(T('~').type, 'null');
  assert.equal(T('').type, 'null');
  assert.equal(T('null').type, 'null');
  assert.deepEqual(T('0755'), { type: 'int', value: 493 }); // octal in YAML 1.1
  assert.deepEqual(T('0x1f'), { type: 'int', value: 31 });
  assert.deepEqual(T('1_000'), { type: 'int', value: 1000 });
  assert.deepEqual(T('1:30'), { type: 'int', value: 90 }); // sexagesimal
  assert.equal(T('1e3').type, 'string'); // no dot: text to PyYAML
  assert.deepEqual(T('1.0e+3'), { type: 'float', value: 1000 });
  assert.equal(T('.inf').value, Infinity);
  assert.equal(T('2026-11-04').type, 'date');
  assert.equal(T('v1.2').type, 'string');
});

test('YAML: warnings for duplicate keys (the last one wins) and for keys PyYAML reads as booleans', () => {
  const r = parseYaml('a: 1\nb: 2\na: 3\non: [push]\n');
  assert.equal(entryOf(r.root, 'a').node.value, 3);
  assert.match(r.warnings[0].message, /duplicate key 'a': the later value wins and line 1 is ignored/);
  assert.match(r.warnings[1].message, /'on' is read as the boolean True/);
});

test('YAML: syntax errors PyYAML raises, with the line', () => {
  assert.match(err('yaml', 'a:\n\tb: 1\n').message, /tab is used for indentation/);
  assert.match(err('yaml', 'a: b\tc\n').message, /tab character here/); // PyYAML refuses this too
  assert.match(err('yaml', 'a: 1\n  b: 2\n').message, /indented more than the key above it/);
  assert.match(err('yaml', 'a: b: c\n').message, /quote the value if it contains ': '/);
  assert.match(err('yaml', 'a: "open\n').message, /never closed/);
  assert.match(err('yaml', 'a: 1\n- x\n').message, /list item/);
  assert.match(err('yaml', 'a: @x\n').message, /cannot start with '@'/);
  assert.match(err('yaml', ',k: v\n').message, /key cannot start with ','/);
  assert.match(err('yaml', 'a:\n    b: 1\n  c: 2\n').message, /does not line up/);
  assert.equal(err('yaml', 'ok: 1\nfine: 2\nbad: [1, 2\n').line, 3);
  assert.match(err('yaml', 'bad: [1, 2\n').message, /never closed/);
  // tabs are fine inside quotes, | blocks and comments
  ok('yaml', 'a: "x\ty"\nb: |\n  x\ty\nc: d # c\te\n');
});

test('YAML: valid files it does not model are "unsupported", never an error', () => {
  assert.match(unsup('base: &b\n  x: 1\nother:\n  <<: *b\n'), /anchors/);
  assert.match(unsup('a: 1\n---\nb: 2\n'), /several YAML documents/);
  // Agent Hub models a [ ] list over several lines (policy/floor.yaml has them); a value it cannot follow stays unsupported
  assert.match(unsup('a: [1,\n  plain\n  continued]\n'), /several-line/);
  assert.match(unsup('a: "one\n  two"\n'), /spans several lines/);
  assert.match(unsup('a: plain\n  continued\n'), /continues on the next line/);
  assert.match(unsup('? complex\n: key\n'), /complex keys/);
  assert.match(unsup('t: !!str 1\n'), /tags/);
});

test('other formats are not parsed', () => {
  assert.deepEqual(parseConfig('text', 'anything'), { root: null, errors: [], warnings: [], unsupported: null });
});

test('0.9.0 audit: absurd nesting is "unsupported", never a crash; real depth is fine', () => {
  for (const [fmt, text] of [
    ['yaml', `a: ${'['.repeat(20000)}`],
    ['toml', `a = ${'['.repeat(20000)}${']'.repeat(20000)}`],
    ['yaml', `a: ${'['.repeat(100)}${']'.repeat(100)}`],
  ]) {
    const r = parseConfig(fmt, text);
    assert.equal(r.root, null);
    assert.match(r.unsupported || r.errors[0]?.message || '', /nesting deeper|never closed/);
  }
  ok('yaml', 'a:\n  b:\n    c:\n      - d: [1, [2, {e: 3}]]\n');
});

test('YAML (Agent Hub): a [ ] or { } list over several lines, with comments, parses with exact spans', () => {
  const text = 'a: [one,  # first\n  "two",\n  three]\nb: {x: 1,\n  y: 2}\nc: z\n';
  const r = parseConfig('yaml', text);
  assert.equal(r.unsupported, null);
  assert.deepEqual(r.errors, []);
  const a = entryOf(r.root, 'a').node;
  assert.deepEqual(
    a.items.map((x) => x.value),
    ['one', 'two', 'three']
  );
  for (const x of a.items) assert.equal(text.slice(x.start, x.end), x.raw);
  assert.deepEqual(
    entryOf(r.root, 'b').node.entries.map((e) => [e.key, e.node.value]),
    [
      ['x', 1],
      ['y', 2],
    ]
  );
  assert.equal(entryOf(r.root, 'c').node.value, 'z');
  assert.match(JSON.stringify(parseConfig('yaml', 'a: [1,\n  2\n').errors), /never closed/);
  assert.match(
    JSON.stringify(parseConfig('yaml', 'a: [1,\n  2] x\n').errors),
    /unexpected text after the closing bracket/
  );
});
