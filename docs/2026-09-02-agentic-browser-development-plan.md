# Agentic browser development for ImpressPress

Date: 2026-09-02
Status: proposed implementation plan
Target: browser-Wasm deployment only for the first release
Feature gate: browser-devtools, disabled by default

This is the durable execution plan for developing an ImpressPress site inside
the browser: edit frontend files, compile new WAFER guest blocks with Rubrc,
test them, expose their typed operations to a browser agent through WebMCP, and
activate or roll back a combined site/backend release without rebuilding the
outer ImpressPress service-worker package.

> **2026-09-02 note.** The dev.impresspress.org sandbox design
> (`docs/superpowers/specs/2026-09-02-dev-sandbox-design.md`) adopts this
> plan's architecture and supersedes it where the two differ: activation is
> automatic with a generation ledger instead of a confirmation nonce, the live
> site is the preview, guest capabilities are declared and granted exactly,
> v1 guests speak JSON for host calls and get table-scoped DDL (wafer-run),
> curated shop tools exist on `/b/dev`, and export is a runnable bundle. Its
> §17 replaces the phases below.

Check a task only when its implementation and stated verification both pass.
When implementation evidence changes a design decision, add a dated note here
instead of silently changing the architecture.

## 1. Outcome

An explicitly opted-in browser build provides an administrator-only development
workspace at /b/dev. A person or WebMCP-capable browser agent can:

1. Create and edit a project containing HTML, CSS, JavaScript and Rust guest
   block source.
2. Compile dependency-free Rust blocks to wasm32-wasip1 in a dedicated Rubrc
   worker owned by the development page.
3. Read structured compiler diagnostics and iterate on source.
4. Validate and test the guest through WAFER's stable ABI with a deny-by-default
   host context.
5. Preview site files without publishing them.
6. Prepare one release containing a site revision and zero or more block
   artifacts.
7. Review the release diff and explicitly activate it.
8. Serve the frontend through the existing wafer-run/web block.
9. Execute activated backend blocks inside the ImpressPress service worker via
   wasmi.
10. Discover activated blocks' curated agent tools through the existing
    auth-filtered WebMCP manifest.
11. Roll back to the last known-good release.

Normal builds contain no development routes, Rubrc assets or wasmi interpreter
overhead.

## 2. Verified feasibility

The research worktree already proves the narrow technical overlap:

- impresspress-web compiles for wasm32-unknown-unknown with wafer-run/wasmi.
- wasm-pack produces the complete service-worker package with that feature.
- A dependency-free guest compiles for wasm32-wasip1.
- WasmiBlock::load_from_bytes loads the guest and successfully invokes its
  metadata, lifecycle and handler ABI.
- The proof guest returns:

      Hello from a browser-compiled WAFER block!

- The optimized host cost measured in the spike is approximately 1.52 MB raw or
  504 KB gzip.
- The guest is 28,469 bytes.
- ImpressPress already registers wafer-run/web and routes the site fallback to
  its site storage folder.
- BrowserStorageService already persists through OPFS.
- The WebMCP work merged by PR 72 derives an auth-filtered manifest from typed
  BlockInfo endpoint declarations and registers HTTP-backed tools in the page.

The proof and measurements are recorded in:

    experiments/browser-service-worker-blocks/README.md

## 3. Fixed architecture decisions

- [x] Keep Rubrc out of the ServiceWorkerGlobalScope. Its compiler runs in a
  dedicated worker owned by /b/dev and may create its own subordinate workers.
- [x] Keep the service worker responsible for authenticated control-plane
  endpoints, persistent storage, artifact validation, runtime activation and
  request execution.
- [x] Name the optional control-plane block impresspress/dev. Rubrc is an
  adapter behind that block's page, not the public product name.
- [x] Reuse wafer-run/web for the activated public site. Do not create a second
  production static-file server.
- [x] Store source, build metadata, artifacts and releases in origin-local
  browser storage. No server is required for the first implementation.
- [x] Treat frontend and backend changes as one versioned release even though
  they use different runtime mechanisms.
- [x] Use content-addressed artifacts and immutable site assets. Publish the
  HTML entry point last.
- [x] Rebuild a complete sealed WAFER runtime for an activated block set and
  swap the active runtime atomically. Do not mutate a sealed Wafer.
- [x] Hold the active runtime in Rc<Wafer>. Each request clones the Rc before
  awaiting so in-flight requests safely retain the previous runtime.
- [x] Use the existing global WebMCP manifest for activated application blocks.
- [x] Register development tools only on /b/dev. Do not advertise privileged
  development mutations on every page an administrator visits.
