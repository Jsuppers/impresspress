// Run with: node --test crates/impresspress-core/src/blocks/dev/assets/test/dev_compile_block.test.mjs
//
// `dev_compile_block` — the half of it that needs no browser.
//
// The compile itself needs a `Worker`, a `SharedArrayBuffer` and 72 MiB of
// toolchain, and it is driven end to end by `dev-compile-tool.spec.ts`. What
// is here is everything AROUND that: reading a block out of the workspace,
// the digest that ties a stored build back to the exact sources it was made
// from, and the split between a failure that is an ANSWER about the block
// (`success: false`, no `isError`) and one that is a failure of the machinery
// (`isError`, and nothing to report about the block). Every one of those is a
// decision the page makes on its own, and none of them is worth a forty-second
// browser test.
//
// See `harness.mjs` for how the tail is loaded without adding a test hook to
// the shipped file.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { instantiate } from './harness.mjs';

/** The real manifest's shape, trimmed to the fields the page reads. */
const MANIFEST = {
  schema_version: 1,
  version: '807ace9e',
  entry: '/__impresspress_dev/compiler/807ace9e/worker.js',
  total_bytes: 75124831,
  target: 'wasm32-wasip1'
};

/** One macrotask, which is long enough for the tail's load-time work. */
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

/** The `sha256` a workspace entry reports, from its own content. */
const digest = (content) => createHash('sha256').update(content).digest('hex');

/** A `blocks/hello/` scaffold, as `dev_create_block` leaves it. */
const file = (path, content, extra = {}) => ({
  path,
  content,
  sha256: digest(content),
  ...extra
});

const CARGO_TOML = '[package]\nname = "hello"\n';
const LIB_RS = 'pub fn block() -> Block { Block::new("site/hello", "Says hello") }\n';
const WAFER_GUEST = 'pub const WAFER_GUEST_VERSION: u32 = 1;\n';

const HELLO = [
  file('blocks/hello/Cargo.toml', CARGO_TOML),
  file('blocks/hello/src/lib.rs', LIB_RS),
  file('blocks/hello/src/wafer_guest.rs', WAFER_GUEST)
];

test('snapshotBlock reads the block crate-relative, with the guest version out of the block itself', async () => {
  const { handle } = instantiate({ workspace: HELLO });
  const snapshot = await handle.snapshotBlock('hello');

  // The worker's VFS is keyed on paths relative to the crate root: it writes
  // `Cargo.toml`, not `blocks/hello/Cargo.toml`. A snapshot that kept the
  // workspace prefix would produce a crate cargo cannot see.
  assert.deepEqual(Object.keys(snapshot.files).sort(), [
    'Cargo.toml',
    'src/lib.rs',
    'src/wafer_guest.rs'
  ]);
  assert.equal(snapshot.files['src/lib.rs'], LIB_RS);
  assert.deepEqual(snapshot.diagnostics, []);
  // Read out of the BLOCK's copy of the vendored module — which is what
  // `POST /b/dev/api/builds/stage` checks against the sandbox's own — and not
  // assumed from anywhere on the page.
  assert.equal(snapshot.guestVersion, 1);
});

test('snapshotBlock reports the guest version the block actually carries, not the current one', async () => {
  const { handle } = instantiate({
    workspace: [
      HELLO[0],
      HELLO[1],
      file('blocks/hello/src/wafer_guest.rs', 'pub const WAFER_GUEST_VERSION: u32 = 7;\n')
    ]
  });
  // A stale copy is exactly what the version check exists to catch, so the
  // page has to report it faithfully — a snapshot that answered `1` here
  // would stage a block built against an ABI this runtime no longer speaks.
  assert.equal((await handle.snapshotBlock('hello')).guestVersion, 7);
});

test('snapshotBlock reports a module it cannot find a version in as unknown', async () => {
  const { handle } = instantiate({
    workspace: [HELLO[0], HELLO[1], file('blocks/hello/src/wafer_guest.rs', '// edited away\n')]
  });
  // `null` becomes an omitted `wafer_guest_version`, which staging records as
  // `0` — "the compiler could not read one" — rather than a guess that would
  // pass a check it never actually made.
  assert.equal((await handle.snapshotBlock('hello')).guestVersion, null);
});

