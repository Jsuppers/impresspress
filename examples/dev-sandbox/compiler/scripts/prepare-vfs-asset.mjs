/**
 * Brotli-compress `vfs.core-<hash>.wasm` and split it into parts small enough
 * to serve as static assets, then delete the original.
 *
 * Cloudflare refuses a static asset over 25 165 824 bytes and the composed
 * component is an order of magnitude past that, so the browser reassembles it:
 * `vfs-runner.ts` reads `<name>.wasm.br.json`, fetches the parts in order and
 * pipes them through a brotli decoder into `WebAssembly.compileStreaming`.
 * The manifest shape is rubrc's (`page/src/worker_process/util_cmd.ts` reads
 * the same one) plus a `sha256` on each part and on the original wasm. Those
 * are for the BUILD: they are what lets a later run prove the split on disk is
 * a split of exactly this component before reusing it instead of spending ten
 * minutes recompressing. At runtime the reassembled stream is checked by
 * brotli itself and against `originalSize`.
 *
 * This is rubrc's `scripts/prepare-vfs-asset.mjs` reduced to the part we
 * need: no `_headers` file (the sandbox's host sets COOP/COEP), no `v1`
 * snapshot fetched out of their git, and node's own brotli rather than a
 * `brotli` binary on PATH, so the build has one less system dependency.
 *
 * Usage: node scripts/prepare-vfs-asset.mjs <dist/version dir>
 */

import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { pipeline } from "node:stream/promises";
import { Writable } from "node:stream";
import zlib from "node:zlib";

const dir = process.argv[2];
if (!dir) {
  console.error("usage: prepare-vfs-asset.mjs <dist/version dir>");
  process.exit(1);
}

const PART_BYTES = Number(process.env.VFS_PART_BYTES ?? "25165824");
if (!Number.isSafeInteger(PART_BYTES) || PART_BYTES < 1 || PART_BYTES > 25165824) {
  console.error(`VFS_PART_BYTES is ${process.env.VFS_PART_BYTES}, which is not a usable part size`);
  process.exit(1);
}

const find = (root, test) => {
  const found = [];
  for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
    const full = path.join(root, entry.name);
    if (entry.isDirectory()) found.push(...find(full, test));
    else if (entry.isFile() && test(entry.name)) found.push(full);
  }
  return found;
};

/** Splits the compressed stream into `<name>.br.part-000`, `-001`, ... */
class Splitter extends Writable {
  constructor(baseName, outDir) {
    super();
    this.baseName = baseName;
    this.outDir = outDir;
    this.parts = [];
    this.compressedSize = 0;
    this.stream = null;
    this.partBytes = 0;
  }

  async _write(chunk, _encoding, callback) {
    try {
      let offset = 0;
      while (offset < chunk.length) {
        if (!this.stream) {
          const file = `${this.baseName}.br.part-${String(this.parts.length).padStart(3, "0")}`;
          const tmp = path.join(this.outDir, `${file}.tmp`);
          this.stream = fs.createWriteStream(tmp);
          // Hashed as it is written: the manifest's per-part sha256 is what a
          // later build checks before reusing this split, and hashing here
          // costs nothing extra to read.
          this.parts.push({ file, tmp, size: 0, hash: createHash("sha256") });
        }
        const room = PART_BYTES - this.partBytes;
        const take = Math.min(chunk.length - offset, room);
        const slice = chunk.subarray(offset, offset + take);
        const drained = this.stream.write(slice);
        this.parts[this.parts.length - 1].hash.update(slice);
        this.partBytes += take;
        this.compressedSize += take;
        this.parts[this.parts.length - 1].size += take;
        offset += take;
        if (this.partBytes >= PART_BYTES) {
          await this.#closePart();
        } else if (!drained) {
          // Both listeners are removed on either outcome: a 380 MB stream
          // drains thousands of times, and leaving the loser attached each
          // round is how you get `MaxListenersExceededWarning` and a slow
          // leak on the write stream.
          const stream = this.stream;
          await new Promise((resolve, reject) => {
            const onDrain = () => {
              stream.off("error", onError);
              resolve();
            };
            const onError = (error) => {
              stream.off("drain", onDrain);
              reject(error);
            };
            stream.once("drain", onDrain);
            stream.once("error", onError);
          });
        }
      }
      callback();
    } catch (error) {
      callback(error);
    }
  }