- [x] Require both a compile-time feature and an explicit runtime setting.
- [x] Require administrator authentication for every development API.
- [x] Keep capability grants empty by default. A guest must declare and receive
  an approved capability before it can call a host service.
- [x] Start with dependency-free guest templates and generated ABI-v1 glue.
  Normal wafer-sdk dependencies and #[wafer_block] procedural macros are later
  work because Rubrc does not yet provide a dependable public workflow for
  them.
- [x] Do not attempt to rebuild the outer ImpressPress wasm32-unknown-unknown
  package in the browser during this project.

## 4. Non-goals for the first release

- Editing or replacing built-in impresspress-core blocks in place.
- Compiling the outer service worker, wasm-bindgen glue or loader in the
  browser.
- Installing arbitrary Cargo dependencies from the network.
- Supporting procedural macros in Rubrc.
- Providing shell access, arbitrary commands or unrestricted same-origin HTTP
  tools to an agent.
- Deploying changes to Cloudflare, a native server or another origin.
- Multi-user collaborative editing.
- Treating browser-local code or data as a protected production secret.
- Running untrusted third-party guest code without verified fuel, memory and
  capability limits.

Built-in behavior can be extended with new guest blocks and new routes. Editing
the built-in admin UI still requires a normal outer build.

## 5. Target architecture

    /b/dev document
      |
      +-- source editor and release UI
      |
      +-- page-scoped WebMCP tools
      |     +-- storage-backed HTTP tools
      |     +-- compiler-worker tools
      |     +-- human-confirmed activation tools
      |
      +-- RubrcCompiler adapter
            |
            +-- dedicated compiler Worker
                  +-- Rubrc toolchain and VFS
                  +-- subordinate workers
                  +-- wasm32-wasip1 artifact
                            |
                            v
    impresspress/dev block in the service worker
      +-- projects and files
      +-- builds and diagnostics
      +-- artifact validation
      +-- site preview and publishing
      +-- release preparation and rollback
                            |
                            v
    BrowserRuntimeManager
      +-- active Rc<Wafer>
      +-- candidate runtime construction
      +-- persisted active release manifest
      +-- atomic runtime swap
         |
         +-- wafer-run/web reads activated site files
         +-- WasmiBlock executes activated guest blocks
         +-- WebMCP manifest discovers their agent tools

The document is the coordinator. It can communicate with both the dedicated
compiler worker and the service worker. The service worker never needs to
create or own the compiler worker.

## 6. Feature and packaging model

Add the following opt-in feature structure:

    impresspress-web:
      dynamic-wasm-blocks = wafer-run/wasmi
      browser-devtools =
        dynamic-wasm-blocks
        + development block
        + development bindings

The current dynamic-wasm-blocks spike remains a low-level capability.
browser-devtools becomes the public feature used by development builds.

The runtime gate is:

    IMPRESSPRESS__DEV__ENABLED=false

For the browser target, add an explicit bundle setting:

    [dev]
    enabled = true

The bundler renders this boolean into the service-worker bootstrap and passes it
to the Wasm initialize call. Initialization publishes the corresponding config
key before constructing the builder and conditionally adds the development block
and route. Do not source this decision from a URL, localStorage or an unverified
page message.

The development block may be compiled in while this value is false, but it must
not register its route, run its migrations, load compiler assets or advertise
development tools. Changing the gate requires rebuilding/reloading the browser
bundle so route and discovery state cannot drift.

Rubrc's compiler payload must not be embedded with include_bytes into the Rust
Wasm module. Its live manifest measured approximately 384 MB uncompressed and
55.9 MB compressed. Package versioned compiler assets as ordinary same-origin
files under a development-only prefix such as:

    /__impresspress_dev/compiler/<version>/

Requirements:

- The prefix is absent from normal distributions.
- The development build adds the prefix to the service-worker network bypass
  list or a dedicated static-asset branch.
- The editor lazily downloads the compiler only when compilation is requested.
- Versioned assets receive long-lived immutable caching.
- The editor shows download and initialization progress.
- Only one compilation per page runs at a time.
- Cancellation terminates or resets the compiler worker cleanly.
- Compiler initialization failure does not affect normal service-worker
  request handling.

The /b/dev document must return:

    Cross-Origin-Opener-Policy: same-origin
    Cross-Origin-Embedder-Policy: require-corp
    Cache-Control: no-store

Apply these headers to the development document, not globally. A global opener
policy can disrupt the existing popup authentication flow.

## 7. Persistent model

Use SQLite for transactional metadata and OPFS-backed storage for source and
binary content.

### 7.1 SQLite tables

