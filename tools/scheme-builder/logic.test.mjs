/* ============================================================
 * Unit tests for the pure JS logic in index.html.
 *
 * The tool is a single HTML file with all its JS in one <script>
 * block — no module system, no test framework, and the surrounding
 * repo is Rust. Rather than refactor the tool, we slice the relevant
 * pure functions out of the HTML by name and evaluate them with
 * `new Function(...)`, injecting the module-level constants they
 * close over (SINGLES, HUES, the GLOW_* tables). DOM-touching code
 * and the global `state` are never pulled in; functions that need a
 * `state` (toToml/toGlowLines) get one passed via the sandbox.
 *
 * Run from the repo root:   node tools/scheme-builder/logic.test.mjs
 * Exits non-zero on the first failing assertion.
 * ============================================================ */
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const __dirname = dirname(fileURLToPath(import.meta.url));
const html = readFileSync(join(__dirname, 'index.html'), 'utf8');

/* grab(name): the source of a top-level `function name(...) { ... }`,
 * from its declaration up to the next top-level `\nfunction ` (every
 * such function in the file starts in column 0). */
function grab(name) {
  const start = html.indexOf('function ' + name + '(');
  if (start < 0) throw new Error('function not found: ' + name);
  // Some helpers are one-liners (`function clone(o) { ... }`); return just
  // that line. Otherwise end at the function's own closing brace in column 0
  // (`\n}`) — not the next `\nfunction `, since top-level statements (e.g.
  // `let state = ...`) can sit between two functions and get swept in.
  const firstNl = html.indexOf('\n', start);
  const firstLine = html.slice(start, firstNl);
  if (/\}\s*$/.test(firstLine)) return firstLine;
  const end = html.indexOf('\n}', start);
  if (end < 0) throw new Error('no closing brace for: ' + name);
  return html.slice(start, end + 2);
}

// Module-level constants the functions close over, lifted verbatim.
const GLOW_DEFAULTS_SRC = html.match(/const GLOW_DEFAULTS = \{[\s\S]*?\n\};/)[0];
const GLOW_COLOR_KEYS_SRC = html.match(/const GLOW_COLOR_KEYS = \[[\s\S]*?\];/)[0];
const GLOW_BOOL_KEYS_SRC = html.match(/const GLOW_BOOL_KEYS = \[[\s\S]*?\];/)[0];
const MIN_PALETTE_SAT_SRC = html.match(/const MIN_PALETTE_SAT = [\d.]+;/)[0];

/* Build a sandbox object exposing the named functions plus a mutable
 * `state` slot. We assemble one source string: the grabbed constants,
 * then the grabbed functions, then a return of the public names. The
 * sandboxed `state` is read through a closure variable so tests can
 * swap it between calls (toToml reads the live `state`). */
function buildSandbox(fnNames, extraConstSrc = '') {
  const consts = [
    MIN_PALETTE_SAT_SRC,
    GLOW_DEFAULTS_SRC,
    GLOW_COLOR_KEYS_SRC,
    GLOW_BOOL_KEYS_SRC,
    extraConstSrc,
  ].join('\n');
  const fns = fnNames.map(grab).join('\n');
  const body = `
    let state = __getState();
    ${consts}
    ${fns}
    return { ${fnNames.join(', ')}, __setState: s => { state = s; } };
  `;
  let _state = null;
  const api = new Function('SINGLES', 'HUES', '__getState', body)(
    ['background', 'foreground', 'cursor', 'selection'],
    ['black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white'],
    () => _state,
  );
  return api;
}

// Pull GLOW_DEFAULTS into JS so tests can build a valid glow block.
const GLOW_DEFAULTS = new Function(GLOW_DEFAULTS_SRC + '\nreturn GLOW_DEFAULTS;')();

/* ---- tiny test runner: collect, run, report, exit non-zero on fail ---- */
let passed = 0, failed = 0;
const failures = [];
function test(name, fn) {
  try { fn(); passed++; }
  catch (e) { failed++; failures.push({ name, e }); }
}
function done() {
  for (const { name, e } of failures) {
    console.error(`  FAIL  ${name}`);
    console.error('        ' + (e && e.message ? e.message.split('\n')[0] : e));
  }
  console.log(`\n${passed} passed, ${failed} failed`);
  if (failed) process.exit(1);
}