  async _final(callback) {
    try {
      if (this.stream) await this.#closePart();
      callback();
    } catch (error) {
      callback(error);
    }
  }

  async #closePart() {
    const stream = this.stream;
    this.stream = null;
    this.partBytes = 0;
    await new Promise((resolve, reject) => {
      stream.once("error", reject);
      stream.end(resolve);
    });
  }
}

const wasmFiles = find(dir, (name) => /^vfs\.core-.*\.wasm$/.test(name));
if (wasmFiles.length === 0) {
  const manifests = find(dir, (name) => /^vfs\.core-.*\.wasm\.br\.json$/.test(name));
  if (manifests.length === 1) {
    console.log(`already split: ${path.relative(dir, manifests[0])}`);
    process.exit(0);
  }
  console.error(`${dir}: no vfs.core-*.wasm and no vfs.core-*.wasm.br.json — did the vite build emit it?`);
  process.exit(1);
}
if (wasmFiles.length > 1) {
  console.error(`${dir}: ${wasmFiles.length} vfs.core-*.wasm files, expected exactly one`);
  process.exit(1);
}

const wasmFile = wasmFiles[0];
const outDir = path.dirname(wasmFile);
const baseName = path.basename(wasmFile);
const originalSize = fs.statSync(wasmFile).size;
const manifestPath = path.join(outDir, `${baseName}.br.json`);

// A split belonging to some other component — left by an earlier pin, or by a
// `--fast` build — must not survive next to this one: `write-manifest.mjs`
// lists whatever is on disk, so it would be published.
for (const file of fs.readdirSync(outDir)) {
  if (!/^vfs\.core-.*\.wasm\.br\.(json|part-\d+)$/.test(file)) continue;
  if (file.startsWith(`${baseName}.br.`)) continue;
  fs.unlinkSync(path.join(outDir, file));
  console.log(`removed ${file}: it belongs to another component`);
}

// Compressing 365 MiB at quality 11 is ~10 minutes and most of this script's
// runtime, so an existing split is reused — but only after proving it is a
// split of THESE bytes. The name already carries vite's content hash; this
// also checks the sha256 the manifest recorded of the original wasm and that
// every part is present at the size it claims. Anything short of that
// recompresses, because a stale or truncated part is a compiler image that
// fails to instantiate half a minute into someone's first visit.
if (fs.existsSync(manifestPath)) {
  const existing = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
  const hash = createHash("sha256");
  for await (const chunk of fs.createReadStream(wasmFile)) hash.update(chunk);
  const actual = hash.digest("hex");

  const parts = Array.isArray(existing.parts) ? existing.parts : [];
  const complaint = (() => {
    if (existing.originalFile !== baseName) return `it is a split of ${existing.originalFile}`;
    if (existing.sha256 !== actual) {
      return `it was taken from ${String(existing.sha256).slice(0, 12)}…, not ${actual.slice(0, 12)}…`;
    }
    if (existing.originalSize !== originalSize) {
      return `it records ${existing.originalSize} bytes, not ${originalSize}`;
    }
    if (parts.length === 0) return "it lists no parts";
    for (const part of parts) {
      const file = path.join(outDir, part.file);
      if (!fs.existsSync(file)) return `${part.file} is missing`;
      const size = fs.statSync(file).size;
      if (size !== part.size) return `${part.file} is ${size} bytes, not ${part.size}`;
      // The bytes themselves, not just their length. A part corrupted in place
      // keeps its size, survives the aside-and-restore this script's caller
      // does around the vite build, and would then be re-hashed into
      // `dist/manifest.json` as if it were correct — the manifest would agree
      // with the disk and the disk would be wrong. A split written before this
      // check existed has no per-part hash, and is not trusted either.
      if (typeof part.sha256 !== "string") return `${part.file} predates per-part hashes`;
      const hash = createHash("sha256");
      hash.update(fs.readFileSync(file));
      const digest = hash.digest("hex");
      if (digest !== part.sha256) {
        return `${part.file} is ${digest.slice(0, 12)}…, not ${String(part.sha256).slice(0, 12)}…`;
      }
    }
    return "";
  })();

  if (complaint === "") {
    fs.unlinkSync(wasmFile);
    console.log(`${baseName}: already split, kept ${parts.length} parts (hashes verified)`);
    process.exit(0);
  }

  console.log(`${baseName}: recompressing — the existing split does not match (${complaint})`);
  for (const part of parts) {
    try {
      fs.unlinkSync(path.join(outDir, part.file));
    } catch {
      // it was already missing; that is one of the reasons we are here
    }
  }
  fs.unlinkSync(manifestPath);
}

