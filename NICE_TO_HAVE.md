# Nice-to-Have Improvements

Low-priority improvements identified during code review. None are blocking, but each would improve the project's operational maturity.

## Performance

- **KV caching for project resolution in dispatch worker** — Currently every request queries D1 to resolve the project subdomain. A Cloudflare KV cache with a short TTL (e.g. 60s) would reduce latency and D1 load for hot projects. Requires cache invalidation on project config changes.

## Security

- **Whether a stored Stripe webhook payload carries a capability** — `impresspress__products__stripe_events.payload_base64` holds the raw body of every webhook Stripe posts, base64-encoded (`blocks/products/stripe.rs`). `secret_tables::SECRET_TABLES` deliberately leaves it readable by the admin SQL explorer: it is a third-party payload, and nothing in it authenticates anyone *to this deployment*, which is the rule that module applies. What was not established is whether an embedded Stripe object can carry a capability of Stripe's own — a Checkout Session object has a `client_secret`, and this repo reads exactly that field off a session response (`stripe.rs`), so a payload that embeds a session would embed one too. Settling it means reading Stripe's event payloads rather than this repo, and the answer changes only whether one more table joins the refused set. Its sibling `provider_operations.request_json`/`response_json` was checked and is clear: both columns hold summaries this repo builds itself, never a provider body.

- **Configurable Argon2 params for native deployments** — Current params (4 MiB memory, 2 iterations, 1 lane) are tuned for Cloudflare Workers' constrained environment. Native deployments should use higher cost params (e.g. 64 MiB, 3 iterations) for stronger password hashing. Could be driven by a `ARGON2_MEMORY_COST` env var.

## Testing

- **Code coverage tracking with cargo-tarpaulin** — No coverage metrics are currently tracked. Integrating `cargo-tarpaulin` into CI would identify untested code paths and track coverage trends over time.

- **Multi-browser Playwright matrix** — E2E tests currently run Chrome only. Adding Firefox and Safari (webkit) to the Playwright config would catch browser-specific rendering and API issues.

- **Component-level frontend tests** — Frontend code has no unit tests (only E2E via Playwright). Adding Vitest for Preact component and utility function tests would catch regressions faster and without the overhead of full browser automation.

## Operations

- **Load/performance testing setup** — No load testing exists. A basic k6 or Artillery script targeting auth, storage, and admin endpoints would establish baseline throughput numbers and catch regressions.

- **Bulk "reset all keys pinned at upgrade" on the Variables page** — The one-time env-precedence transition (`platform_state::variables::seed_and_load`) pins precisely the keys whose stored value disagreed with the environment, which on a deployment that has been configured through the admin UI is several keys, not one. Today each is released individually through `POST /b/admin/variables/{key}/reset-to-environment`, so an operator who decides "the environment was right all along" clicks once per key. A bulk control scoped to `Pin::PreUpgrade` only — never to `Pin::AdminEdit`, which is a decision a person actually made and recorded — would make the common answer one click. Deliberately out of scope for the PR that introduced the transition: the per-key route and its page control are the correctness fix, and a bulk action over config keys wants its own confirm-and-preview design rather than being bolted onto it.

## Scalability

- **Release asset key inventory scaling — resolved.** The inventory now lives in `{prefix}/keys.json` in R2 (digest-pinned, fetched once per isolate, fail-closed on a digest mismatch) instead of being inlined into the `IMPRESSPRESS_RELEASE_ASSET_KEYS_JSON` Worker var, so the Worker vars are O(1) regardless of asset count. The two secondary issues that came with the old approach — `manages_folder`'s O(n) scan over the full key set, and an abort message that named an infrastructure variable content authors had never heard of — are gone with it. Parsing and loading live in `impresspress-core/src/release_inventory.rs`, where they're covered by native tests.
