# Prepared runtime architecture: feasibility and cross-platform findings

**Date:** 2026-07-24
**Scope:** ImpressPress runtime construction and deployment on Cloudflare Workers,
native binaries, and long-running VMs/servers.
**Status:** **ARTIFACT ARCHITECTURE VALIDATED; CURRENT-ACCOUNT CPU ACCEPTANCE FAILED;
NOT PRODUCTION-READY AND NOT MERGED.** The corrected final attempt verified the two-stage
artifact identities, preview routes, immutable assets, and lack of secret values, but
the exact production P32 mixed workload exceeded the confirmed 10ms CPU limit and
produced hang cancellations. Production was rolled back and checked healthy. Never
serialize a live `Wafer`.

### Implementation update — 2026-07-24 (current branch state)

The baseline sections below intentionally preserve the reasoning and pre-change code
audit. The following changes are now present on `feat/improvements` and are not merged
to `main`:

- ordinary Cloudflare runtime construction calls the physically read-only
  `load_block_settings`; structural default inserts/updates run after admin migration
  inside `/_deploy/init`, fail closed, and are republished to the router and synchronous
  config snapshot;
- deploy creates `wrangler-upload.toml` with no `[build]` hook, uploads the already-built
  Wasm, and verifies its SHA-256 before and after `wrangler versions upload`;
- generated configs include `CF_VERSION_METADATA`; runtime identity includes the Worker
  version plus explicit application config identity, with a hashed known-value fallback
  for development configs that lack version metadata;
- cached Cloudflare runtimes retain stateless forwarding proxies rather than concrete
  D1, R2, config, crypto, network, logger, or config-source services. A fresh service
  bundle is derived from the current `worker::Env` for each request, and the scope is
  re-entered on every future poll so overlapping requests cannot select each other's
  services;
- an authenticated `/_deploy/prepare` candidate runs migrations/seeds, reloads final
  settings and grants, writes one final KV config generation, and returns a hashed
  schema-v1 `PreparedRuntimePlan`. The CLI packages that exact JSON as a Text module,
  uploads the final version without rebuilding Wasm, verifies the final candidate, and
  promotes only the final version;
- packaged plan JSON is verified once per isolate and cached by Worker version, plan
  hash, and module hash. A prepared cold hydration performs one KV generation read but
  skips the D1 block-settings and WRAP-grant discovery reads. A generation mismatch,
  local config write, or later periodic version mismatch moves that isolate to the
  existing dynamic path until a new plan/Worker identity arrives;
- the plan hash binds the application build, `wafer.lock` identity, release identity,
  structural settings, grants, routes/config, and exact post-funnel config generation.
  Final verification checks the same generation before accepting the plan;
- release files use an immutable content-addressed R2 prefix and exact sorted key
  inventory. Managed reads are redirected only for exact members; managed writes and
  deletes fail, and listing a managed folder fails explicitly rather than returning a
  mixed view. Direct R2 fast paths must use `release_asset_object_key`;
- the build guard now combines a monotonic owner token with a weak liveness token. A
  live long build is not reclaimed merely because five seconds elapsed; an orphaned
  scalar slot can be reclaimed after the grace period, and a late old owner cannot
  clear or overwrite its successor;
- the pre-promotion reliability gate accepts a validated
  `[cloudflare].deploy_smoke_paths` list, sends exactly 160 unique requests with at most
  32 in flight, rejects redirects and every other non-2xx response, and fails before
  promotion. Existing consumers default to `/health`; GDSF configures four canonical
  dynamic routes;
- runtime construction is isolate-single-flight and emits build ordinal plus measured
  build duration through structured logs and the debug-only `Server-Timing` header;
- the branch contains focused tests for poll-scoped services, deterministic plan
  hashing/packaging, generation binding, tamper rejection, immutable release routing,
  build-owner liveness/ABA behavior, strict eager initialization, and the concurrent
  release gate. Final branch validation passed: 2/2 strict-init tests; 1,204/1,204 core
  library tests; the feature-enabled `lockfile_e2e` test; all `impresspress` tests (55
  library tests plus every integration suite); the Cloudflare wasm32 check; the GDSF
  workspace's full all-target test run; and the GDSF release Cloudflare wasm32 check.

Stage 0's historical live measurement supported prototyping, and that prototype now
exists. A corrected final attempt exercised bootstrap → prepare → final upload → verify
→ promote → load → rollback. Artifact integrity passed, but the current account's CPU
behavior failed the production acceptance gate. The branch remains unmerged.

Current limitations are deliberate and should remain visible:

- prepared hydration is **not zero-I/O**: it performs one request-current KV generation
  read. This is required to reject a stale packaged plan after an admin mutation;
- KV is eventually consistent, so cross-location observation follows KV's documented
  propagation model. The generation check prevents a fresh isolate that sees a newer
  stamp from accepting an older plan, but it does not turn KV into a globally linearizable
  deployment coordinator;
- an admin mutation after deployment intentionally disables the packaged plan for that
  isolate and restores dynamic D1-backed construction. Fully zero-discovery requests
  require structural admin changes to create a new deployment;
- the Cloudflare adapter now injects the foundational services, but Wafer does not yet
  expose the generalized typed capability lookup proposed for LLM, image, vector,
  embedding, and arbitrary consumer services;
- JWT remains in the immutable Wafer config snapshot as a bounded compatibility
  exception because current CSRF/auth middleware reads it synchronously. Per-request
  crypto/config uses the current secret, and Worker-version identity forces a rebuild,
  but a live warm-isolate secret-rotation test is still required;
- remote-Wasm offline sealing is not implemented. The current Cloudflare feature set
  leaves Wafer's online remote-resolution feature off; enabling it would invalidate the
  no-network hydration assumption;
- managed release-folder listing is intentionally unsupported rather than merged with
  the immutable manifest. Consumers that need enumeration must read the manifest;
- native and VM paths remain dynamic and retain their process-owned services. The plan
  schema is target-neutral, but prepared-plan hydration is currently integrated only in
  the Cloudflare adapter.

The measured evidence supplied for the corrected attempt is recorded in section 13.3,
including the full Worker IDs and artifact hashes. The branch owner should also attach
the commands, raw result files, `/_deploy/verify` response, and Cloudflare tail export.

---

## 1. Executive verdict

The intended experience remains a sound architectural target:

1. `impresspress deploy` performs migrations, seeds, validation, and application
   preparation.
2. The deployed Worker receives an immutable description of the application.
3. A new isolate checks the KV generation, then hydrates that description without D1,
   R2, or network-based structural discovery.
4. Each request supplies its current Cloudflare bindings.
5. Only the blocks needed by the request are activated, then the response is returned.

That design is feasible. Stage 0 recorded enough reliability and cold-concurrency pain
to justify a prototype, and the feature branch now contains that prototype. The
prepared path still performs one KV generation read, registration/build/sealing CPU,
and the first route's lazy block initialization, but it imports block settings and
WRAP grants from the verified plan instead of discovering them from D1. The dynamic
fallback retains the older discovery path.

Separate correctness findings did not need a latency benchmark. The baseline could seed
block settings during a cold request and immediately force a second full build, cache
binding-derived services, run `worker-build` twice, and promote code before its R2
assets. The feature branch addresses those paths as described in the implementation
update; the remaining JWT snapshot exception and final live validation are explicit.

The important correction is what gets prepared. A live Rust object graph is not a
portable deployment artifact. `Arc<dyn Block>`, trait-object vtables, mutex state,
function pointers, cached initialization results, database clients, and Cloudflare
binding handles cannot safely be serialized at deploy time and resumed in another
isolate. The artifact must instead be stable data from which a small runtime kernel is
hydrated.

