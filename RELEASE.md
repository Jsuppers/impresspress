# Releasing Impresspress

## Version Scheme

Impresspress uses [Semantic Versioning](https://semver.org/): `MAJOR.MINOR.PATCH`

- **MAJOR** — breaking changes to CLI flags, config format, or stored data
- **MINOR** — new features, new blocks, new config options
- **PATCH** — bug fixes, security patches, dependency updates

## Upgrade Notes

Notes for operators upgrading an **existing** deployment. Migrations are gated:
they run on a fresh install, or when the operator opts in with
`impresspress --run-migrations` (native) / `deploy-cloudflare.sh deploy
--run-migrations` (Cloudflare). So whenever a release's code half assumes a
data repair the migration half performs, it has to be called out here — the
two ship together but only one of them runs by default.

### Products: `deleted_at` normalization (migration 020) — upgrade with `--run-migrations`

Product deletion is a soft delete, and `deleted_at` now carries a strict
two-value invariant: SQL NULL for a live product, an RFC3339 stamp for a
deleted one. The empty string is neither, and it now reads as **deleted**
everywhere — the public catalog, the storefront, the admin product list and
the per-seller product cap.

Earlier releases disagreed with themselves about `''`: the customer-facing
paths tested `!is_null && != ""`, so an empty string meant *live*, while the
list reads used `deleted_at IS NULL`. And `''` was reachable — until the
product handlers began refusing bodies that name an internally-owned column,
every create/update path forwarded the request body verbatim, so a client
sending `"deleted_at": ""` produced such a row.

Migration `020_normalize_blank_deleted_at` repairs those rows back to NULL.
**Upgrade with `--run-migrations`.** Without it the code half lands alone and
any affected product drops out of the catalog and the storefront with no admin
action; the only signal is the generic `schema drift; redeploy with
--run-migrations to apply` warning each boot logs for the products block.

### Products API: internally-owned columns are now refused

The four product create/update endpoints (`POST`/`PATCH` under
`/b/products/api/admin/products` and `/b/products/api/products`) now answer
**400** naming any of `id`, `owner_kind`, `owner_id`, `created_by`,
`seller_account_id`, `approval_status`, `stripe_product_id`,
`current_version`, `submitted_at`, `published_at` and `deleted_at` that a
request body carries. Each of those columns has a dedicated writer that
maintains its invariants; none of them is a caller-supplied value on any tier
or verb.

**What each endpoint did before is not the same story, so check yours:**

- **Admin create** (`POST /b/products/api/admin/products`) forwarded the body
  to the database verbatim and applied its own defaults only for keys the
  body omitted. Every one of the eleven fields was **honoured**, `id`
  included — the database layer synthesizes a UUID only when `id` is absent.
  A seeding client that POSTs chosen ids has been getting those ids, and will
  now get a 400 for every such create. Drop `id` from the body and read the
  server-assigned one out of the response.
- **Seller create** (`POST /b/products/api/products`) overwrote `status`,
  `approval_status`, `owner_kind`, `owner_id` and `created_by` with its own
  values after parsing the body — those five were genuinely dropped, silently,
  behind a 200. The other six, `id` among them, were honoured.
- **Both PATCH paths** wrote every key in the body into the `UPDATE … SET`
  list, `id` included. That is the reason `id` is on the list at all: a
  `PATCH` body carrying one rewrote the product's primary key and orphaned
  every `line_items` / `offers` / `product_versions` / `entitlements` row
  pointing at it — and then the by-id re-read looked up an id that no longer
  existed and answered **"Product not found"**, so the caller was told the
  write had failed while the catalog had already been rewritten.

The admin and seller UIs send only caller-owned fields and are unaffected. An
API client that round-trips a whole product record back into a `PATCH` must
now send only the fields it is changing.

### Products: deleting a product is undoable

Deleting a product is now a soft delete: the row stays, with every
`line_items` / `offers` / `product_versions` / `entitlements` reference to it
intact, and only `deleted_at` changes. That is the point of the change — the
hard delete it replaces orphaned a completed order's line items.

Admin → Products has a **Deleted** tab listing those rows most-recently-
deleted first, with **Restore** on each. A deleted product is not editable
until it is restored.

Sellers get the same thing for their own products: **My Products** has the
same **Deleted** tab, showing only the caller's own deleted products, with
**Restore** (`POST /b/products/api/products/{id}/restore`) and **Close Stripe
surface** on each row. Both are scoped to the caller — another seller's
deleted product answers 404 on every path.

**Closing a deleted product's Stripe surface.** Soft delete touches nothing in
Stripe: a deleted product's Prices and Payment Links stay live in the connected
account and keep taking money, and deleting the product archives none of them.
Each row in a Deleted tab therefore also carries **Close Stripe surface**,
which opens a close-only manager for that product: archive its offers,
deactivate its payment links, nothing else. Use it *before* Restore if the
reason for the delete was that the product should stop selling — Restore puts
an active, approved product back into the public catalog immediately.

**Known gaps.**

- The close-only manager acts one offer and one link at a time. There is no
  "close everything" action, and nothing blocks Restore while a money surface
  is still open.
- A suspended seller cannot restore a deleted product, nor archive its offers
  or deactivate its payment links — those are all mutations a platform
  suspension stops (suspension already archives the seller's Stripe catalog).
  An administrator can do any of them on their behalf.

### Products: restoring a deleted product whose slug was taken

020 deliberately skips a row whose slug a live product of the same owner
already holds. Repairing it would violate migration 005's partial unique slug
index and abort the migration, which is unrecoverable in place: the hash never
gets stamped, so every later boot retries and re-fails, and on Cloudflare that
is a 500 on every request. A skipped row keeps its current half-state and stays
listed in the *deleted products* view (admin, or the owning seller's My
Products), where **Restore** is the remedy —
it reports the slug conflict in plain language instead of failing opaquely. To
find them:

```sql
SELECT id, owner_kind, owner_id, slug
FROM impresspress__products__products WHERE deleted_at = '';
```

Rename whichever product should not hold the slug, then restore. Re-running 020
is *not* the remedy: once applied, its hash is stamped and the migration
short-circuits for good.

### Branding: the built-in raster wordmark is gone — no action required

The bundled brand art is now a true pixel-art mark, and the long-form raster
wordmark (`impresspress-logo-long.png`) has been deleted along with its
`/b/static/impresspress-logo-long-{hash}.png` route. Brand text is text now:
`WAFER_RUN_SHARED__LOGO_URL` defaults to blank, and every surface that used to
show the wordmark — the sidebar, the auth cards and the userportal account
card — renders the square mark next to the app name instead.

**Why this needs a note.** Older releases declared that route's URL as
`LOGO_URL`'s *default*, and `seed_defaults` writes a declared default into the
`variables` table the first time it sees a key with no row. So an existing
deployment does not fall back to the new blank default: it holds a stored
`/b/static/impresspress-logo-long-{hash}.png`, pointing at a route this release
no longer serves. Left alone that is a silently broken image on every page.

**It repairs itself.** `seed_defaults` clears any `LOGO_URL` row still holding
that route back to blank, on the first boot after the upgrade, and logs a
warning naming the value it cleared. This deliberately does *not* ship as a
migration: migrations are gated on `--run-migrations` (see the top of this
section) and a broken logo gives an operator nothing to opt in *from*, whereas
`seed_defaults` runs on every boot's `Init` on all three targets. The match is
scoped to that one built-in route, so a white-labelled `LOGO_URL` of your own
is never touched.

**If you want a wordmark back,** set `WAFER_RUN_SHARED__LOGO_URL` to your own
image in Admin → Settings → Variables. It renders exactly as before.

**SDK (`@impresspress/js`):** `IMPRESSPRESS_ASSETS.logoLong` and
`static/logo_long.png` are removed — a breaking change for any consumer that
referenced them. `IMPRESSPRESS_ASSETS.logo` (the square mark) and
`favicon.ico` are unchanged in name and now carry the new art.

## Pre-Release Checklist

Before tagging a release, verify:

- [ ] `main` branch CI is green (check the [Actions tab](../../actions))
- [ ] Cross-platform builds pass (the `CI Main` workflow runs on every push to `main`)
- [ ] Update `version` in `Cargo.toml` workspace section to match the intended release
- [ ] No known critical bugs (check [open issues](../../issues))
- [ ] Test the binary locally:
  ```bash
  cargo build -p impresspress --release
  ./target/release/impresspress
  ```
- [ ] If this release changes config variables or CLI flags, update the docs
- [ ] If this release ships a migration that repairs existing data, add an entry
      to [Upgrade Notes](#upgrade-notes) so operators know to pass
      `--run-migrations`

## Creating a Release

```bash
# 1. Make sure you're on main and up to date
git checkout main
git pull

# 2. Tag the release
git tag v0.2.0

# 3. Push the tag — this triggers the release workflow
git push origin v0.2.0
```

The [Release workflow](../../actions/workflows/release.yml) will automatically:
1. Build binaries for all 5 platforms (Linux amd64/arm64, macOS amd64/arm64, Windows amd64)
2. Create a GitHub Release with auto-generated notes from merged PRs

## After Release

- [ ] Verify the [GitHub Release](../../releases) was created with all 5 platform artifacts
- [ ] Download and smoke-test at least one binary
- [ ] Announce in relevant channels if this is a notable release

## Hotfix Process

Branch protection prevents pushing directly to `main` — hotfixes follow the same PR flow:

```bash
# 1. Create a hotfix branch
git checkout main && git pull
git checkout -b hotfix/v0.2.1

# 2. Fix the bug, commit, push
git push -u origin hotfix/v0.2.1

# 3. Open a PR — CI must pass, 1 approval required
gh pr create --title "fix: critical bug description"

# 4. After merge, tag the patch release
git checkout main && git pull
git tag v0.2.1
git push origin v0.2.1
```

## Undoing a Release

If a release was tagged by mistake or contains a critical issue:

```bash
# Delete the tag locally and remotely
git tag -d v0.2.0
git push origin --delete v0.2.0
```

Then delete the GitHub Release from the [Releases page](../../releases). Note: users who already downloaded the binary still have it.
