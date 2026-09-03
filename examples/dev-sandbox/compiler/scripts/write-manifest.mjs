/**
 * Write `dist/manifest.json` — what the sandbox reads to find the compiler.
 *
 * The page never guesses a URL: it fetches this manifest, loads `entry`, and
 * that path carries the pinned rubrc sha, so a redeploy with a different pin
 * cannot be served a stale worker out of a cache. Every file is listed with
 * its size and sha256 so `verify-compiler-assets.mjs` can prove the tree that
 * ships is the tree that was built.
 *
 * `COMPILER_BUILD_KIND` is required and must be `full` or `fast`:
 * `build-compiler.sh` reads it off the composed component's own
 * `.build-kind`, and a default here would be the very thing that file exists
 * to prevent — an unlabelled component reported as optimized.
 *
 * Usage: COMPILER_BUILD_KIND=full node scripts/write-manifest.mjs
 */

import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const pin = JSON.parse(fs.readFileSync(path.join(here, "PIN.json"), "utf8"));
const dist = path.join(here, "dist");
const version = pin.version;
const out = path.join(dist, version);

if (!fs.existsSync(out)) {
  console.error(`write-manifest.mjs: ${out} does not exist — run build-compiler.sh`);
  process.exit(1);
}

const buildKind = process.env.COMPILER_BUILD_KIND;
if (buildKind !== "full" && buildKind !== "fast") {
  console.error(
    `write-manifest.mjs: COMPILER_BUILD_KIND must be "full" or "fast", got ` +
      `${JSON.stringify(buildKind)} — build-compiler.sh reads it from the composed ` +
      `component's own .build-kind`,
  );
  process.exit(1);
}

const walk = (dir) =>
  fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const full = path.join(dir, entry.name);
    return entry.isDirectory() ? walk(full) : [full];
  });

const assets = walk(out)
  .map((file) => ({
    path: path.relative(dist, file).split(path.sep).join("/"),
    bytes: fs.statSync(file).size,
    sha256: createHash("sha256").update(fs.readFileSync(file)).digest("hex"),
  }))
  .sort((a, b) => a.path.localeCompare(b.path));

const manifest = {
  schema_version: 1,
  version,
  // `fast` means the component was composed without wasm-opt
  // (`build-compiler.sh --fast`): fine for local iteration, and refused by
  // `verify-compiler-assets.mjs` so it can never be deployed. It is what the
  // component's own `.build-kind` says, not what this run asked for.
  build: buildKind,
  entry: `/__impresspress_dev/compiler/${version}/worker.js`,
  total_bytes: assets.reduce((n, asset) => n + asset.bytes, 0),
  assets,
  license: pin.license,
  rubrc: { repo: pin.rubrc.repo, sha: pin.rubrc.sha },
  target: pin.target,
};

fs.writeFileSync(path.join(dist, "manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
console.log(
  `manifest.json: ${assets.length} files, ${(manifest.total_bytes / 1024 / 1024).toFixed(1)} MiB`,
);