/* ============================================================
 * normHex / toTomlHex
 * ============================================================ */
{
  const { normHex } = buildSandbox(['normHex']);
  const { toTomlHex } = buildSandbox(['toTomlHex']);

  test('normHex expands #rgb shorthand', () => {
    assert.equal(normHex('#abc'), '#aabbcc');
  });
  test('normHex accepts 0x prefix', () => {
    assert.equal(normHex('0xFF8800'), '#ff8800');
  });
  test('normHex accepts bare rrggbb', () => {
    assert.equal(normHex('11AA22'), '#11aa22');
  });
  test('normHex accepts #rrggbb and lowercases', () => {
    assert.equal(normHex('#DDEEFF'), '#ddeeff');
  });
  test('normHex trims surrounding whitespace', () => {
    assert.equal(normHex('  #abcdef  '), '#abcdef');
  });
  test('normHex rejects bad length', () => {
    assert.equal(normHex('#abcd'), null);
  });
  test('normHex rejects non-hex chars', () => {
    assert.equal(normHex('#gggggg'), null);
  });
  test('normHex rejects empty / null', () => {
    assert.equal(normHex(''), null);
    assert.equal(normHex(null), null);
  });

  test('toTomlHex emits 0xrrggbb', () => {
    assert.equal(toTomlHex('#ff8800'), '0xff8800');
  });
}

/* ============================================================
 * parseToml
 * ============================================================ */
{
  // parseToml depends on parseGlowKey -> normHex/clampGlowNum.
  const { parseToml } = buildSandbox(
    ['parseToml', 'parseGlowKey', 'clampGlowNum', 'normHex'],
  );

  test('parseToml reads window single colors (0x and bare forms)', () => {
    const { out, errors } = parseToml(
      'background = 0x002b36\nforeground = 0x839496\ncursor = 93a1a1\nselection = 073642',
    );
    assert.deepEqual(errors, []);
    assert.equal(out.background, '#002b36');
    assert.equal(out.foreground, '#839496');
    assert.equal(out.cursor, '#93a1a1');
    assert.equal(out.selection, '#073642');
  });

  test('parseToml treats # as a line comment (so #rrggbb values are unusable)', () => {
    // The whole `#93a1a1` is stripped as a comment, leaving an empty value.
    const { out, errors } = parseToml('cursor = #93a1a1');
    assert.equal(out.cursor, null);
    assert.equal(errors.length, 1);
    assert.match(errors[0], /invalid hex/);
  });

  test('parseToml reads an ANSI [normal, bright] pair', () => {
    const { out, errors } = parseToml('red = [0xab0000, 0xff5555]');
    assert.deepEqual(errors, []);
    assert.deepEqual(out.ansi.red, ['#ab0000', '#ff5555']);
  });

  test('parseToml allows a trailing comma in pairs', () => {
    const { out, errors } = parseToml('green = [0x00ab00, 0x55ff55,]');
    assert.deepEqual(errors, []);
    assert.deepEqual(out.ansi.green, ['#00ab00', '#55ff55']);
  });

  test('parseToml rejects malformed pairs without throwing', () => {
    const { out, errors } = parseToml('blue = [0x0000ab]');
    assert.equal(out.ansi.blue, undefined);
    assert.equal(errors.length, 1);
    assert.match(errors[0], /expected \[normal, bright\]/);
  });

  test('parseToml rejects bad hex inside a pair', () => {
    const { errors } = parseToml('cyan = [0x00abab, nothex]');
    assert.equal(errors.length, 1);
    assert.match(errors[0], /invalid hex in pair/);
  });

  test('parseToml max_colors quoted', () => {
    assert.equal(parseToml('max_colors = "256"').out.max_colors, '256');
  });
  test('parseToml max_colors bare', () => {
    assert.equal(parseToml('max_colors = 16').out.max_colors, '16');
  });
  test('parseToml max_colors aliases monochrome/16m/16777216', () => {
    assert.equal(parseToml('max_colors = "monochrome"').out.max_colors, 'mono');
    assert.equal(parseToml('max_colors = 16m').out.max_colors, 'truecolor');
    assert.equal(parseToml('max_colors = "16777216"').out.max_colors, 'truecolor');
  });
  test('parseToml rejects unknown max_colors', () => {
    const { errors } = parseToml('max_colors = 12');
    assert.equal(errors.length, 1);
    assert.match(errors[0], /max_colors must be one of/);
  });

  test('parseToml accepts selection_fg and the _foreground alias', () => {
    assert.equal(parseToml('selection_fg = 0x112233').out.selection_fg, '#112233');
    assert.equal(parseToml('selection_foreground = 0x445566').out.selection_fg, '#445566');
  });

  test('parseToml glow_* keys set glowSeen and parse values', () => {
    const { out, errors } = parseToml(
      'glow_match_brightness = true\nglow_intensity = 1.5\nglow_scanline_color_dark = 0x101010',
    );
    assert.deepEqual(errors, []);
    assert.equal(out.glowSeen, true);
    assert.equal(out.glow.match_brightness, true);
    assert.equal(out.glow.intensity, 1.5);
    assert.equal(out.glow.scanline_color_dark, '#101010');
  });

  test('parseToml skips comments and blank lines', () => {
    const { out, errors } = parseToml('# a comment\n\nbackground = 0x000000 # trailing\n');
    assert.deepEqual(errors, []);
    assert.equal(out.background, '#000000');
  });

  test('parseToml reports unknown keys without throwing', () => {
    const { errors } = parseToml('frobnicate = 0x123456');
    assert.equal(errors.length, 1);
    assert.match(errors[0], /unknown key 'frobnicate'/);
  });

  test('parseToml reports a missing = sign', () => {
    const { errors } = parseToml('background 0x000000');
    assert.equal(errors.length, 1);
    assert.match(errors[0], /missing '='/);
  });

  test('parseToml does not throw on a bad single-color value', () => {
    const { out, errors } = parseToml('background = ziggurat');
    assert.equal(out.background, null);
    assert.equal(errors.length, 1);
    assert.match(errors[0], /invalid hex/);
  });
}