Create development-only migrations owned by impresspress/dev:

    browser_dev_projects
      id
      name
      created_at
      updated_at

    browser_dev_builds
      id
      project_id
      source_revision
      artifact_sha256
      block_info_json
      diagnostics_json
      status
      created_at

    browser_dev_releases
      id
      project_id
      parent_release_id
      site_manifest_json
      block_manifest_json
      status
      created_at
      activated_at
      failure_message

    browser_dev_runtime_state
      singleton_id
      active_release_id
      desired_release_id
      activation_phase
      generation
      updated_at

    browser_dev_confirmations
      nonce_hash
      release_id
      action
      expires_at
      used_at

Statuses and activation phases must be closed enums in Rust, not arbitrary
strings spread across handlers.

### 7.2 OPFS layout

All paths still pass through ImpressPress storage namespace isolation:

    impresspress/dev/projects/<project-id>/...
    impresspress/dev/artifacts/<sha256>.wasm
    impresspress/dev/release-manifests/<release-id>.json
    wafer-run/web/site/index.html
    wafer-run/web/site/assets/<content-hash>.<ext>

The development block needs an explicit WRAP grant for write access only to:

    @wafer-run/web/site

It must not receive blanket access to wafer-run/web or another block's storage.

### 7.3 Release manifest

Define a versioned, canonical JSON contract:

    {
      "schema_version": 1,
      "release_id": "...",
      "parent_release_id": "...",
      "site": {
        "entrypoint_sha256": "...",
        "files": [
          {
            "path": "index.html",
            "sha256": "...",
            "content_type": "text/html; charset=utf-8"
          }
        ]
      },
      "blocks": [
        {
          "name": "site/example",
          "artifact_sha256": "...",
          "routes": [
            {
              "prefix": "/b/example/",
              "access": "Authenticated"
            }
          ],
          "capabilities": []
        }
      ]
    }

Canonicalize and hash the manifest. Every referenced file and artifact must be
present and hash-verified before a release becomes prepared.

## 8. Storage foundation

BrowserStorageService currently passes slash-containing folder and key strings
directly to OPFS getDirectoryHandle/getFileHandle calls. OPFS names cannot
contain path separators, so hierarchical projects and ordinary site asset
paths require a storage fix before the development block.

Implement shared path helpers in crates/impresspress-browser/js/bridge.js:

- Split folders and keys into validated path segments.
- Reject empty, dot and dot-dot segments.
- Traverse intermediate directories explicitly.
- Create parents only for mutating operations that request creation.
- Preserve metadata sidecars beside the final file.
- Delete empty directories only when explicitly requested; a file delete must
  not recursively delete unrelated siblings.
- Make list support nested keys and stable prefix ordering.
- Exclude metadata sidecars from returned listings.
- Keep cursor and offset semantics stable.

Verification:

- Unit tests for normalization and traversal rejection.
- wasm-bindgen browser tests for nested put/get/list/delete.
- Regression tests for existing flat keys.
- Tests that block namespace resolution still prevents cross-block access.
- Test a real wafer-run/web request for assets/app.js from OPFS.

## 9. Runtime manager

### 9.1 Safe request ownership

Refactor crates/impresspress-browser/src/runtime.rs:

- Replace RefCell<Option<Wafer>> with RefCell<Option<Rc<Wafer>>>.
- Clone the Rc synchronously before request conversion or dispatch awaits.
- Remove the raw pointer and its safety invariant.
- Keep first initialization explicit.
- Add an internal replace_runtime function available only to the browser
  runtime controller.
- Return the previous Rc from replacement so activation can roll back if a
  later step fails.

Tests must prove:

- Dispatch before initialization returns 503.
- The first store succeeds and duplicate cold initialization fails.
- A replacement changes subsequent requests.
- A request that started before replacement completes on the old runtime.
- Dropping the old global reference does not invalidate an in-flight request.

### 9.2 Runtime factory

Split crates/impresspress-web/src/lib.rs into:

    lib.rs
    runtime_manager.rs
    runtime_factory.rs
    dynamic_blocks.rs
    dev/

RuntimeFactory owns clonable Arc handles for:

- database
- storage
- config
- crypto
- network
- logger
- browser LLM/image/vector/embedding services
- BlockSettings handle
- JWT secret handle
- asset loader configuration

Cold initialization keeps the current invariant:

    db_init
    build
    seal
    admin Init and migrations
    seed/load variables and settings
    init remaining blocks
    store runtime

Reload construction:

1. Read the desired release manifest.
2. Hash-verify every guest artifact.
3. Load each guest with WasmiBlock::load_from_bytes.
4. Verify guest BlockInfo against its manifest.
5. Add each block with ImpresspressBuilder::extra_block.
6. Add each approved route with ImpresspressBuilder::add_route.
7. Build and seal a completely new Wafer.
8. Initialize blocks with the already loaded service/config handles.
9. Update storage WRAP grants for the new runtime.
10. Return Rc<Wafer> without modifying the active runtime.

The first implementation may rerun idempotent block lifecycle initialization
during reload, but it must not rerun db_init or recreate/purge OPFS. Measure the
reload. If repeated built-in migrations or initialization are materially
expensive or unsafe, add a Wafer reload lifecycle or reuse-safe registration
path rather than silently skipping required initialization.

### 9.3 Activation coordination

RuntimeManager owns:

- the RuntimeFactory
- the active release id
- an optional prepared candidate
- an activation-in-progress flag

Activation sequence:

1. Verify administrator confirmation and consume its one-time nonce.
2. Set desired_release_id and activation_phase in SQLite.
3. Enter an activation gate that prevents ordinary requests from observing a
   half-activated release.
4. Write all immutable content-hashed site assets.
5. Construct and validate the candidate Rc<Wafer>.
6. Swap the active runtime, retaining the previous Rc.
7. Write index.html last with no-cache semantics.
8. Commit active_release_id, increment generation and clear the desired state.
9. Release the activation gate.

If a step after the runtime swap fails, restore the previous Rc and previous
entrypoint before releasing the gate.

On service-worker startup, a non-empty desired_release_id is a recovery journal.
Initialization must converge to that desired release or restore the previously
active release before accepting normal requests. Never serve indefinitely from
an ambiguous phase.

## 10. Dynamic block validation

Define DynamicBlockSpec and validate it before candidate construction:

- Artifact hash matches the stored bytes.
- Wasmi can compile/instantiate it.
- BlockInfo parses and passes WAFER validation.
- BlockInfo name exactly matches the release manifest.
- Names are restricted to an application namespace such as site/<name>.
- Names beginning with wafer-run/ or impresspress/ are reserved.
- Route prefixes are normalized and cannot shadow built-in routes.
- Duplicate block names, route prefixes and WebMCP tool names are rejected.
- Extra routes remain lower priority than built-ins.
- Declared endpoint paths are consistent with approved route prefixes.
- Route access cannot be weaker than the approved release policy.
- Capabilities default to none and are compared to an explicit approval list.
- Artifact size, module memory and execution fuel limits are enforced.
- Lifecycle and one deny-all test invocation complete without trapping.

Before accepting code from outside the local administrator, verify that the
pinned wasmi/WAFER integration provides hard fuel and memory limits. If it does
not, add them in wafer-run before calling the guest execution sandboxed.

A block trap must fail the request or candidate without poisoning the outer
service-worker runtime.

## 11. Development block and HTTP contracts

Register BrowserDevBlock from impresspress-web with:

    extra_block("impresspress/dev", ...)
    add_route("/b/dev", "impresspress/dev", Admin)

Keeping it in impresspress-web avoids adding browser/Rubrc concerns to the
cross-platform core. The block receives browser-specific project storage and a
unit RuntimeControl service that reaches RuntimeManager through the
single-threaded browser runtime.

All JSON contracts use typed Serialize, Deserialize and JsonSchema structs.
Every unsafe-method endpoint passes the existing CSRF policy and administrator
router gate.

Suggested endpoints:

    GET  /b/dev
      Development document.

    GET  /b/dev/api/status
      Feature state, compiler version, active release and runtime generation.

    GET  /b/dev/api/projects
    POST /b/dev/api/projects
      List and create projects.

    GET  /b/dev/api/projects/{project_id}/files
      Stable recursive listing with hashes.

    POST /b/dev/api/projects/{project_id}/files/read
      Read one UTF-8 or binary file by validated relative path.

    POST /b/dev/api/projects/{project_id}/files/write
      Write with expected_sha256 optimistic concurrency.

    POST /b/dev/api/projects/{project_id}/files/delete
      Delete with expected_sha256.

    GET  /b/dev/api/projects/{project_id}/preview/{path}
      Serve workspace site files with no-store and a restrictive preview CSP.

    POST /b/dev/api/builds/stage
      Receive compiler result, artifact bytes, source revision and diagnostics.

    POST /b/dev/api/builds/{build_id}/validate
      Run ABI, metadata, route, capability and deny-all execution validation.

    POST /b/dev/api/releases/prepare
      Build a canonical release manifest and return its diff.

    GET  /b/dev/api/releases/{release_id}
      Read status, manifest, validation and activation diagnostics.

    POST /b/dev/api/releases/{release_id}/confirm
      Human UI action creates a short-lived one-time activation nonce.

    POST /b/dev/api/releases/{release_id}/activate
      Consume the nonce and invoke RuntimeManager activation.

    POST /b/dev/api/releases/{release_id}/rollback
      Prepare activation of a known previous release; also requires confirmation.