| Proposal | Verdict | Reason |
|---|---|---|
| Run migrations and all structural seeds before promotion | Implemented on branch | Keeps mutations off the request path and lets a failed candidate remain unpromoted. |
| Fix binding/secret freshness and inject request-current capabilities | Foundational services implemented; generalized capability API remains open | Binding-only changes may reuse warm isolates; cached derivatives can remain stale indefinitely. |
| Upload immutable deployment assets before promotion | Implemented for the explicit release inventory | Direct R2 consumers must use the release-key helper; rollback still needs end-to-end validation. |
| Resolve and validate block metadata, routes, grants, and structural config at deploy time | Prototype implemented | Prepared hydration imports the artifact; dynamic fallback remains intentional after admin mutation. |
| Package a versioned plan with the Worker version | Prototype implemented | The final Worker binds plan/module/build/lock/release/config-generation identity. |
| Serialize the complete `Wafer` value or Wasm memory | No | It contains executable objects, synchronization state, and host/request resources. |
| Cache clients/config derived from the first request's bindings | No | Documented binding staleness is sufficient reason; cross-request failure of D1/KV/R2 stubs is not yet demonstrated and needs an integration test. |
| Inject current services/bindings for each request | Yes | This follows the Worker request lifecycle and remains easy to map to process services on native/VM. |
| Lazily activate compiled Rust blocks after routing | Yes | Rust block code remains linked into the Worker Wasm, but activation can be deferred. |
| Resolve remote Wasm blocks on a production request | No | Wafer already supports remote resolution, but it currently occurs inside feature-gated `seal()`; preparation must resolve and pin it offline. |
| Preserve arbitrary live admin changes without redeploying while also doing zero cold-start I/O | No | Those are conflicting consistency models. Structural changes must either create a deployment or use a runtime overlay/read. |

Overall feasibility is **high for the implemented prepared-plan prototype**, **high and
correctness-justified for the implemented foundational request-service injection**, and
**low/unnecessary for a full heap snapshot**. Production readiness remains conditional
on the final branch validation and rollback tests, not on additional architectural
speculation.

## 2. Why any build is needed when the application is Wasm

“Run the Wasm and pass request in/response out” is the correct end-state for request
handling, but the word *build* currently covers three different jobs:

1. **Compile:** Rust source and selected block features are compiled into the Wasm
   program. This is unavoidable when code changes. Cloudflare runs the resulting Wasm;
   it does not run the Rust sources directly.
2. **Prepare:** ImpressPress discovers registrations, loads structural settings,
   resolves routes/flows/references, checks grants, and creates an execution plan. This
   work can and should mostly move to deployment.
3. **Instantiate:** A process or isolate creates in-memory block objects and attaches
   platform services. Some small amount of this is unavoidable because every
   Cloudflare isolate has a fresh heap. It should be deterministic, synchronous where
   possible, and contain no deployment work.

The problem was not that Wasm needs compilation. Baseline request handling did parts of
jobs 2 and 3 and deployment invoked compilation more than once. The branch now stages
Wasm once and packages preparation output separately; whether that final implementation
materially improves production performance remains a measurement question. The target
is:

```text
source change        deploy/config change         each new isolate        each request
     |                        |                           |                     |
compile Wasm  ->  prepare + validate plan  ->  hydrate immutable kernel  ->  inject bindings
                         |                                                   route
                         +-> migrate/seed candidate                           activate block
                                                                             execute/respond
```

A deployment cannot leave a ready Rust heap behind for future Cloudflare isolates.
Cloudflare may create isolates later, in different locations, and may discard them at
any time. Wasm is the executable; the prepared plan is data; a new in-memory instance is
still created wherever the executable starts.

## 3. Baseline before the Stage 0 implementation

### 3.1 Cloudflare deployment

`crates/impresspress/src/cli/flows/embed_cloudflare.rs` currently does the following:

1. Generate Wrangler configuration and stage assets.
2. Cross-compile the consumer crate to `wasm32-unknown-unknown`.
3. Upload an unpromoted Worker version.
4. Call `/_deploy/init` through that version's preview URL.
5. Promote only when initialization succeeds.
6. Upload staged R2 assets.

This is partly a good promotion boundary. `crates/impresspress-core/src/deploy_init.rs`
seals the runtime, initializes admin, seeds, initializes every registered block, and
runs `post_start`. Failures are reported before traffic is moved.

There are three important exceptions:

- the generated `wrangler.toml` contains a `[build]` command, so the explicit build in
  step 2 is followed by another `worker-build` when `wrangler versions upload` runs;
- block settings are loaded/seeded before admin migration, while the Cloudflare
  post-admin hook seeds only auto-generated variables, so a fresh production cold
  request can still perform the block-settings writes;
- R2 assets are uploaded after promotion, leaving a live stale/missing/mixed-asset
  window and no asset rollback tied to the Worker version.

However, that initialized `Wafer` exists only inside the preview request. Its block
slots and successful initialization outcomes disappear when the request/isolate is
discarded. A production request later constructs a separate runtime.

### 3.2 Request runtime construction

`crates/impresspress-cloudflare/src/lib.rs` currently creates D1, KV, R2, crypto,
network, logger, and config services; reads settings/configuration; invokes the
consumer registration closures; builds `Wafer`; and applies post-build work. The
runtime cache can then retain that object in an isolate.

This mixes two kinds of state:

- immutable application structure that is safe and useful to cache; and
- D1/KV/R2/config service objects derived from a particular `worker::Env` invocation.

Cloudflare explicitly warns that isolates are reused, request-scoped state must not be
global, and binding-only changes may reuse an existing isolate. Its binding guidance
recommends constructing derived clients per request when freshness matters. See
[Workers Best Practices][cf-best] and [Bindings][cf-bindings].

Cloudflare also forbids using request-owned I/O objects from a different invocation;
see [Errors and exceptions][cf-errors]. That rule definitely covers requests,
responses, streams, and similar objects. The documentation does not establish that a
D1/KV/R2 binding stub retained from an earlier `worker::Env` will itself trigger that
error. Treat that as an unverified compatibility risk and integration-test it; do not
describe it as a known crash-class defect. Binding/secret staleness is independently
documented and already proves the current cache boundary is wrong.

The isolate-level single-flight guard on `feat/improvements` is a valid immediate
stabilization: it prevents overlapping runtime builds from racing inside one isolate.
It should remain as a safety net while this architecture is introduced. It does not,
by itself, separate immutable application state from request-derived services.

### 3.3 Confirmed current correctness defects

Four current behaviors change the priority of this plan:

1. **Cold-build self-invalidation.** `build_runtime()` calls
   `load_and_seed_block_settings()`. When rows are missing or defaults change, its D1
   writes pass through the KV wrapper and set the isolate `DIRTY` flag. The cold branch
   stores the newly built runtime without consuming that flag, so the next request
   rebuilds unconditionally. With concurrent cold requests, a waiter can start that
   second build as soon as the first builder releases its slot.
2. **Stale JWT and consumer secrets.** The JWT secret is read from `env.secret()` into
   the config snapshot and crypto service at build time. The cache generation changes
   only on D1 writes to variables, block settings, or WRAP grants. A binding-only secret
   rotation therefore leaves the cached secret in use until isolate eviction or an
   unrelated config write. Consumer post-build snapshots can have the same problem.
3. **Non-atomic assets.** Staged `dist/`, `content/`, and `public/` files are written to
   mutable R2 keys only after the Worker is promoted. A failed partial upload leaves a
   mixed release, and Worker rollback does not restore overwritten R2 objects.
4. **Redundant compilation.** `impresspress deploy` explicitly builds, then Wrangler's
   configured custom build runs again during `versions upload`. A future two-stage
   upload would add still more rebuilds unless uploads consume an immutable staged
   bundle through an upload-only Wrangler configuration.

### 3.4 Existing Wafer snapshot is not this artifact