/* ============================================================
 * toToml + round-trip
 * ============================================================ */
{
  const sb = buildSandbox(
    ['toToml', 'toGlowLines', 'toTomlHex', 'tomlNum', 'normalizeState', 'clone'],
  );
  const parser = buildSandbox(
    ['parseToml', 'parseGlowKey', 'clampGlowNum', 'normHex'],
  );

  // Build a minimal valid state mirroring DEFAULTS, then normalize it so the
  // glow block + selection_fg slot exist.
  function freshState() {
    return sb.normalizeState({
      background: '#fbfaf7', foreground: '#2d2519',
      cursor: '#1a00cc', selection: '#3366d9',
      ansi: {
        black:   ['#000000', '#808080'],
        red:     ['#ab0000', '#ff5555'],
        green:   ['#00ab00', '#55ff55'],
        yellow:  ['#abab00', '#ffff55'],
        blue:    ['#0000ab', '#5555ff'],
        magenta: ['#ab00ab', '#ff55ff'],
        cyan:    ['#00abab', '#55ffff'],
        white:   ['#ababab', '#ffffff'],
      },
      max_colors: '',
    });
  }

  test('normalizeState fills in the glow block and selection_fg slot', () => {
    const s = freshState();
    assert.equal(s.selection_fg, null);
    assert.deepEqual(s.glow, GLOW_DEFAULTS);
  });

  test('toToml round-trips window + ansi + glow + selection_fg', () => {
    const s = freshState();
    s.selection_fg = '#aabbcc';
    s.glow.enabled = true;
    s.glow.match_brightness = true;
    s.glow.intensity = 1.5;
    s.glow.iterations = 4;
    s.glow.scanline_color_dark = '#101010';
    sb.__setState(s);

    const toml = sb.toToml();
    const { out, errors } = parser.parseToml(toml);
    assert.deepEqual(errors, []);
    assert.equal(out.background, '#fbfaf7');
    assert.equal(out.selection_fg, '#aabbcc');
    assert.deepEqual(out.ansi.magenta, ['#ab00ab', '#ff55ff']);
    assert.equal(out.glow.match_brightness, true);
    assert.equal(out.glow.intensity, 1.5);
    assert.equal(out.glow.iterations, 4);
    assert.equal(out.glow.scanline_color_dark, '#101010');
  });

  test('toToml emits no glow_* lines when glow is disabled', () => {
    const s = freshState();
    s.glow.enabled = false;
    s.glow.match_brightness = true; // would otherwise be emitted
    sb.__setState(s);
    assert.equal(/glow_/.test(sb.toToml()), false);
  });

  test('toToml emits only NON-default glow keys', () => {
    const s = freshState();
    s.glow.enabled = true;
    s.glow.match_brightness = true; // the single non-default
    sb.__setState(s);
    const toml = sb.toToml();
    const glowLines = toml.split('\n').filter(l => l.startsWith('glow_'));
    assert.deepEqual(glowLines, ['glow_match_brightness = true']);
  });

  test('toToml omits the glow comment block when all values are default', () => {
    const s = freshState();
    s.glow.enabled = true; // enabled, but nothing changed from defaults
    sb.__setState(s);
    const toml = sb.toToml();
    assert.equal(/glow_/.test(toml), false);
    assert.equal(/Glow \+ scanline overrides/.test(toml), false);
  });

  test('toToml omits selection_fg + max_colors when unset', () => {
    const s = freshState();
    sb.__setState(s);
    const toml = sb.toToml();
    assert.equal(/selection_fg/.test(toml), false);
    assert.equal(/max_colors/.test(toml), false);
  });
}

