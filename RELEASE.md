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
--run-migrations to apply` warning each boot logs for the products block. There
is no in-UI way to bring such a product back — see *deleting a product has no
undo in the UI yet* below for the statement that does it.

### Products API: internally-owned columns are now refused, not dropped

The four product create/update endpoints (`POST`/`PATCH` under
`/b/products/api/admin/products` and `/b/products/api/products`) used to drop
`id`, `owner_kind`, `owner_id`, `created_by`, `seller_account_id`,
`approval_status`, `stripe_product_id`, `current_version`, `submitted_at`,
`published_at` and `deleted_at` from a request body and answer 200. They now
answer **400** naming the offending fields, so a client is told which part of
its write was not accepted instead of receiving a success for a write that
was half discarded.

`id` is new to that list, and its omission was the reason for the change: a
`PATCH` body carrying one rewrote the product's primary key and orphaned
every `line_items` / `offers` / `product_versions` / `entitlements` row
pointing at it, while answering "Product not found".

The admin and seller UIs send only caller-owned fields and are unaffected. An
API client that round-trips a whole product record back into a `PATCH` must
now send only the fields it is changing.

### Products: deleting a product has no undo in the UI yet

Deleting a product is now a soft delete: the row stays, with every
`line_items` / `offers` / `product_versions` / `entitlements` reference to it
intact, and only `deleted_at` changes. That is the point of the change — the
hard delete it replaces orphaned a completed order's line items.

There is not yet any UI that reaches a soft-deleted product. The admin
*Deleted* view and its **Restore** button are a follow-up. Until they land,
undoing a delete is an operator statement against the database (admin →
Database → SQL, or the D1 / Postgres console):

```sql
-- find it
SELECT id, name, owner_kind, owner_id, slug, deleted_at
FROM impresspress__products__products
WHERE deleted_at IS NOT NULL;

-- bring it back
UPDATE impresspress__products__products
SET deleted_at = NULL, updated_at = '2026-01-01T00:00:00Z'   -- use the current time
WHERE id = '<product id>';
```

Two things to know before running that `UPDATE`:

- Soft delete **frees the product's slug** — migration 005's unique index is
  partial on `deleted_at IS NULL` — so if another product of the same
  `(owner_kind, owner_id)` has claimed the slug since, the `UPDATE` violates
  that index. Rename whichever product should not hold the slug first.
- It is an **administrator** operation. A seller who deletes their own
  product cannot undo it themselves and has to ask an administrator until the
  restore endpoint ships.

The same statement is the remedy for a row migration 020 deliberately skipped.
020 leaves alone any `deleted_at = ''` row whose slug a live product of the
same owner already holds, because repairing it would violate that same index
and abort the migration — which is unrecoverable in place: the hash never gets
stamped, so every later boot retries and re-fails, and on Cloudflare that is a
500 on every request. Find the skipped rows with:

```sql
SELECT id, owner_kind, owner_id, slug
FROM impresspress__products__products WHERE deleted_at = '';
```

Rename whichever product should not hold the slug, then clear `deleted_at` as
above. Re-running 020 is *not* the remedy: once applied, its hash is stamped
and the migration short-circuits for good.

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