The current `wafer_run::StartupSnapshot` contains block/flow/interface metadata and
block config, but it is not serializable and is not a complete executable plan. It does
not contain registered `Arc<dyn Block>` implementations, aliases, resolved block slots,
runtime capabilities/WRAP state, service implementations, or initialized block state.
The internal `SealedPlan` contains more resolved execution data, but is private and also
not a stable serialized contract.

Neither type should simply gain `Serialize` and become a public file format. Internal
hash maps and compiled runtime structures change with implementation details. A
deployment artifact needs an intentionally designed, versioned schema.

### 3.5 `wafer.lock` is dependency-lock prior art, not a competing plan

`wafer-block/src/lockfile.rs` already defines `wafer.lock` schema v2 with fail-closed
version checking, deterministic package ordering, package and Wasm SHA-256 pins, source
provenance, and an explicit v1-to-v2 break. `Wafer::new` can load those pinned remote
Wasm packages from the local cache on native targets.

This is important prior art, but it does not describe the whole application. It lacks
native blocks, routes, flows, aliases, grants, application config, services,
migrations, and the application build. Automatic lockfile discovery is also skipped
on wasm32, and ImpressPress's current Cloudflare feature set does not enable remote
Wasm loading.

Do not create a second source of truth for remote packages. The intended relationship
is:

```text
wafer.lock (remote dependency identity + integrity)
    + AppDefinition
    + deployment-owned structural configuration
    -> PreparedRuntimePlan
```

If the plan proceeds, it should record the lockfile schema/digest and resolved remote
package identities. Wafer remains the owner of dependency resolution and artifact
integrity; the deployment plan owns application assembly.

## 4. Recommended architecture

Use four explicit layers.

### 4.1 `AppDefinition`: code and factories

`AppDefinition` is the target-neutral description contributed by ImpressPress and the
consumer crate:

- block factories and implementation IDs;
- block metadata and interfaces;
- route and flow declarations;
- aliases and feature selections;
- configuration schemas and secret **names**;
- deploy/activation hooks with explicit lifecycle roles.

The factories remain executable Rust code compiled into the native binary or Worker
Wasm. They are not serialized. The same definition must drive dynamic development and
production preparation so the two modes cannot drift.

The current `FnOnce(ImpresspressBuilder)` and post-build closures are convenient at
runtime but difficult for a host-side CLI to introspect without executing the consumer.
They should evolve into a deterministic registration function or generated inventory
that can produce `AppDefinition` in a host-side prepare command and in target builds.
Avoid maintaining a separate hand-written block list for Cloudflare.

### 4.2 `PreparedRuntimePlan`: stable deployment data

The plan should include only serializable, immutable control-plane data. A conceptual
schema is:

```json
{
  "schema_version": 1,
  "application_id": "example-app",
  "application_build": "sha256:...",
  "plan_hash": "sha256:...",
  "config_generation": "<exact post-funnel KV stamp>",
  "dependency_lock": {
    "schema_version": 2,
    "sha256": "sha256:..."
  },
  "assets": {
    "prefix": "deployments/<plan-hash>",
    "manifest_sha256": "sha256:..."
  },
  "blocks": [
    {
      "implementation": "impresspress/auth@1",
      "instance": "auth",
      "config": { "session_ttl_seconds": 3600 },
      "required_secret_names": ["IMPRESSPRESS_JWT_SECRET"]
    }
  ],
  "routes": [],
  "flows": [],
  "aliases": {},
  "grants": [],
  "interfaces": [],
  "migration_revision": "..."
}
```

It must not include:

- secret values, API tokens, JWT signing material, or bootstrap credentials;
- D1/KV/R2 handles or database/network clients;
- requests, responses, streams, futures, tasks, mutex state, or cached errors;
- Rust pointers, trait-object/vtable identity, or target-dependent compiled structures;
- initialized block state that depends on an external resource.

The plan should have a canonical encoding, schema version, final staged-Wasm hash,
dependency-lock hash, asset-manifest hash, and content hash. Unknown schema versions
and code/plan/dependency mismatches must fail closed with a clear deployment error.
Remote-package entries must be derived from `wafer.lock`, not copied into a separately
maintained list.

The implemented v1 schema also binds the exact KV config generation persisted after the
successful prepare funnel. That field participates in `plan_hash`; an unbound sentinel
is permitted for structure-only unit construction but is rejected by packaged-plan
validation. Cloudflare cold hydration and final verification compare it with the
request-current KV stamp before accepting the plan.

Keep a human-readable JSON copy for inspection. Deploy JSON first if it is small enough;
add a compact binary encoding only if measurement shows parsing or size is material.
Serializing internal router/hash-map layouts for a marginal speed gain would make
upgrades brittle.

### 4.3 `RuntimeKernel`: isolate/process-safe state

Hydrating a plan creates a runtime kernel containing only data safe for the lifetime of
its host:

- route matcher and resolved flow graph;
- immutable block metadata/configuration;
- block factory lookup;
- lazy activation slots that contain no request-bound handles;
- target-stable pure utilities.

Cloudflare can cache this kernel per isolate. Native/VM can cache it for the process.
Hydration should not call D1, KV, R2, `fetch`, or any migration/seed hook.

That contract requires a Wafer API boundary. Today `Wafer::seal()` can download
registry manifests, flows, and Wasm when the `wasm` feature is enabled. The feature is
off for the current Cloudflare build, so seal is network-free there today, but changing
one feature flag would silently invalidate the hydration contract. Preparation must
perform remote resolution, while a `seal_prepared`/offline sealing path rejects any
unresolved remote reference instead of fetching it.

There is still a small per-isolate cost to parse/validate the plan and create Rust
objects. That is normal. Cloudflare currently requires global-scope startup to complete
within one second and notes that large bundles or expensive global initialization hurt
startup. It also recommends moving suitable work to build time. See [Workers limits][cf-limits].
Whether plan hydration belongs in module startup or the first request should be decided
by profiling: top-level work counts against startup validation, while first-request
parsing counts against request CPU. The design supports either without external I/O.

### 4.4 `ExecutionCapabilities` and `RequestContext`: current capabilities

**Implemented subset:** the Cloudflare adapter now installs stateless forwarding
services in the cached `Wafer` and selects a concrete request-current bundle on every
poll of the dispatch future. This covers database, storage, config, crypto, network,
logger, and lazy config source, including builder/seal work performed under an explicit
scope. It solves the first-request `worker::Env` retention problem for these services.

This is not yet the generalized core API sketched below. LLM, image, optional
vector/embedding routers, and consumer-defined services have not all been migrated to
one typed capability registry. Native and VM continue to inject their normal
process-owned services directly, which is correct but does not yet exercise the same
prepared-plan path.

Every request supplies current capabilities through an extensible, typed lookup rather
than a fixed list:

```rust
enum ServiceKey {
    Database,
    Storage,
    Crypto,
    Network,
    Logger,
    Config,
    Llm,
    Image,
    Vector,
    Embedding,
    Extension(String),
}

struct RequestContext {
    capabilities: Arc<dyn ServiceCapabilities>,
    // request identity, cancellation, audit sink, etc.
}
```

The types are illustrative; the implementation should expose typed accessors rather
than require callers to downcast arbitrary values. The important property is
extensibility. In addition to the six foundational services, the current builder
registers LLM and image routers plus feature/platform-dependent vector and embedding
services. Consumer-defined capabilities must also remain possible.

Each capability needs an explicit lifetime classification:

- **request-current:** Cloudflare binding facades, secrets, request audit/cancellation;
- **kernel/isolate-safe:** immutable routers or pure caches with no captured request
  values;
- **process-safe:** native/VM connection pools and clients, injected into each request
  through cheap `Arc` clones.