Initial artifact upload may use base64 inside a typed JSON request because proof
guests are small. Add a bounded streaming application/wasm upload before raising
the artifact size limit.

## 12. Site editing and publishing

The public site is already routed to wafer-run/web with:

    web_root = site
    web_spa = true
    web_index = index.html

wafer-run/web reads storage for every request. HTML receives no-cache; hashed
assets receive immutable caching. The development publisher should use those
semantics rather than introduce a new serving block.

Requirements:

- Workspace files may use ordinary nested paths.
- Preview reads from the project's workspace, never the active site folder.
- Preview HTML runs in a sandboxed iframe.
- Publishing transforms mutable asset names to content-hashed names or verifies
  that the project already generated hashed assets.
- Immutable assets are written before index.html.
- Old hashed assets remain available while any retained release references
  them.
- Garbage collection deletes assets only when no retained release references
  them.
- Rollback restores a prior index.html and corresponding backend block set.
- A build diagnostic warns if the consumer configured a service-worker bypass
  prefix that would cause network assets to override the OPFS-managed site.
- Root serving explicitly enables WAFER_RUN_SHARED__HAS_LANDING_PAGE for an
  active site release. Removing the active site restores the normal root
  redirect behavior.

For the first release, frontend source is HTML, CSS and JavaScript. A future
dedicated bundler worker may add TypeScript or framework builds. Rubrc alone
does not make a Rust wasm32-unknown-unknown frontend viable because that path
also requires dependency resolution and wasm-bindgen tooling.

## 13. Rubrc compiler adapter

Rubrc is currently an application, not a stable small compiler SDK. Hide its
details behind a narrow JavaScript interface:

    interface BrowserRustCompiler {
      initialize(onProgress): Promise<void>
      compile(projectSnapshot, options): Promise<CompileResult>
      cancel(buildId): Promise<void>
      dispose(): Promise<void>
    }

CompileResult contains:

    buildId
    success
    target
    artifactBytes
    stdout
    stderr
    diagnostics
    elapsedMs
    compilerVersion

Implementation tasks:

- Pin an exact Rubrc revision and record its license and asset hashes.
- Extract the minimum compiler/VFS worker integration from the application.
- Do not couple the ImpressPress editor to Rubrc's React or Monaco UI.
- Create the compiler worker with new Worker(..., { type: "module" }).
- Preserve Rubrc's subordinate worker URL resolution after packaging.
- Map Rubrc diagnostics into stable file, line, column, severity and message
  records.
- Compile with target wasm32-wasip1 and release-size settings.
- Use a generated dependency-free WAFER ABI-v1 adapter.
- Reject unexpected target/output files.
- Upload only after a successful compile and local SHA-256 calculation.
- Terminate and recreate the worker after an unrecoverable compiler failure.
- Cache the pinned compiler assets, not mutable unversioned URLs.

The first template exposes a small Rust handler API while generated code owns:

- __wafer_alloc
- __wafer_info
- __wafer_handle
- __wafer_lifecycle
- JSON request/response framing
- BlockInfo and endpoint metadata

The generated adapter must have golden tests against WAFER's ABI-v1 host. Keep
the handwritten proof guest as the smallest compatibility fixture.

## 14. Page-scoped WebMCP development tools

The merged global WebMCP registration script fetches the server manifest once
and translates each tool into an HTTP request. Keep that behavior for normal
application blocks.

Development tools are different:

- They are highly privileged mutations.
- Compilation executes in a page-owned worker rather than an HTTP handler.
- They should exist only while the trusted development document is open.

Therefore /b/dev registers its own tools directly with
document.modelContext.registerTool and an AbortController. Abort all
registrations when the page is unloaded or the administrator session is lost.
Do not annotate the underlying development endpoints with global agent_tool
metadata unless WebMCP later gains an enforced page-scope concept.