/* ============================================================
 * parseGlowKey + clampGlowNum
 * ============================================================ */
{
  const { parseGlowKey, clampGlowNum } = buildSandbox(
    ['parseGlowKey', 'clampGlowNum', 'normHex'],
  );

  test('parseGlowKey parses a bool field', () => {
    const g = {};
    assert.equal(parseGlowKey(g, 'scanlines', 'true'), null);
    assert.equal(g.scanlines, true);
    assert.equal(parseGlowKey(g, 'scanlines', 'false'), null);
    assert.equal(g.scanlines, false);
  });
  test('parseGlowKey rejects a non-bool bool', () => {
    assert.match(parseGlowKey({}, 'scanlines', 'yes'), /expected true\/false/);
  });

  test('parseGlowKey parses a color field', () => {
    const g = {};
    assert.equal(parseGlowKey(g, 'scanline_color_bright', '0xabcdef'), null);
    assert.equal(g.scanline_color_bright, '#abcdef');
  });
  test('parseGlowKey rejects bad color hex', () => {
    assert.match(parseGlowKey({}, 'scanline_color_bright', 'nope'), /invalid hex/);
  });

  test('parseGlowKey parses a numeric field with clamping', () => {
    const g = {};
    assert.equal(parseGlowKey(g, 'threshold', '0.6'), null);
    assert.equal(g.threshold, 0.6);
  });
  test('parseGlowKey rejects a non-numeric number', () => {
    assert.match(parseGlowKey({}, 'threshold', 'abc'), /expected a number/);
  });

  test('parseGlowKey parses iterations as integer', () => {
    const g = {};
    assert.equal(parseGlowKey(g, 'iterations', '3'), null);
    assert.equal(g.iterations, 3);
  });
  test('parseGlowKey iterations clamp to [1,12]', () => {
    const g = {};
    parseGlowKey(g, 'iterations', '99'); assert.equal(g.iterations, 12);
    parseGlowKey(g, 'iterations', '0');  assert.equal(g.iterations, 1);
  });
  test('parseGlowKey rejects a non-integer iterations value', () => {
    assert.match(parseGlowKey({}, 'iterations', '2.5'), /expected an integer/);
  });

  test('parseGlowKey rejects unknown fields and the master enabled key', () => {
    assert.match(parseGlowKey({}, 'bogus', '1'), /unknown key 'glow_bogus'/);
    assert.match(parseGlowKey({}, 'enabled', 'true'), /unknown key 'glow_enabled'/);
  });

  test('clampGlowNum: threshold/softness clamp to [0,1]', () => {
    assert.equal(clampGlowNum('threshold', 2), 1);
    assert.equal(clampGlowNum('threshold', -1), 0);
    assert.equal(clampGlowNum('softness', 0.5), 0.5);
  });
  test('clampGlowNum: intensity clamps to [0,inf)', () => {
    assert.equal(clampGlowNum('intensity', -3), 0);
    assert.equal(clampGlowNum('intensity', 5), 5);
  });
  test('clampGlowNum: hue_tolerance_deg clamps to [0,180]', () => {
    assert.equal(clampGlowNum('hue_tolerance_deg', 999), 180);
    assert.equal(clampGlowNum('hue_tolerance_deg', -5), 0);
  });
  test('clampGlowNum: fg_tolerance clamps to [0,sqrt(3)]', () => {
    assert.equal(clampGlowNum('fg_tolerance', 99), Math.sqrt(3));
    assert.equal(clampGlowNum('fg_tolerance', -1), 0);
  });
  test('clampGlowNum: scanline_period clamps to [1,inf)', () => {
    assert.equal(clampGlowNum('scanline_period', 0), 1);
    assert.equal(clampGlowNum('scanline_period', 8), 8);
  });
}