On Cloudflare, request-current capabilities are derived from the current `env` and
request context. On native/VM, the provider can return facades backed by long-lived
pools. Thus the application and plan stay common while the lifecycle remains
platform-correct.

Today the foundational services and additional routers are registered as blocks inside
`Wafer`. That is the main refactor boundary. Request execution must be able to resolve
request-current capabilities from the execution context rather than from the cached
runtime object. Kernel-safe routers may remain cached only when every backend they hold
has the same safe lifetime. This is a larger but cleaner change than special-casing each
Cloudflare service.

```text
                       PreparedRuntimePlan
                               |
                 +-------------+-------------+
                 |                           |
          Cloudflare kernel            Native/VM kernel
          (per isolate)                 (per process)
                 |                           |
       current env/request bindings     Arc'd pools/services
                 +-------------+-------------+
                               |
                        execute request
```

## 5. Lazy blocks: what is and is not lazy

Rust blocks selected at compile time are already code inside the Worker Wasm. Route
matching cannot make that code disappear from the bundle. “Lazy loading a block” should
therefore mean:

- do not run its activation work until a matching route/flow needs it;
- construct cheap pure block objects from their registered factory on demand if useful;
- initialize request-independent caches once per kernel/isolate where safe;
- obtain D1/KV/R2/network capabilities from the current request, not from the first
  activation request.

Deploy-time initialization and runtime activation need separate contracts:

| Lifecycle | Purpose | May do external I/O? | Persisted in plan? |
|---|---|---:|---:|
| `describe` | Declare blocks/routes/config/interface requirements | No | Yes |
| `prepare` / `validate` | Resolve structure and validate configuration | Prefer host/deploy I/O only | Result only |
| `migrate` / `seed` | Change durable environment state | Yes, before promotion | Revision/result only |
| `hydrate` | Create kernel from the plan | No | N/A |
| `activate` | Create safe runtime-local block state | Only through an active request when required | No |
| `handle` | Execute the request/flow | Yes, through current request services | No |

The current deploy funnel initializes every block, but that does not make later block
slots initialized. The proposed split makes its value explicit: deploy-time block work
verifies dependencies and durable state; runtime activation creates ephemeral state.

Lazy activation also needs a defined concurrency policy. Concurrent first requests for
the same block must not share a future or I/O object created by one Worker invocation.
A safe implementation can serialize the state transition while allowing each waiting
request to yield independently, then retry activation with its own context. Cache only
request-independent success data. Decide explicitly whether failures are retryable,
backed off, or cached for the isolate.

Wafer already has an interpreted remote-Wasm mechanism behind its `wasm` feature:
`seal()` can fetch registry manifests and download flow/Wasm artifacts. The current
Cloudflare build leaves that feature off. The `wafer.lock` cache-loading path verifies
`wasm_sha256`, but the seal-time registry manifest currently carries artifact URLs
without a content digest. Therefore production hydration must not use that online path.
Preparation should resolve remote code into `wafer.lock`-pinned/package-pinned bytes;
offline hydration should verify those pins and fail on anything unresolved.

Separately deployed Workers/service bindings or Cloudflare dynamic workers remain
alternative plugin models, but they are not prerequisites for remote Wasm in Wafer and
should not be conflated with lazy activation of compiled Rust blocks.

## 6. Configuration and freshness policy

The architecture cannot be correct until settings are classified by lifecycle.

| Class | Examples | Source | Change takes effect |
|---|---|---|---|
| Code/static structure | available blocks, routes, interfaces, flow topology | source/generated definition | new build/deployment |
| Deploy-time structural config | block enablement, aliases, structural grants, non-secret route settings | deployment plan | new deployment |
| Runtime secret/binding | JWT secret, API keys, D1/KV/R2 bindings, log level | platform `env`/secret store | current request/binding version |
| Operational overlay | kill switch, rate, tenant policy that must change immediately | strongly defined runtime source | according to its consistency contract |
| Business data | users, content, orders, files | D1/R2/etc. | normal transaction semantics |

The current D1-backed block settings and WRAP grants are structural inputs. There are
three honest choices:

1. **Deployment-owned structure (recommended end-state):** admin changes create a
   deployment draft/new plan. Promotion makes the whole structural change atomic with
   code. Requests do no config discovery.
2. **Hybrid overlay (pragmatic migration):** embed code-derived structure, then load a
   small versioned config/grant overlay at runtime. This removes most construction work
   but retains one asynchronous freshness path. It must use a request-safe single-flight
   design and has defined stale/error behavior.
3. **Two-stage upload:** upload a bootstrap candidate, run migrations and plan generation
   through its preview URL, attach the returned plan as a Wrangler `Data`/`Text` module,
   upload a final candidate from the same immutable staged Wasm, smoke-test that version,
   and promote it. Wrangler supports additional data/text modules, but the repository's
   generated configuration currently runs `worker-build` on every `versions upload`.
   The feature branch now uses an upload-only generated configuration with the `[build]`
   hook removed and verifies the staged Wasm digest. This remains a required invariant.
   See
   [Wrangler configuration and module rules][cf-wrangler] and
   [Wrangler custom builds][cf-custom-builds].

Option 3 preserves database-derived preparation without production cold I/O, but it is
more operationally complex. The generated plan must be treated as untrusted deploy
output until the CLI validates its schema, hashes it, and tests the final candidate.

Do not store the only plan in KV merely to avoid changing the bundle. Workers KV is
eventually consistent; changes may take 60 seconds or more to appear in other locations.
That creates plan/code skew unless the application deliberately accepts it. See
[How KV works][cf-kv]. If a plan is stored externally, address it by an immutable content
hash captured in the Worker version and keep the old object available for rollback.

Cloudflare Worker versions capture code, Workers Static Assets, bindings, and
compatibility settings, but not D1/KV/R2 state. The branch therefore uploads explicit
release files under an immutable content-addressed R2 prefix and binds that identity
into the plan. Business/user storage remains outside Worker rollback, and the immutable
release rollback contract still needs a live end-to-end test.
See [Versions and deployments][cf-versions].

## 7. Deployment pipeline recommendation

### 7.1 Preferred steady-state pipeline

Use this when structural configuration is deployment-owned:

```text
impresspress deploy
  1. Produce AppDefinition using the consumer's real registrations.
  2. Prepare + validate PreparedRuntimePlan.
  3. Emit target/impresspress/runtime-plan.json for inspection.
  4. Compile/stage Wasm once, record its digest, and package the exact plan.
  5. Upload an unpromoted candidate through an upload-only Wrangler config.
  6. Run migrations/seeds and environment validation on its preview URL.
  7. Upload deployment assets under an immutable plan-hash prefix.
  8. Smoke-test normal routes and asset availability using the packaged plan.
  9. Promote (optionally gradually) and retain code/plan/asset identity in telemetry.
```

This is simple, deterministic, and makes rollback coherent.

### 7.2 Transitional pipeline for database-derived structure

```text
build and stage Wasm once; record digest
  -> generate upload-only Wrangler config (no [build] hook)
  -> upload bootstrap candidate (0% traffic)
  -> preview: migrate, seed, generate canonical plan
  -> CLI validates and stages returned plan module
  -> assert staged Wasm digest is unchanged
  -> upload final candidate from staged Wasm + plan
  -> upload immutable plan-hash-prefixed R2 assets
  -> preview: verify code/plan/asset hashes, bindings, and smoke routes
  -> promote final candidate
```

Do not promote the bootstrap version. The final smoke test is essential because it is
the first version containing the generated artifact. Gradual deployments can split
traffic between versions, so database migrations and manifest schemas must remain
backward compatible during rollout. Cloudflare documents both version skew and version
affinity concerns in [Gradual deployments][cf-gradual].

The existing preview-before-promotion flow is a good foundation for either pipeline.
`/_deploy/init` should eventually stop building a disposable production runtime and
instead expose explicit migration, plan-generation/validation, and smoke-test phases.