Initial tools:

    impresspress_dev_status
      Read-only. Return feature, runtime, project and compiler state.

    impresspress_dev_list_files
      Read-only. List project files and hashes.

    impresspress_dev_read_file
      Read-only. Return one source file and its hash.

    impresspress_dev_write_file
      Mutating. Require path, full content and expected_sha256.

    impresspress_dev_delete_file
      Mutating. Require path and expected_sha256.

    impresspress_dev_compile_block
      Page-local. Snapshot source, invoke Rubrc, stage the artifact and return
      structured diagnostics plus build id.

    impresspress_dev_validate_build
      Run server-side ABI and deny-all validation.

    impresspress_dev_preview_site
      Return a preview URL and changed-file summary.

    impresspress_dev_prepare_release
      Produce an immutable candidate and frontend/backend diff.

    impresspress_dev_activate_release
      Page-local confirmation wrapper. Show the diff to the human and call the
      confirmation/activation endpoints only after approval.

    impresspress_dev_rollback
      Page-local confirmation wrapper for a known retained release.

Tool rules:

- Names remain stable and explicitly curated.
- Input and output schemas are typed and self-contained.
- Every result includes structuredContent where supported.
- Mutating descriptions state their side effects.
- Compiler errors are successful tool results with success=false and structured
  diagnostics, not transport failures.
- Auth/CSRF/server validation remains authoritative; hiding a tool is never the
  security gate.
- Never expose an arbitrary fetch, arbitrary route, shell, eval or raw OPFS
  tool.

After runtime activation, reload or refresh the global WebMCP registrations so
the new block tools appear. The first implementation may reload /b/dev. A later
implementation can register manifest tools under an AbortSignal, abort the old
set, fetch the new generation and re-register, allowing the browser's
toolchange event to notify the agent.

## 15. Agentic workflow

The intended agent loop is:

1. Read project status and file inventory.
2. Read only relevant files.
3. Write with optimistic concurrency.
4. Compile.
5. Inspect structured diagnostics.
6. Repeat edits until compilation succeeds.
7. Validate the guest under deny-all capabilities.
8. Preview site files.
9. Prepare a combined release.
10. Present the frontend/backend diff and validation report.
11. Pause for human activation confirmation.
12. Activate.
13. Refresh WebMCP tools.
14. Invoke the new block's curated tools to verify live behavior.
15. Roll back with human confirmation if verification fails.

WebMCP is the tool transport, not the agent. A WebMCP-capable browser agent can
drive this workflow. A future in-page agent can use the same development service
interfaces without changing the runtime architecture.

## 16. Security model

Development mode creates code-writing and code-execution authority, so all of
the following are release blockers:

- browser-devtools is disabled by default.
- IMPRESSPRESS__DEV__ENABLED is false by default and is checked before route
  registration.
- Production build checks reject accidental inclusion unless explicitly
  allowed.
- Runtime development setting is also false by default.
- Every /b/dev route is Admin.
- The page and APIs are same-origin and use the existing cookie, CSRF and
  routing authorization.
- Development responses are no-store.
- Project paths are normalized relative paths with fixed size/count quotas.
- Writes use expected hashes to prevent silent lost updates.
- Artifacts and manifests are content-addressed and hash-verified.
- Reserved block names and built-in route collisions are rejected.
- Guest capabilities are deny-by-default and included in the human-visible
  release diff.
- Network access is absent by default.
- Site publication cannot write framework service-worker files, loader files or
  the outer Wasm package.
- Activation and rollback require a short-lived, single-use confirmation.
- Page-scoped development tools are absent from ordinary site pages.
- Activation keeps a last-known-good runtime and site entrypoint.
- Startup recovery resolves any incomplete activation journal.
- Compiler worker messages validate origin, shape, build id and transfer sizes.
- Compiler and guest resource limits are enforced.
- Source, compiler logs, build hashes and activation records are retained for
  audit and recovery.

Prompt-injection boundary:

The existing WebMCP design correctly notes that page content can steer a browser
agent acting with the user's ambient authority. Development mutations must
therefore be registered only on the trusted /b/dev page. Preview content runs in
a sandboxed iframe and cannot register or invoke parent development tools.
Human confirmation is mandatory for activation and rollback even if an agent
prepared the release.

## 17. Implementation phases

### Phase 0 — Integrate prerequisites and preserve the proof

- [ ] Rebase the research branch onto origin/main containing PR 72.
- [ ] Resolve the wafer-run revision change and reconfirm WasmiBlock APIs.
- [ ] Preserve dynamic-wasm-blocks as an opt-in feature.
- [ ] Rebuild the proof guest and verifier against the integrated revision.
- [ ] Run the current browser-Wasm and WebMCP test baselines.
- [ ] Record updated host-size and guest-size measurements.

Checkpoint:

- The existing application is unchanged when browser-devtools is off.
- The proof guest still executes through wasmi.
- The WebMCP manifest and browser registration tests pass.

### Phase 1 — Hierarchical OPFS