test('snapshotBlock refuses a binary file under blocks/ with a binary-source diagnostic', async () => {
  const { handle } = instantiate({
    workspace: [
      ...HELLO,
      file('blocks/hello/src/logo.png', 'iVBORw0KGgo=', { encoding: 'base64' })
    ]
  });
  const snapshot = await handle.snapshotBlock('hello');

  // Not skipped: a crate that compiled without a file its source references
  // would produce a block whose sources are not the sources on disk.
  assert.equal(snapshot.diagnostics.length, 1);
  assert.equal(snapshot.diagnostics[0].severity, 'error');
  assert.equal(snapshot.diagnostics[0].code, 'binary-source');
  // Crate-relative, like everything else the compiler is told about.
  assert.equal(snapshot.diagnostics[0].file, 'src/logo.png');
  assert.match(snapshot.diagnostics[0].message, /base64/);
  assert.ok(!('src/logo.png' in snapshot.files));
});

test('the source manifest digest is over sorted `path\\0sha256\\n` lines, crate-relative', async () => {
  const { handle } = instantiate({ workspace: HELLO });
  const snapshot = await handle.snapshotBlock('hello');

  // Spelled out here rather than recomputed by the same code under test: the
  // digest is the only link between a stored build and the exact bytes it was
  // made from, so this is the definition of it. NUL separates because a path
  // may contain anything else, and the paths are the ones the COMPILER saw.
  const expected = digest(
    [
      `Cargo.toml\0${digest(CARGO_TOML)}\n`,
      `src/lib.rs\0${digest(LIB_RS)}\n`,
      `src/wafer_guest.rs\0${digest(WAFER_GUEST)}\n`
    ].join('')
  );
  assert.equal(snapshot.sourceSha, expected);
});

test('the source manifest digest changes when a source byte does', async () => {
  const { handle: before } = instantiate({ workspace: HELLO });
  const { handle: after } = instantiate({
    workspace: [HELLO[0], file('blocks/hello/src/lib.rs', `${LIB_RS}// a comment\n`), HELLO[2]]
  });
  assert.notEqual(
    (await before.snapshotBlock('hello')).sourceSha,
    (await after.snapshotBlock('hello')).sourceSha
  );
});