### 7.3 Deployment assets and rollback

The baseline `r2_upload_dir` mirrored relative paths directly into mutable R2 keys after
promotion. The branch replaces that release path with a sorted manifest and immutable
prefix uploaded before promotion. The alternatives below remain the governing contract,
not additional work for the already-versioned files.

Use one of these contracts:

1. **Workers Static Assets:** package suitable immutable application assets with the
   Worker version so Cloudflare versions and rolls them back together; or
2. **Versioned R2 assets:** upload every release under an immutable
   `deployments/<plan-or-asset-hash>/...` prefix before promotion, record the exact asset
   manifest/prefix in the Worker or plan, smoke-test it, and retain old prefixes for the
   rollback window before garbage collection.

Business/user-uploaded objects remain outside deployment versioning and must not share
a cleanup policy with immutable release assets.

## 8. Cross-platform behavior

The design is portable if the plan is target-neutral and service lifetimes remain
adapter-specific.

| Concern | Cloudflare Worker | Native development | Native production / VM |
|---|---|---|---|
| Plan source | Packaged data module or embedded bytes | Generate in memory by default; optional file | Embedded or sidecar artifact |
| Kernel lifetime | Isolate | Process, rebuilt for hot reload | Process |
| Database/storage services | Construct/inject from current request `env` | Long-lived local clients/pools | Long-lived production pools |
| Secrets | Current Worker bindings | environment/dev secret provider | environment/secret manager |
| Structural changes | New deployment or explicit overlay | Immediate rebuild/reload | Restart/redeploy or explicit overlay |
| Block activation | Lazy, request-context safe | Lazy or eager for diagnostics | Lazy or eager based on workload |
| Migrations/seeds | Preview candidate before promotion | Explicit dev bootstrap | Release job/startup command, not request path |
| Concurrency | Single-threaded isolate, overlapping async requests | Usually multithreaded | Usually multithreaded/multiprocess |

Important portability requirements:

- Do not put `worker::Env`, D1, KV, or R2 types in core plan/runtime APIs.
- Keep `MaybeSend`/wasm constraints at adapter boundaries; native synchronization must
  still be genuinely `Send + Sync`.
- Do not impose “create a database pool per request” on native/VM. Injecting an `Arc`
  facade per request is cheap while the pool remains process-owned.
- Preserve the unification already present: `impresspress/*` feature blocks use one
  `register_feature_blocks` manifest on native and wasm32. The remaining registration
  divergence is the six `wafer-run/*` middleware blocks hand-registered on wasm32
  because `linkme` does not support that target. Include those and any filesystem/dynamic
  inputs in parity tests without rebuilding the already-unified feature manifest.
- Keep development dynamic: `impresspress serve` should automatically re-prepare after
  relevant changes rather than asking developers to manage a manifest manually.

Cloudflare supports Rust Workers through Wasm, but Workers is not a general native/WASI
host: threads are unavailable and WASI support is partial/experimental. Portability
comes from ImpressPress's service abstractions and plan schema, not from assuming the
same compiled Wasm can run unchanged everywhere. See [Cloudflare WebAssembly][cf-wasm].

## 9. Developer experience

This can be developer-friendly if preparation is an implementation detail of existing
commands.

Recommended CLI behavior:

- `impresspress serve`: dynamic definition, automatic re-prepare, local bindings;
- `impresspress prepare [--target ...]`: explicit CI/debug command;
- `impresspress build`: automatically runs prepare and embeds/stages the result;
- `impresspress deploy`: prepare, upload, initialize, smoke-test, promote;
- `impresspress inspect plan`: show blocks, routes, grants, sources, hashes, and secrets
  required (names only);
- `impresspress validate plan`: verify schema and code compatibility without serving.

Developer-facing guarantees should include:

- one registration API for all targets;
- deterministic output: identical inputs produce the same plan hash;
- an actionable error naming the route/block/config field that failed preparation;
- source/provenance for each setting so developers know whether to edit code, deploy
  config, a Worker secret, or business data;
- no requirement to commit generated artifacts under `target/`;
- an escape hatch to run the dynamic path in tests and compare it with the prepared
  path;
- a documented compatibility window for plan schema upgrades.

The most likely developer-hostile outcome would be two subtly different applications:
one assembled dynamically on native and another declared in a Cloudflare-only manifest.
The `AppDefinition` single source of truth is therefore a correctness requirement, not
just an ergonomic preference.

## 10. Security and correctness requirements

1. **No secrets in the plan.** Store only secret names and validation constraints.
2. **Bind plan to the uploaded code.** Validate `schema_version`, the final staged-Wasm
   digest, feature set, dependency-lock digest, and `plan_hash` before accepting traffic.
3. **Validate capabilities twice.** Prepare-time validation gives good deploy errors;
   kernel hydration should also fail closed if the artifact is corrupt or incompatible.
4. **Pin remote code.** Reuse `wafer.lock`'s `wasm_sha256` contract for installed
   packages. Do not treat the current seal-time registry URL as pinned: its manifest
   does not carry an artifact digest. Resolve remote code during preparation and verify
   immutable bytes during offline hydration.
5. **Preserve rollback data.** Never overwrite a content-addressed plan or release-asset
   prefix needed by an older Worker version. R2 business data has a separate lifecycle.
6. **Keep deploy endpoints protected and non-public in effect.** Authentication,
   candidate-version targeting, bounded output, and replay/idempotency behavior should
   be explicit.
7. **Separate validation from mutation.** A smoke test should not seed or migrate again;
   migrations must be idempotent and compatible with the old active version.
8. **Do not cache request identity or request-owned I/O.** Lazy slots may cache immutable
   activation results, never request bodies, streams, responses, or futures created for
   another request. Binding-derived clients/config must be request-current because of
   documented staleness; test binding-stub reuse separately rather than claiming it is
   already known to throw a cross-request I/O error.
9. **Record identity in telemetry.** Log Worker version, staged-Wasm digest, dependency
   lock, plan hash, asset-manifest hash, and config-overlay revision so mixed-version
   incidents are diagnosable.

## 11. Performance expectations

A prepared plan may reduce cold-request latency and variability. Historical Stage 0
results justified building the prototype, but the improvement of the final
implementation has not yet been measured. The prepared path performs one KV generation
read, imports settings and grants from the plan, executes registration/build/sealing,
and retains lazy config reads for blocks used by the first route. The dynamic fallback
still performs D1-backed structural discovery. Compare those two paths on the same
Worker version and environment before making a production claim.

If pursued, it must not be described as “zero initialization”:

- Cloudflare still loads/instantiates the Worker module and creates a fresh heap.
- ImpressPress still validates/hydrates the plan and constructs the route matcher.
- The first use of a lazily activated block can still do permitted request-time work.
- All code selected into the Rust build remains in the Wasm bundle even when a route is
  never called.

The Worker size limit is currently 3 MB compressed on Free and 10 MB on Paid, memory is
128 MB, and global startup has a one-second limit. Plan size and parsing therefore need
measurement, especially if every block/config is embedded. These current limits are in
[Workers limits][cf-limits].

Measure, at minimum:

- each current cold-build phase separately: generation probe, block settings, WRAP
  grants, registration, `build`, `seal`, lazy config, activation, and dispatch;
- whether block-settings seeding wrote rows, set `DIRTY`, and caused build ordinal 2;
- compressed Worker size and `startup_time_ms`;
- plan bytes, parse/validation CPU, and hydrated memory;
- cold first-request latency with no matching block activation;
- first activation latency per expensive block;
- warm p50/p95/p99 latency and CPU;
- D1/KV/R2 operations on cold and warm requests (target: zero structural reads);
- 20–100 concurrent requests against a fresh isolate/candidate;
- binding/secret rotation while an isolate is reused;
- number of `worker-build` invocations per deploy and staged-Wasm digest stability;
- asset availability before promotion, partial-upload failure, and rollback behavior;
- native/VM boot time and throughput before/after.