- [ ] Add safe recursive folder/key resolution.
- [ ] Add nested put/get/delete/list support.
- [ ] Preserve metadata and cursor behavior.
- [ ] Add browser tests for traversal, nested assets and namespace isolation.
- [ ] Serve a nested JavaScript asset through wafer-run/web end to end.

Checkpoint:

- A project tree and assets/app.js can be stored, listed, retrieved and deleted
  without escaping the caller's namespace.

### Phase 2 — Safe runtime replacement

- [ ] Change runtime storage to Rc<Wafer>.
- [ ] Remove the raw pointer across await.
- [ ] Add safe replace and rollback operations.
- [ ] Extract RuntimeFactory and RuntimeManager.
- [ ] Load a persisted dynamic-block manifest at cold startup.
- [ ] Rebuild a candidate from a precompiled proof guest.
- [ ] Swap runtimes while an old request remains in flight.
- [ ] Recover from a deliberately interrupted activation journal.

Checkpoint:

- Uploading a precompiled fixture, without Rubrc or an editor, activates a new
  route and survives service-worker restart.
- Failed validation/build leaves the current runtime active.

### Phase 3 — Development control plane

- [ ] Add the browser-devtools feature.
- [ ] Add BrowserDevBlock, migrations and Admin route.
- [ ] Implement project and file APIs with optimistic concurrency.
- [ ] Add artifact staging and validation.
- [ ] Add release preparation, confirmation, activation and rollback.
- [ ] Add an activation gate and startup recovery.
- [ ] Verify the complete feature is absent when disabled.

Checkpoint:

- An administrator can use HTTP APIs to edit files, stage the fixture block,
  prepare a release, activate it and roll back.
- Anonymous and non-admin callers see neither data nor actionable tool names.

### Phase 4 — Site workspace, preview and publishing

- [ ] Add a minimal /b/dev file tree/editor interface.
- [ ] Add sandboxed site preview.
- [ ] Publish immutable assets and index.html last.
- [ ] Enable root landing-page routing only for an active release.
- [ ] Add retained-release asset references and garbage collection.
- [ ] Add combined frontend/backend release diffs.

Checkpoint:

- Editing index.html and an asset updates the public wafer-run/web site after
  activation and restores the previous version after rollback.

### Phase 5 — Page-scoped WebMCP tools

- [ ] Register development tools only from /b/dev.
- [ ] Implement typed read/write/status tools.
- [ ] Implement prepare/preview tools.
- [ ] Add human-confirming activate and rollback wrappers.
- [ ] Abort registrations on logout/unload.
- [ ] Sandbox preview content away from parent tools.
- [ ] Add a tool-registration refresh after runtime generation changes.

Checkpoint:

- A WebMCP test harness can edit a frontend file, prepare a release and stop at
  confirmation.
- No development tool is visible on a normal site page.

### Phase 6 — Rubrc worker

- [ ] Pin and package the Rubrc compiler assets.
- [ ] Implement BrowserRustCompiler and its dedicated worker.
- [ ] Verify crossOriginIsolated on /b/dev.
- [ ] Add lazy download, progress, caching, cancellation and disposal.
- [ ] Compile the dependency-free WAFER template.
- [ ] Normalize diagnostics.
- [ ] Upload and validate the resulting artifact.
- [ ] Add impresspress_dev_compile_block as a page-local WebMCP tool.

Checkpoint:

- An agent changes Rust source, compiles it in the dedicated worker, receives a
  useful diagnostic for a broken build, fixes it, and stages a valid guest.

### Phase 7 — Dynamic application WebMCP tools

- [ ] Extend the generated guest BlockInfo template with typed endpoints and
  curated agent_tool metadata.
- [ ] Reject duplicate or invalid tool declarations during validation.
- [ ] Confirm a newly activated block appears in the auth-filtered manifest.
- [ ] Refresh registrations after activation.
- [ ] Invoke the new tool through the WebMCP browser harness.
- [ ] Confirm rollback removes the tool and restores the prior implementation.

Checkpoint:

- One agentic loop edits, compiles, activates, discovers and invokes a new
  backend tool without rebuilding the outer service worker.

### Phase 8 — Hardening and developer ergonomics

- [ ] Enforce artifact, source, worker memory, compile-time and execution quotas.
- [ ] Verify/add wasmi fuel and memory limits.
- [ ] Add crash/trap recovery and corrupted-artifact fallback.
- [ ] Add release export/import for backup.
- [ ] Add clear storage usage and compiler payload diagnostics.
- [ ] Test multiple tabs and serialize activation.
- [ ] Add accessibility and keyboard support to the editor/review UI.
- [ ] Document browser support, secure-context and header requirements.
- [ ] Add a release build guard proving compiler assets are absent by default.

