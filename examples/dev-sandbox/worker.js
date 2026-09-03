// Pass-through so headers can be added later without leaving static assets.
// The 25 MiB static-asset per-file cap is respected by Plan 3's compiler packaging.
export default { fetch: (req, env) => env.ASSETS.fetch(req) };