test('snapshotBlock refuses a name with nothing under it rather than compiling an empty crate', async () => {
  const { handle } = instantiate({ workspace: HELLO });
  await assert.rejects(() => handle.snapshotBlock('nope'), /no block at blocks\/nope\//);
});

test('the Compile select is populated from the blocks/ prefixes in the file listing', async () => {
  const { elements } = instantiate({
    workspace: [
      ...HELLO,
      file('blocks/newsletter/Cargo.toml', '[package]\nname = "newsletter"\n'),
      file('site/index.html', '<!doctype html>\n')
    ]
  });
  await settle();

  // One option per block, and nothing for `site/`: a block IS a
  // `blocks/<name>/` prefix with files under it. In path order, because the
  // listing is.
  assert.deepEqual(
    elements.get('dev-compile-block').children.map((option) => option.value),
    ['hello', 'newsletter']
  );
});

test('with no compiler in the build, dev_compile_block is an isError and not a failed build', async () => {
  const { tools } = instantiate({
    hasModelContext: true,
    compilerManifest: null,
    workspace: HELLO
  });
  await settle();

  const result = await tools.get('dev_compile_block').execute({ name: 'hello' });
  // "This build has no toolchain" is not a verdict on the block: there is no
  // build to report diagnostics about, and an agent told `success: false`
  // would go looking for a mistake in its Rust.
  assert.equal(result.isError, true);
  assert.match(result.content[0].text, /No compiler in this build\./);
  assert.equal(result.structuredContent, undefined);
});

/**
 * A `BrowserRustCompiler` that answers without a worker.
 *
 * The real one is covered by `dev-compiler.spec.ts` against the protocol, and
 * by `dev-compile-tool.spec.ts` against real rustc. What the two tests below
 * need is only its CONTRACT: `compile` resolves with a `CompileResult` for a
 * crate that does not build, and rejects when the adapter itself cannot go on.
 */
function fakeCompiler(behaviour) {
  return class {
    constructor(manifest) {
      this.manifest = manifest;
    }

    async initialize() {
      return 'rustc 1.90.0-nightly (fake)';
    }

    async compile() {
      return behaviour();
    }
  };
}

const BUILT = {
  buildId: 'build-1',
  success: true,
  cancelled: false,
  artifact: Uint8Array.from([0, 1, 2, 3]),
  artifactSha256: 'unused',
  stdout: 'Finished `release` profile',
  stderr: '',
  diagnostics: [],
  elapsedMs: 38000,
  compilerVersion: 'rustc 1.90.0-nightly (fake)'
};

test('a crate that does not compile is a result, and the diagnostics keep rustc shape', async () => {
  const { tools } = instantiate({
    hasModelContext: true,
    compilerManifest: MANIFEST,
    workspace: HELLO,
    compiler: fakeCompiler(() => ({
      ...BUILT,
      success: false,
      artifact: null,
      stderr: 'error: expected `;`, found `value`\n',
      // The second one is what a rustc diagnostic without a number looks
      // like: `code` is optional in `compiler/src/protocol.ts` and required
      // in `validation::Diagnostic`.
      diagnostics: [
        { file: 'src/lib.rs', line: 12, column: 4, severity: 'error', message: 'expected `;`', code: 'E0425' },
        { file: 'src/lib.rs', line: 3, column: 1, severity: 'warning', message: 'unused import' }
      ]
    }))
  });
  await settle();

  const result = await tools.get('dev_compile_block').execute({ name: 'hello' });
  assert.equal(result.isError, undefined);
  assert.equal(result.structuredContent.success, false);
  // Nothing was staged, so there is no build row and no generation.
  assert.equal(result.structuredContent.build_id, null);
  assert.equal(result.structuredContent.generation, null);
  assert.equal(result.structuredContent.elapsed_ms, 38000);
  assert.deepEqual(result.structuredContent.diagnostics[0], {
    severity: 'error',
    code: 'E0425',
    message: 'expected `;`',
    file: 'src/lib.rs',
    line: 12,
    column: 4
  });
  // An unnumbered diagnostic still has to satisfy `validation::Diagnostic`'s
  // required `code`, or a single warning on an otherwise good build would
  // make the staging call a 400.
  assert.equal(result.structuredContent.diagnostics[1].code, 'rustc');
});

test('a successful compile stages the artifact and merges what staging answered', async () => {
  const staged = [];
  // The sandbox as it stands, which the compile below changes: staging
  // activates a generation, so the status the page reads afterwards is the
  // one that generation produced. Modelling that is the whole point here —
  // the ladder assertion at the end is about what the page shows once the
  // activation is OVER and the journal has gone quiet again.
  let live = null;
  const { tools, elements } = instantiate({
    hasModelContext: true,
    compilerManifest: MANIFEST,
    workspace: HELLO,
    compiler: fakeCompiler(() => BUILT),
    status: () => ({
      active_generation: live,
      runtime_generation: live ? 1 : 0,
      blocks: [],
      activation: null,
      wafer_guest_version: 1
    }),
    stage(request) {
      staged.push(request);
      live = { id: 'gen_2', cause: 'block_compile', status: 'active' };
      return {
        build_id: 'bld_1',
        success: true,
        diagnostics: [],
        generation: { id: 'gen_2', cause: 'block_compile', status: 'active' },
        progress: [
          { phase: 'validating', ms: 4, detail: '' },
          { phase: 'building_runtime', ms: 91, detail: '' },
          { phase: 'publishing', ms: 2, detail: '' },
          { phase: 'active', ms: 0, detail: '' }
        ]
      };
    }
  });
  await settle();

  const result = await tools.get('dev_compile_block').execute({ name: 'hello' });
  assert.equal(result.isError, undefined);
  assert.equal(result.structuredContent.success, true);
  assert.equal(result.structuredContent.build_id, 'bld_1');
  assert.equal(result.structuredContent.generation.cause, 'block_compile');
  assert.equal(result.structuredContent.progress.length, 4);

  // The request the page built: base64 of the artifact's own bytes, the
  // block's short name (not `site/hello`), and the version read out of the
  // block's own guest module.
  assert.equal(staged.length, 1);
  assert.equal(staged[0].block_name, 'hello');
  assert.equal(staged[0].artifact_base64, Buffer.from(BUILT.artifact).toString('base64'));
  assert.equal(staged[0].wafer_guest_version, 1);
  assert.equal(staged[0].compiler_version, 'rustc 1.90.0-nightly (fake)');
  assert.match(staged[0].source_manifest_sha256, /^[0-9a-f]{64}$/);

  // The ladder shows the activation that just finished. The journal rests at
  // `idle` the moment the swap is recorded, so the staging response is the
  // only account of those phases that survives the catch-up poll — a page
  // that drew the ladder from the status alone would blank it here.
  assert.equal(elements.get('dev-progress-steps').getAttribute('data-phase'), 'active');
  assert.deepEqual(
    elements.get('dev-progress-steps').children.map((step) => step.getAttribute('data-state')),
    ['done', 'done', 'done', 'done']
  );
});