## 18. Verification matrix

### Rust/unit

- Dynamic manifest parsing and canonical hashing.
- Artifact hash and BlockInfo agreement.
- Name, route, access and capability rejection.
- Runtime swap ownership and rollback.
- Activation state-machine transitions and recovery.
- Release diff and retained-asset reference counting.
- Typed endpoint schemas.
- Admin routing and feature-off behavior.
- Global WebMCP discovery of an activated fixture.

### Browser/wasm-bindgen

- Nested OPFS operations.
- Runtime Rc retained across an awaited request.
- Service-worker restart reloads the active release.
- Cross-origin isolation is true on /b/dev.
- Dedicated and nested compiler workers start.
- Compiler cancellation and failure recovery.
- Site preview isolation.

### Playwright

- Feature off: /b/dev is absent and no compiler request occurs.
- Feature on, anonymous/non-admin: denied.
- Admin: create project, edit file and handle a stale-hash conflict.
- Compile invalid Rust and show structured diagnostics.
- Compile valid guest, validate and prepare.
- Preview frontend.
- Require human confirmation.
- Activate and request the new route.
- Refresh manifest and invoke the new WebMCP tool.
- Reload/terminate the service worker and verify persistence.
- Roll back UI, route and WebMCP tool set.
- Interrupt activation at each journal phase and verify convergence.

### Performance and size

- Record outer Wasm raw, wasm-opt and gzip size with and without
  browser-devtools.
- Confirm Rubrc assets are lazy and absent from the default distribution.
- Record first compiler download, warm initialization and compile duration.
- Record runtime rebuild/activation duration.
- Set explicit warnings/limits rather than allowing silent growth.

## 19. Rollout

1. Land the OPFS fix independently because it benefits browser storage beyond
   development mode.
2. Land safe Rc runtime ownership independently without enabling replacement.
3. Land precompiled fixture installation behind browser-devtools.
4. Land project/site editing and release activation.
5. Land page-scoped WebMCP tools.
6. Land Rubrc last, behind the same disabled feature.
7. Keep documentation explicit that this is a local development facility.
8. Do not advertise production arbitrary-code execution until resource limits,
   capability review and adversarial tests pass.

Each phase should be reviewable and revertible without leaving the default
browser build dependent on Rubrc.

## 20. Known risks and mitigations

| Risk | Mitigation |
| --- | --- |
| Rubrc changes or is difficult to extract | Narrow adapter, pinned revision, precompiled-fixture path remains useful |
| Compiler payload is very large | Lazy versioned download, cache, progress, no default packaging |
| Rubrc lacks dependencies/proc macros | Generated dependency-free ABI template for MVP |
| Service worker is replaced mid-activation | Persistent desired-release journal and startup convergence |
| UI and backend activation are not one storage transaction | Activation gate, candidate runtime, entrypoint-last publish and rollback |
| OPFS paths cannot currently represent normal trees | Phase 1 recursive path foundation |
| Guest loops or exhausts memory | Hard wasmi fuel/memory limits before untrusted use |
| Agent overwrites concurrent edits | expected_sha256 on every mutation |
| Site prompt injection reaches dev tools | Register tools only on trusted /b/dev; sandbox preview |
| New block requests excessive authority | Deny-by-default capabilities shown in release diff |
| New route shadows core behavior | Reserved namespaces and built-in-first collision validation |
| WebMCP tools become stale after activation | Runtime generation plus reload/abort-and-reregister |
| Built-in block edit expectation | Clear boundary: extend with guests; outer rebuild remains conventional |

## 21. Definition of done

The first agentic browser-development release is complete when:

- browser-devtools is disabled by default and absent from a normal bundle.
- An opted-in build opens a cross-origin-isolated administrator-only /b/dev.
- A WebMCP-capable agent can read and update project files without unrestricted
  filesystem or shell access.
- Rubrc compiles the dependency-free WAFER template in a dedicated worker.
- Compiler diagnostics are structured and useful for an edit/compile loop.
- The resulting guest is hash-verified, capability-checked and executed through
  wasmi.
- Frontend files preview and publish through wafer-run/web.
- A combined release activates without exposing a half-written state.
- The active runtime survives service-worker restart.
- The activated guest's curated WebMCP tool appears and can be invoked.
- Activation and rollback require a human confirmation.
- Rollback restores frontend, backend route behavior and tool discovery.
- The full unit, browser and Playwright verification matrix passes.
- The implementation and limitations are documented for users.