/* ============================================================
 * relLuminance / contrastRatio / gradeOf
 * ============================================================ */
{
  const { relLuminance, contrastRatio, gradeOf } = buildSandbox(
    ['relLuminance', 'contrastRatio', 'gradeOf'],
  );

  test('relLuminance: black ~0, white ~1', () => {
    assert.ok(Math.abs(relLuminance('#000000')) < 1e-9);
    assert.ok(Math.abs(relLuminance('#ffffff') - 1) < 1e-9);
  });

  test('contrastRatio: black on white ~= 21:1', () => {
    assert.ok(Math.abs(contrastRatio('#000000', '#ffffff') - 21) < 0.01);
  });
  test('contrastRatio: equal colors == 1:1', () => {
    assert.equal(contrastRatio('#336699', '#336699'), 1);
  });
  test('contrastRatio is symmetric', () => {
    assert.equal(
      contrastRatio('#123456', '#abcdef'),
      contrastRatio('#abcdef', '#123456'),
    );
  });

  test('gradeOf: >=7 is AAA/pass', () => {
    assert.deepEqual(gradeOf(21), ['AAA', 'pass']);
    assert.deepEqual(gradeOf(7), ['AAA', 'pass']);
  });
  test('gradeOf: [4.5,7) is AA/pass', () => {
    assert.deepEqual(gradeOf(4.5), ['AA', 'pass']);
    assert.deepEqual(gradeOf(6.99), ['AA', 'pass']);
  });
  test('gradeOf: [3,4.5) is AA·lg/aa', () => {
    assert.deepEqual(gradeOf(3), ['AA·lg', 'aa']);
    assert.deepEqual(gradeOf(4.49), ['AA·lg', 'aa']);
  });
  test('gradeOf: <3 is fail', () => {
    assert.deepEqual(gradeOf(2.99), ['fail', 'fail']);
    assert.deepEqual(gradeOf(1), ['fail', 'fail']);
  });
}

/* ============================================================
 * smoothstep / hsvLin / hueDist
 * ============================================================ */
{
  const { smoothstep, hsvLin, hueDist } = buildSandbox(
    ['smoothstep', 'hsvLin', 'hueDist'],
  );

  test('smoothstep is 0 at/below the low edge', () => {
    assert.equal(smoothstep(0.2, 0.8, 0.1), 0);
    assert.equal(smoothstep(0.2, 0.8, 0.2), 0);
  });
  test('smoothstep is 1 at/above the high edge', () => {
    assert.equal(smoothstep(0.2, 0.8, 0.9), 1);
    assert.equal(smoothstep(0.2, 0.8, 0.8), 1);
  });
  test('smoothstep midpoint is 0.5', () => {
    assert.ok(Math.abs(smoothstep(0, 1, 0.5) - 0.5) < 1e-9);
  });

  test('hsvLin of a grey returns h=-1 (achromatic)', () => {
    const { h } = hsvLin([0.4, 0.4, 0.4]);
    assert.equal(h, -1);
  });
  test('hsvLin of pure red has hue 0, sat 1', () => {
    const { h, s, v } = hsvLin([1, 0, 0]);
    assert.equal(h, 0);
    assert.equal(s, 1);
    assert.equal(v, 1);
  });
  test('hsvLin of pure green has hue 120', () => {
    assert.equal(hsvLin([0, 1, 0]).h, 120);
  });
  test('hsvLin of pure blue has hue 240', () => {
    assert.equal(hsvLin([0, 0, 1]).h, 240);
  });

  test('hueDist wraps around 360', () => {
    assert.equal(hueDist(350, 10), 20);
    assert.equal(hueDist(10, 350), 20);
  });
  test('hueDist of equal hues is 0', () => {
    assert.equal(hueDist(120, 120), 0);
  });
  test('hueDist of opposite hues is 180', () => {
    assert.equal(hueDist(0, 180), 180);
  });
}

done();