const splitter = new Splitter(baseName, outDir);
const published = [];

// The hash of the original wasm goes into the manifest for provenance, and it
// is taken as the file streams past: `readFileSync` on 380 MB would be a
// pointless second read and a 380 MB buffer.
const originalHash = createHash("sha256");
const source = fs.createReadStream(wasmFile);
source.on("data", (chunk) => originalHash.update(chunk));

try {
  await pipeline(
    source,
    zlib.createBrotliCompress({
      params: {
        [zlib.constants.BROTLI_PARAM_QUALITY]: 11,
        [zlib.constants.BROTLI_PARAM_LGWIN]: 24,
        [zlib.constants.BROTLI_PARAM_SIZE_HINT]: originalSize,
      },
    }),
    splitter,
  );

  let total = 0;
  for (const part of splitter.parts) {
    const size = fs.statSync(part.tmp).size;
    if (size !== part.size) throw new Error(`${part.file}: wrote ${size} bytes, counted ${part.size}`);
    if (size > 25165824) throw new Error(`${part.file}: ${size} bytes is over the asset limit`);
    total += size;
  }
  if (total !== splitter.compressedSize) throw new Error("the parts do not add up");

  // Publish only once every part is known-good, so a failed run leaves the
  // previous split (or nothing) rather than a half-written one.
  for (const oldFile of fs.readdirSync(outDir)) {
    if (oldFile.startsWith(`${baseName}.br.part-`) && !oldFile.endsWith(".tmp")) {
      fs.unlinkSync(path.join(outDir, oldFile));
    }
  }
  for (const part of splitter.parts) {
    const final = path.join(outDir, part.file);
    fs.renameSync(part.tmp, final);
    published.push(final);
  }
  fs.writeFileSync(
    manifestPath,
    `${JSON.stringify(
      {
        version: 1,
        encoding: "br",
        originalFile: baseName,
        originalSize,
        compressedSize: splitter.compressedSize,
        sha256: originalHash.digest("hex"),
        parts: splitter.parts.map((part) => ({
          file: part.file,
          size: part.size,
          sha256: part.hash.digest("hex"),
        })),
      },
      null,
      2,
    )}\n`,
  );
  published.push(manifestPath);
  fs.unlinkSync(wasmFile);

  const mib = (n) => `${(n / 1024 / 1024).toFixed(1)} MiB`;
  console.log(
    `${baseName}: ${mib(originalSize)} -> ${mib(splitter.compressedSize)} in ${splitter.parts.length} parts`,
  );
} catch (error) {
  for (const file of [...published, ...splitter.parts.map((part) => part.tmp)]) {
    try {
      fs.unlinkSync(file);
    } catch {
      // best effort
    }
  }
  console.error(`prepare-vfs-asset.mjs: ${error?.message ?? error}`);
  process.exit(1);
}
