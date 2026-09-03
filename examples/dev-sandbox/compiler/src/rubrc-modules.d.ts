/**
 * The pinned rubrc modules, and vite's query imports, as `any`.
 *
 * `vite.config.ts` aliases `rubrc-worker` and `rubrc-lib` into `.rubrc/`, a
 * checkout of someone else's repository at the commit `PIN.json` names. Its
 * types are not ours to fix and `.rubrc/` may not even exist in a tree that
 * has not been built, so `npm run typecheck` treats those imports as opaque
 * and checks the code we wrote. What crosses that boundary is annotated by
 * hand in `worker-entry.ts` and `vfs-runner.ts`.
 */
declare module "rubrc-worker/*";
declare module "rubrc-lib/*";
declare module "*?worker";
declare module "*?worker&url";
declare module "*?url";
