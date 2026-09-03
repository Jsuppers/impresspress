/**
 * Check that `compiler/dist/` is servable and is what `PIN.json` says it is.
 *
 * `examples/dev-sandbox/build.sh --check` runs this, so a tree that would be
 * rejected at deploy time — or, worse, half-served — fails in CI instead. The
 * things that can go wrong here, all but the last of which have actually gone
 * wrong once:
 *
 *   * a file over the 24 MiB (25 165 824 byte) cap this package holds itself
 *     to — deliberately under Cloudflare's 25 MiB static-asset limit, and why
 *     the component is split at all,
 *   * a `manifest.json` that disagrees with the bytes next to it, because a
 *     file was regenerated without rewriting the manifest,
 *   * the composed `vfs.core-*.wasm` left beside its `.br.part-NNN` files,
 *     which would ship a quarter of a gigabyte nobody fetches,
 *   * a `dist/` built from a different pin than the one in the tree,
 *   * ANYTHING under `dist/` outside the one version directory the manifest
 *     names — a leftover version from before a pin bump is overlaid and
 *     served exactly like the current one, and nothing else here would look
 *     at it, and
 *   * a component composed by `--fast`, which skips `wasm-opt` and is for
 *     local iteration only — unless `IMPRESSPRESS_COMPILER_ALLOW_FAST=1` is
 *     set, which is how a CI job that only wants to know whether the compiler
 *     still WORKS can use a cheap build. The deploy path never sets it, and
 *   * a `manifest.json` with no version directory beside it — an interrupted
 *     build or a partial extraction — which is reported like everything else
 *     here rather than crashing the walk that would otherwise hit it first.
 *
 * Usage: node scripts/verify-compiler-assets.mjs
 */

import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const MAX_ASSET_BYTES = 25165824;

const here = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const dist = path.join(here, "dist");
const problems = [];

/**
 * Print everything found so far and exit non-zero.
 *
 * Called from the end of the run, and from the one place that cannot go on:
 * a `dist/` whose version directory is missing, where the walk below would
 * throw an ENOENT traceback over the top of the list this script exists to
 * print.
 */
const reportAndExit = () => {
  for (const problem of problems) console.error(`  ${problem}`);
  console.error(`verify-compiler-assets.mjs: ${problems.length} problem(s) in ${dist}`);
  process.exit(1);
};

const pin = JSON.parse(fs.readFileSync(path.join(here, "PIN.json"), "utf8"));
const manifestPath = path.join(dist, "manifest.json");
if (!fs.existsSync(manifestPath)) {
  console.error(`verify-compiler-assets.mjs: ${manifestPath} is missing — run build-compiler.sh`);
  process.exit(1);
}
const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));

if (manifest.version !== pin.version) {
  problems.push(`manifest.json is version ${manifest.version}, PIN.json says ${pin.version}`);
}
if (manifest.rubrc?.sha !== pin.rubrc.sha) {
  problems.push(`manifest.json was built from rubrc ${manifest.rubrc?.sha}, PIN.json pins ${pin.rubrc.sha}`);
}
if (manifest.entry !== `/__impresspress_dev/compiler/${pin.version}/worker.js`) {
  problems.push(`manifest.json's entry is ${manifest.entry}, which is not this version's worker`);
}
const allowFast = process.env.IMPRESSPRESS_COMPILER_ALLOW_FAST === "1";
if (manifest.build === "fast" && !allowFast) {
  problems.push(
    "this component was composed by `build-compiler.sh --fast`, which skips wasm-opt — " +
      "rebuild without --fast before deploying, or set IMPRESSPRESS_COMPILER_ALLOW_FAST=1 " +
      "if this is the correctness-testing CI job",
  );
}
if (manifest.build === "fast" && allowFast) {
  console.warn(
    "verify-compiler-assets.mjs: accepting a --fast component because " +
      "IMPRESSPRESS_COMPILER_ALLOW_FAST=1 — this tree must not be deployed",
  );
}

// dist/ holds exactly one version plus the manifest that names it. Everything
// in this directory is overlaid onto the bundle, so a second version left
// behind by a pin bump would be deployed without ever being checked.
for (const entry of fs.readdirSync(dist, { withFileTypes: true })) {
  if (entry.name === "manifest.json" && entry.isFile()) continue;
  if (entry.name === manifest.version && entry.isDirectory()) continue;
  problems.push(
    `dist/${entry.name}: dist holds one version (${manifest.version}) plus manifest.json, ` +
      "and everything here is deployed — remove it or rebuild",
  );
}

// The loop above catches a version directory that should not be there; this
// catches the one that should. `dist/` reaches this state from an interrupted
// `build-compiler.sh` or a partial extraction in `fetch-dist.sh` — the
// manifest lands and the tree it names does not — and the walk below would
// die with an ENOENT traceback instead of saying so.
const versionDir = path.join(dist, manifest.version);
if (!fs.existsSync(versionDir)) {
  problems.push(
    `dist/${manifest.version}: manifest.json names this version but the directory is not ` +
      "there — rebuild with build-compiler.sh, or re-fetch with fetch-dist.sh",
  );
  reportAndExit();
}

const walk = (dir) =>
  fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const full = path.join(dir, entry.name);
    return entry.isDirectory() ? walk(full) : [full];
  });

const onDisk = new Map(
  walk(versionDir).map((file) => [
    path.relative(dist, file).split(path.sep).join("/"),
    file,
  ]),
);

for (const asset of manifest.assets) {
  const file = onDisk.get(asset.path);
  if (!file) {
    problems.push(`${asset.path}: listed in manifest.json but not in dist/`);
    continue;
  }
  onDisk.delete(asset.path);
  const bytes = fs.readFileSync(file);
  if (bytes.length !== asset.bytes) {
    problems.push(`${asset.path}: is ${bytes.length} bytes, manifest.json says ${asset.bytes}`);
  }
  const sha256 = createHash("sha256").update(bytes).digest("hex");
  if (sha256 !== asset.sha256) {
    problems.push(`${asset.path}: is ${sha256}, manifest.json says ${asset.sha256}`);
  }
  if (bytes.length > MAX_ASSET_BYTES) {
    problems.push(`${asset.path}: ${bytes.length} bytes is over the ${MAX_ASSET_BYTES} byte asset limit`);
  }
  if (/^vfs\.core-.*\.wasm$/.test(path.basename(asset.path))) {
    problems.push(`${asset.path}: the composed wasm must be split, not shipped whole`);
  }
}

for (const orphan of onDisk.keys()) {
  problems.push(`${orphan}: in dist/ but not listed in manifest.json`);
}

if (problems.length > 0) {
  reportAndExit();
}

console.log(
  `dist/${manifest.version}: ${manifest.assets.length} files, ` +
    `${(manifest.total_bytes / 1024 / 1024).toFixed(1)} MiB, largest ` +
    `${(Math.max(...manifest.assets.map((a) => a.bytes)) / 1024 / 1024).toFixed(1)} MiB`,
);