**Current measurement gate:** Stage 0 supplied the go decision for a prototype, and the
prototype is implemented. Do not merge or call it production-ready until an end-to-end
run records the final candidate's plan/module/Wasm/asset identities and compares dynamic
versus prepared cold CPU, latency, I/O, concurrency behavior, and rollback. Foundational
request-service injection remains justified independently on correctness grounds.

Start with JSON and straightforward maps. If profiling shows the plan is material, first
reduce redundant data and avoid reparsing individual block config. Only then evaluate a
compact encoding or generated static tables. A fragile zero-copy representation is not
worth adopting without a measured bottleneck and a schema-evolution strategy.

## 12. Main risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Plan schema drifts from runtime code | Candidate fails or misroutes | Version + build/feature hash; fail before promotion. |
| `PreparedRuntimePlan` duplicates `wafer.lock` package identity | Remote dependency drift | Make the lockfile an input; record its digest/resolution instead of maintaining another package list. |
| Consumer registrations differ between prepare and target compile | Missing blocks/routes | Generate both from one `AppDefinition`; parity-test the six remaining wasm32 middleware registrations and downstream hooks. |
| Structural admin changes appear to succeed but require deployment | Operator confusion | Make UI create/show a deployment draft and status, or clearly label runtime-overlay settings. |
| Plan generated after first upload is not in that version | False belief that deploy init “prepared” production | Use deployment-owned config or the explicit two-stage upload. |
| `versions upload` reruns the generated `[build]` hook | Bootstrap/final Wasm identity drifts and deploy compiles repeatedly | Build into immutable staging, upload with a no-build config, and assert the digest. |
| External plan store is stale during global rollout | Code/plan mismatch | Package with version, or use immutable content hash and retain old objects. |
| Cold settings seeding writes through the KV wrapper | Newly built runtime is immediately dirty and rebuilt | Seed after admin migration in deploy init; make request-time settings loading read-only. |
| JWT/consumer secrets are copied into the cached runtime | Binding rotation is invisible to warm isolates | Resolve request-current secret/config capabilities; add warm-isolate rotation tests. |
| Binding stubs are retained across requests | Staleness is documented; request-context behavior is uncertain | Stop retaining derivatives for freshness and add a live Cloudflare test before labeling it a crash risk. |
| R2 assets upload after promotion under mutable keys | Missing/mixed release and incomplete rollback | Immutable release prefix or Workers Static Assets; upload and smoke-test before promotion. |
| Enabling Wafer's `wasm` feature makes `seal()` fetch remotely | Hydration contract silently gains network I/O | Split online prepare/resolution from offline sealing; reject unresolved references. |
| Cloudflare-specific lifecycle degrades native | Per-request pool/client churn | Keep adapter-owned process services and inject cheap `Arc` facades. |
| Plan becomes large/slow to parse | Startup/request CPU regression | Benchmark, deduplicate, feature-gate blocks, compact only when justified. |
| Deploy migration breaks old version during gradual rollout | Production errors | Additive/backward-compatible migrations and explicit compatibility checks. |

## 13. Incremental implementation plan

Do not rewrite the entire runtime in one change. Stage 0 is a decision gate, not the
first step of a pre-approved six-stage performance refactor. Each stage should retain a
working native and Cloudflare path.

### Stage 0 — gate: fix confirmed defects and measure

- Keep the `feat/improvements` Cloudflare build single-flight guard.
- Move mutating block-settings seeding into deploy init after admin migration; make the
  ordinary cold-request loader read-only and verify it never leaves the new runtime
  dirty.
- Make JWT and consumer binding-derived config request-current, with a warm-isolate
  secret-rotation test. Do not wait for the full plan work to fix this.
- Upload release assets immutably and smoke-test them before Worker promotion.
- Remove the redundant deploy rebuild by separating build/stage from upload, and record
  the uploaded Wasm digest.
- Measure build phases separately: config version read, D1 settings/grants, registration,
  `build`, `seal`, lazy config, block activation, and request execution.
- Add Worker version, Wasm digest, build ordinal, seed-write count, and asset identity to
  internal diagnostics.

Stage 0 exits with one of two recorded decisions:

- **Stop after correctness fixes:** the remaining cold construction cost is acceptable;
  keep the simpler dynamic runtime.
- **Proceed with prepared-plan prototyping:** measurements show a material problem and
  establish concrete CPU/latency/I/O targets for Stages 4–6.

### Stage 1 — define lifecycles and configuration classes

- Split block deploy work (`migrate`, `seed`, `validate`) from runtime `activate`.
- Classify every current block setting/grant/variable using the table in section 6.
- Decide which admin changes create deployments and which are runtime overlays.

This decision is required before the artifact schema can be stable.

### Stage 2 — extract `AppDefinition`

- Replace opaque builder-only registration with deterministic declarations plus block
  factory IDs.
- Preserve the existing shared `register_feature_blocks` manifest; do not redesign the
  already-unified `impresspress/*` feature registrations.
- Bring the remaining six wasm32 middleware registrations and downstream post-build
  mutations under the same deterministic definition or parity checks.
- Add a parity test comparing block/route/flow inventories across targets/features.

### Stage 3 — separate request-current capabilities from cached runtime structure

- Add the extensible `ExecutionCapabilities`/request context boundary to Wafer
  execution, covering LLM/image/vector/embedding and consumer extensions as well as the
  six foundational services.
- Resolve service capabilities from that context instead of service blocks retained in
  the runtime; retain a router in the kernel only if all its backends are kernel-safe.
- Make Cloudflare create binding facades per request and native clone process-owned
  service handles.
- Test secret/binding changes with reused kernels.

This stage addresses the current structural correctness risk even before plans are
serialized.

### Stage 4 — introduce `PreparedRuntimePlan` (prototype implemented)

Stage 0's measured gate passed for prototyping. The remaining bullets describe the
contract and follow-up validation, not permission to claim production readiness.

- Define a deliberately small v1 schema.
- Consume `wafer.lock` and record its digest/resolved dependency identities rather than
  duplicate its package inventory.
- Prepare, hash, serialize, inspect, validate, and hydrate it.
- Initially run dynamic and prepared construction in conformance tests and compare
  routes, flows, grants, block config, and observable responses.
- Keep internal `SealedPlan` private; compile it from the public deployment plan.
- Add an offline seal path that refuses unresolved registry/network work.

### Stage 5 — integrate deployment (implemented in code; live validation pending)

- Automatically generate/package the plan during build/deploy.
- Refactor `/_deploy/init` into explicit migration/validation/smoke behavior.
- If D1-derived structural config remains, implement the two-stage upload rather than
  claiming the first uploaded version contains the generated plan.
- Upload both bootstrap and final candidates from immutable staged Wasm through an
  upload-only Wrangler configuration; verify the digest around every upload.
- Upload immutable release assets and verify code/plan/dependency/asset hashes before
  promotion; retain all rollback artifacts.

### Stage 6 — enable lazy activation and remove obsolete rebuild work (partial)

- Make activation slots request-context safe.
- Add per-block activation telemetry and retry policy.
- Remove request-time structural build/config reads only after live verification.
- Retain a bounded recovery path for corrupt/incompatible plans; do not silently rebuild
  from production D1 unless that fallback's consistency semantics are explicit.

## 13.1 Historical Stage 0 production result (2026-07-24)

The measurement gate ran against `godosomethingfun` before the final prepared-plan and
request-service changes. It changed the decision from conditional to **prototype: GO**;
it is a baseline, not validation of the final implementation:

- The final tested artifact was 4.60 MiB raw / 1.66 MiB gzip with 2-3ms
  Cloudflare-reported startup time. Bundle startup is not the failure.
- A single-flight guard eliminated duplicate builders, request-time structural
  seeding was removed, environment/version identity was added, and a hard-
  termination lease prevents a permanently owned build slot.
- Application query batching made all six collection routes stable for
  175/175 sequential requests and reduced category collection latency from
  roughly 0.8-1.0s to 0.17-0.20s.
- A published homepage fast path that stores only pure bytes returned 200 for
  100/100 GET requests at 20-way concurrency. This validates the central
  distinction: immutable artifacts are safe to reuse; live request-derived
  runtime services are not the right cache unit.
- A fresh-version mixed dynamic burst at 16-way concurrency returned 65/160
  successes, 54 controlled build-busy 503s, and 41 Cloudflare hang
  cancellations following a builder CPU termination. Tail events captured
  `exceededCpu` at 20-24ms followed by zero-CPU hang cancellations. The
  configured ceiling is 10ms; the terminal event's larger recorded consumption
  is not evidence of a larger allowance.
- The effective per-request CPU limit is confirmed as 10ms. Time awaiting
  outbound fetches or platform binding I/O is not charged as Worker CPU; active
  JavaScript/Wasm execution before and after the await is charged. Wall latency
  must therefore be measured separately from CPU time. The Worker reports
  `usage_model=standard` and no explicit `limits.cpu_ms`, and billing state is
  unavailable to the current OAuth scope. No plan, limit, or billing setting was
  changed.

Therefore Stage 3 request-current capability injection remains required on
correctness grounds, and a prepared immutable plan is now also justified as a
performance/reliability prototype. The prototype must be measured against the
numbers above. The chosen next step is later profiling and optimization within
the 10ms limit. A different CPU allowance remains an operational alternative,
but it would not remove the stale-service correctness requirement.

**Provenance TODO for the branch owner:** add links or archived output for the exact
Worker version, Cloudflare tail events, load command, and result files behind these
historical figures. Add a separate dated subsection for the final prepared-plan run;
do not overwrite this baseline or imply it exercised the final pipeline.

## 13.2 Current feature-branch stage status

| Stage | Status on `feat/improvements` | Remaining work |
|---|---|---|
| 0 — correctness and baseline | Implemented; historical baseline recorded | Re-run the full validation suite after the final patch and attach evidence. |
| 1 — lifecycle/config classes | Partial | The prepare lifecycle marker exists, but admin UX still does not turn every structural mutation into a deployment. |
| 2 — `AppDefinition` | Partial | `PreparedPlanExporter` captures deterministic builder output; a general target-neutral `AppDefinition`/factory API and complete cross-target inventory parity remain open. |
| 3 — request-current capabilities | Partial, usable for foundational Cloudflare services | Generalize beyond the foundational services and add live warm-isolate binding/secret-rotation tests. |
| 4 — `PreparedRuntimePlan` | Prototype implemented | Complete dynamic/prepared parity and native/VM consumption tests; implement offline remote-Wasm sealing before enabling that feature. |
| 5 — deployment integration | Artifact flow validated end to end; runtime CPU gate failed | Preserve the evidence, keep production rolled back, and rerun the strengthened P32 gate only after CPU behavior is addressed. |
| 6 — lazy activation/removal | Partial | Prepared hydration removes D1 structural discovery but still performs one KV generation read and ordinary lazy block config/activation work. |

## 13.3 Corrected final deployment attempt and rollback

This run exercised the corrected two-stage artifact pipeline against
`godosomethingfun`. The identifiers below are the full values supplied in the
measurement handoff:

| Identity | Recorded value |
|---|---|
| Bootstrap/candidate Worker version | `7c9963b0-2eba-415b-bb7a-f8e711e9e5bf` |
| Final Worker version | `dee6d238-dd9d-4e0a-98b8-16829fa17cc3` |
| Staged Wasm SHA-256 | `21f44a5a8082bddcb70b8028d0ae6f445fc4a888fc2fcd7214b0843b0f0c1260` |
| Prepared plan hash | `f743874f3779c58ad12cef6c6750f82600ed3cac114a1ce606b2230307b2850e` |
| Packaged Text-module SHA-256 | `fb230e5e92af17b904966fffb6f03eeb340e45870ff2cb50558d7303145cc13a` |
| Release asset identity | `f1899dd9cdb1c04dfca193ef7791a6800994c6da582febc583328675538c53a5` |
| Post-funnel config generation | `a8f553d9d3c6f7c66b3f15505946b53b` |
| Rolled-back Worker version | `6cb010fa-d3f8-4b48-8f19-1ca6b022d0ac` |

Artifact and preview evidence:

- the candidate and final uploads used the same staged Wasm; the final upload used the
  no-build configuration rather than invoking `worker-build` again;
- the release manifest contained 33 immutable assets;
- the prepared artifact contained no secret values;
- the final preview passed the 32× destinations check before production promotion.

Production runtime evidence:

| Workload | HTTP results | Timing (seconds) | Tail evidence |
|---|---|---|---|
| Mixed 160 requests, concurrency 16 | 159 × 200, 1 controlled 503, 0 × 500, 0 hangs | avg 1.086, p50 1.178, p95 2.043, max 2.138 | Error tail empty. |
| Mixed 160 requests, concurrency 32 | 127 × 200, 8 × 503, 25 × 500 | avg 1.685, p50 1.406, p95 3.875, max 4.175 | `exceeded CPU` events followed by hang cancellations; no cross-request-promise warnings. |
| Configured retained-preview gate, concurrency 32 | 38 × 200, 122 controlled 503, 0 × 500 | avg 0.882, p50 0.030, p95 8.179, max 8.594 | Authenticated post-rollback replay over the four configured dynamic paths; correctly rejected. |

After the P32 run, sequential health had degraded to 3 successes out of 10 checks. The
deployment was rolled back to Worker version
`6cb010fa-d3f8-4b48-8f19-1ca6b022d0ac`; homepage, health, and sources were then each
verified with HTTP 200.

The configurable release gate was then completed and unit-tested. An authenticated
post-rollback replay against the retained corrected preview used the exact GDSF path
set with 160 unique requests and 32-way concurrency; its 122 non-2xx responses confirm
that the gate would fail before promotion on the current account.

The result separates two decisions:

- **Artifact architecture: pass.** Same-Wasm/no-build packaging, plan/module/build/
  release/config-generation identity, preview verification, immutable assets, and the
  no-secrets contract behaved as designed. The absence of cross-request-promise
  warnings is also consistent with the request-scoped service design.
- **Current-account production CPU acceptance: fail.** P32 produced 500 responses, CPU
  exceptions, hang cancellations, and subsequent sequential degradation. This branch
  is not production-ready and was not merged.

**Future acceptance criterion — not achieved by the observed P32 run:** rerun the exact
mixed-160 workload at concurrency 32 and require 160/160 HTTP 200 responses, zero
503/500 responses, zero hangs or CPU-limit events, an empty error tail, and healthy
sequential homepage/health/sources checks afterward. The observed result was only 127 ×
200, 8 × 503, and 25 × 500, so the strengthened gate failed. Run the future gate against
the final promoted candidate, not a lower-concurrency proxy.

## 14. Acceptance criteria

### 14.1 Immediate correctness gate

These criteria apply whether or not a prepared plan is built:

- An ordinary production request never writes or seeds block settings.
- A cold build that reads current settings does not leave `DIRTY` set and cannot cause
  an unconditional second build on the next/waiting request.
- A JWT or consumer secret/binding rotation follows an explicit policy and is observed
  by a warm isolate; the test does not rely on isolate eviction or an unrelated D1
  config write.
- Live Cloudflare tests establish the behavior of reused D1/KV/R2 binding stubs; until
  then, documentation calls cross-request failure an unverified risk, not a known bug.
- `impresspress deploy` compiles/stages Wasm once, records the digest, and uploads the
  exact staged artifact without rerunning the custom build hook.
- Release assets are fully uploaded under an immutable identity and verified before
  promotion; rollback selects a retained compatible asset set.
- Business/user R2 objects are not treated as release artifacts or garbage-collected
  with release prefixes.

### 14.2 Prepared-plan gate

Only if Stage 0 measurements justify the prepared plan, its implementation is ready
when all of the following are true:

- A production Cloudflare request never runs migrations, seeds, or structural D1
  settings/grant discovery. Deterministic compiled registration still runs while the
  in-memory `Wafer` is hydrated.
- A fresh isolate performs only the request-current KV generation check before
  hydrating the packaged structure; hydration performs no D1/R2/fetch structural I/O.
- Offline sealing rejects unresolved remote references; remote Wasm is derived from and
  verified against `wafer.lock`/content pins.
- No cached kernel object contains a request, response, stream, future, `worker::Env`, or
  stale binding-derived client/config value.
- The capability provider covers foundational, LLM, image, vector, embedding, and
  consumer-defined services without target-specific application code.
- Native development remains one command with automatic rebuild/re-prepare.
- Native/VM production can use the same plan and long-lived connection pools.
- Dynamic and prepared paths pass inventory and behavior parity tests.
- Code/plan/dependency/asset mismatch fails on the final candidate before promotion.
- Rollback restores a matching code + plan + release-asset set without relying on
  mutable KV/D1/R2 release state.
- Cold concurrency tests return bounded success/error responses—never an unresolved
  promise or cross-request I/O error.
- Measured Worker size, startup time, cold latency, and memory remain within explicit
  budgets and meet the improvement target recorded at the Stage 0 gate.

### 14.3 Current acceptance status

| Requirement | Status | Evidence still needed |
|---|---|---|
| Request-time block-settings loading is read-only | Implemented | Final branch regression run. |
| Exact Wasm reused for bootstrap/final uploads | Measured artifact pass in corrected attempt | Attach the raw upload logs for `21f44a5a8082bddcb70b8028d0ae6f445fc4a888fc2fcd7214b0843b0f0c1260`. |
| Packaged plan is schema/hash/build/lock/release/generation bound | Measured artifact pass | Attach the final `/_deploy/verify` output. |
| Fresh isolate rejects plan generation `v1` after KV reaches `v2` | Implemented and unit-tested | Live two-version/admin-mutation test across reused and fresh isolates. |
| Cached runtime retains no foundational request-derived handles | Implemented and interleaving-tested | Live binding rotation/reuse test. |
| Live long builder is not reclaimed; orphan can recover; ABA cannot overwrite | Unit-tested; production run emitted no cross-request-promise warnings | CPU-limit/hang behavior still fails the P32 gate and needs separate resolution. |
| Immutable release reads/manifest verification/direct-R2 helper | Measured artifact and rollback smoke pass | Attach raw route checks for `f1899dd9cdb1c04dfca193ef7791a6800994c6da582febc583328675538c53a5`. |
| General LLM/image/vector/embedding/consumer capability lookup | Open | Core API, migrations, and parity tests. |
| Offline remote-Wasm sealing | Open | Implement before enabling Wafer's `wasm` feature for Cloudflare. |
| Native/VM prepared-plan consumption | Open | Cross-platform hydration and process-service tests. |
| Dynamic/prepared observable parity | Open | Inventory plus route/response comparison suite. |
| Final performance and memory target | **Failed at the confirmed 10ms limit for mixed-160/P32** | Reduce Worker-local CPU, then pass the exact strengthened gate in section 13.3. |

## 15. Final recommendation

The prototype described by this document exists on `feat/improvements`, and the
corrected final attempt produced a split verdict: the artifact architecture passed, but
the current-account runtime CPU gate failed. Keep production on the verified rollback
version and do not merge. Proceed in this order:

1. Preserve the full deployment/rollback records for the values in section 13.3, plus
   the focused/native/wasm test outputs.
2. Profile and reduce the P32 Worker-local CPU path within the confirmed 10ms limit.
   Awaiting fetch or binding I/O does not consume that budget, so optimize active
   Wasm/JavaScript execution, serialization, hydration, routing, and result processing.
   A future limit/account-plan change would be a separate operational decision, not an
   architectural performance improvement.
3. Re-run the exact mixed-160/P32 gate from section 13.3 against a corrected final
   candidate. Lower concurrency, preview-only success, or the P16 result does not
   substitute for this gate.
4. Recheck sequential homepage, health, and sources after the load run and retain the
   error-tail export. Any 500, hang, CPU exception, or post-run health degradation is a
   failure.
5. Only after that gate passes should merge be considered. Generalized capabilities,
   native/VM plan use, and offline remote-Wasm sealing remain explicit follow-ups.

The architecture should continue to be named and scoped precisely:

> **ImpressPress should prepare an immutable deployment plan and inject platform
> services at execution time. It should not serialize a live `Wafer` or retain
> request-bound services, I/O objects, or initialization futures in an isolate-cached
> runtime. A request-neutral `Wafer` may be reused only when concrete platform services
> are established from the current invocation.**

The most valuable implemented change is separating application structure from
request-current foundational services. It is justified by the concrete stale
JWT/consumer-secret behavior, not by an assumed latency win. The remaining synchronous
JWT snapshot exception and non-foundational service routers must stay documented until
their live tests and capability migration are complete.

`wafer.lock` remains the dependency-integrity source, and immutable release identity is
bound into the plan. The prototype uses deterministic builder export rather than a
finished general `AppDefinition`; plan generation should therefore be described as a
contained Cloudflare optimization, not proof that the full cross-platform definition
and capability design already exists.

The desired steady-state request path is then genuinely small:

```text
request
  -> get already-hydrated immutable kernel
  -> inject current platform services
  -> match route
  -> activate only required safe block state
  -> execute
  -> response
```

That steady state is feasible and the Cloudflare prototype approximates it. Native and
VM behavior remains stable because their dynamic/process-service paths were not
replaced. The artifact design has now passed its deployment checks; the unresolved
decision gate is current-account CPU reliability under the exact production workload.

---

## References

- [Cloudflare Workers Best Practices][cf-best]
- [Cloudflare Workers errors and cross-request I/O][cf-errors]
- [Cloudflare bindings and binding-change behavior][cf-bindings]
- [Cloudflare Workers platform limits][cf-limits]
- [Cloudflare Workers versions and deployments][cf-versions]
- [Cloudflare gradual deployments and version skew][cf-gradual]
- [Cloudflare Wrangler additional modules][cf-wrangler]
- [Cloudflare Wrangler custom builds][cf-custom-builds]
- [Cloudflare Workers KV consistency][cf-kv]
- [Cloudflare WebAssembly runtime][cf-wasm]

[cf-best]: https://developers.cloudflare.com/workers/best-practices/workers-best-practices/
[cf-errors]: https://developers.cloudflare.com/workers/observability/errors/
[cf-bindings]: https://developers.cloudflare.com/workers/runtime-apis/bindings/
[cf-limits]: https://developers.cloudflare.com/workers/platform/limits/
[cf-versions]: https://developers.cloudflare.com/workers/versions-and-deployments/
[cf-gradual]: https://developers.cloudflare.com/workers/versions-and-deployments/gradual-deployments/
[cf-wrangler]: https://developers.cloudflare.com/workers/wrangler/configuration/
[cf-custom-builds]: https://developers.cloudflare.com/workers/wrangler/custom-builds/
[cf-kv]: https://developers.cloudflare.com/kv/concepts/how-kv-works/
[cf-wasm]: https://developers.cloudflare.com/workers/runtime-apis/webassembly/
