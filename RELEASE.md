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

### Config: your `.env` applies again, and one boot decides the ties

**What changes.** A `WAFER_RUN_SHARED__*` / `{ORG}__{BLOCK}__*` environment
variable used to be silently ignored from the second boot onward. It was seeded
with `INSERT OR IGNORE`, so it only ever landed on a virgin database; afterwards
a row existed, the insert was discarded, and nothing said so. From this release
the environment sets a key on every boot — **unless an admin has edited that key
through the admin UI**, in which case the stored row wins permanently and the
boot log says which key and why.

**The one-time upgrade boot.** Rows written before this release carry no record
of who wrote them, so an admin's settings-form edit and an earlier boot's env
seed look identical. On the first boot after upgrading, any key whose stored
value **differs** from a value you export is **kept as it is**, pinned, and
named in a WARN. Nothing is reverted, and no export is lost — it simply does not
apply until you say so. A key whose stored value already matches its export is
left alone silently.

**What to do.** Read the boot log. For each `NO EFFECT` line, decide which value
you want:

- *the environment's* — open **Admin → Settings → Variables**, find the key, and
  use **Reset to environment** (or `POST
  /b/admin/api/settings/{key}/reset-to-environment`), then restart. The export
  applies from then on, with no further intervention.
- *the stored one* — do nothing. The line stops once you remove the export.

**If the answer is "the environment, for all of them".** The upgrade boot pins
exactly the keys you had configured, so several keys is the ordinary case rather
than the rare one. **Reset all keys pinned at upgrade**, at the top of Admin →
Settings → Variables, releases every one of them in a single confirm, then one
restart. It is scoped to the keys *that boot* pinned and cannot touch a key an
admin edited in the UI — those stay, whether the edit was before the upgrade or
after it. The button only appears while at least one such key is pinned, and only
on a deployment that boots from a process environment, so on Cloudflare and in
the browser it is never shown.

After that boot the rule is simply: the environment sets a key until an admin
edits it in the UI.

**What this does not protect.** The upgrade boot can only resolve conflicts it
can actually see. This applies to changes made on a **block settings page**
(Products, Legal pages, User portal, Email, Auth) — a change made on **Admin →
Variables** records who made it and is protected outright, whatever your
deployment config says. Such a settings-page change is kept **only if, on that
boot, your deployment config exported that same key with a non-empty value that
differed from the stored one.** If any of those is not
true — the key is not in your config, or it is set to an empty value, or it is
set to the value already stored — the boot passes over it silently and the key
is ordinary from then on. **A later change to your deployment config then wins,
including over that pre-upgrade UI change.**

Concretely: you disabled OAuth in the UI, your compose file said nothing about
it at upgrade time, and months later you add
`WAFER_RUN_SHARED__ENABLE_OAUTH=true`. OAuth comes back on. Same for
`WAFER_RUN_SHARED__ALLOW_SIGNUP`.

The remedy is one action, and it is worth doing now rather than later:
**re-apply in the admin UI any setting you care about that you changed there
before upgrading.** That records it for good — an admin edit made *after* the
upgrade is always safe, whatever your deployment config says.

**Keys worth checking first**, because they decide who can get in:

- `WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_EMAIL` — **every** signup with this
  address is granted admin, not just the first one (`auth::initial_role_for`),
  so a stale value here is a standing back door. Clear it once you have your
  admin account.
- `WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_PASSWORD` and
  `..._BOOTSTRAP_ADMIN_TOKEN` — plaintext credentials; a cleared one now stays
  cleared across a restart even with the export still present.
- `WAFER_RUN_SHARED__ALLOW_SIGNUP` and `WAFER_RUN_SHARED__ENABLE_OAUTH` — if you
  turned either off in the UI during an incident, it stays off.
- `WAFER_RUN_SHARED__ENVIRONMENT` — if this deployment first booted as
  `development` and your deployment config later said `production`, the stored
  value is the **less secure** one (session cookies without `Secure`, and a
  wildcard `Access-Control-Allow-Origin` on discovery documents). Reset this key
  to the environment before anything else.
- Block-scoped credentials such as `IMPRESSPRESS__PRODUCTS__STRIPE_SECRET_KEY` —
  if you rotated one in the UI and your deployment config still carries the old
  one, the rotated value is what is kept. Neither value is printed in the log;
  compare the stored one where you issued it.

**Break glass — if a pin locks you out.** Every route above needs a working admin
login, and the keys most able to deny you one are pinnable. The case to know
about: a deployment with no admin user yet, whose stored
`WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_EMAIL` / `..._PASSWORD` are wrong, and
whose corrected values are in the deployment config. The upgrade boot keeps the
stored pair, and `auth::bootstrap` creates the first admin from **those** — so
signing in to fix it needs the credentials you were replacing.

Release a key without logging in by writing the released marker directly in the
database (`impresspress__admin__variables`), then restarting:

```sql
UPDATE impresspress__admin__variables
   SET updated_by = 'released-to-environment'
 WHERE key = 'WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_EMAIL';
```

`updated_by` is the whole mechanism, and `released-to-environment` is exactly
what the **Reset to environment** button writes. **Do not blank the column
instead.** An empty `updated_by` means "nothing has ever claimed this row",
which is the state the one-time upgrade pass looks for — so on a deployment that
has not recorded that pass yet, blanking the column can get the key pinned
straight back on the next boot, with no UI to tell you. The sentinel above reads
as "the environment owns this" and is correct either way.

Only native deployments can be in this position — Cloudflare and the browser
never seed from a process environment, so nothing there is ever pinned against
one.

**No migration.** Nothing to opt into, and the transition runs once per database
whether or not you pass `--run-migrations`. Cloudflare and browser deployments
are unaffected: neither seeds from a process environment, so neither has a tie
to break, and the **Reset to environment** control does not render there.

### Routing: the `/api` prefix is gone (Cloudflare deployments only)

**What changes.** A Cloudflare deployment used to accept an `/api`-prefixed
copy of every route: the Worker adapter stripped the prefix before dispatch, so
`POST /api/b/storage/api/buckets/photos/objects` reached the same handler as
`POST /b/storage/api/buckets/photos/objects`. That stripping is removed, and
`/api/...` now falls through to the SPA like any other unclaimed path.

**Why.** The prefix only ever worked on Cloudflare. The site-main flow's router
matches the path as received — `/`, `/b/**`, `/health`, `/openapi.json`,
`/.well-known/agent.json`, everything else to `wafer-run/web` — so on the
native and browser transports an `/api/...` request was served the SPA, and the
pipeline's own `/api` strip (which ran after routing) could never see one.
Neither strip was segment-bounded either: `/apiary/hives` became `ary/hives` on
every transport, and `/api/api/x` reached `/x` on Cloudflare and `/api/x`
elsewhere. One transport honouring a prefix the others do not is a routing
difference between deployments of the same app, which is worse than not having
the prefix.

**Who is affected.** Only a Cloudflare deployment with a client that calls
`/api/...` by hand. Nothing in this repository or in the TypeScript SDK does —
the SDK's README states there is no `/api/*` surface, and every block route
already carries its own `/b/<block>/api/...` path, which is untouched.

**What to do.** Drop the `/api` prefix from any such caller: `/api/b/x` →
`/b/x`. There is no migration and no config toggle.

Routing alone does not bring it back, either: adding `{ "path": "/api/**",
"block": "impresspress/router" }` to the flow's routes hands the router a
`req.resource` of `/api/b/x`, and `routing::route_to_block` matches prefixes
like `/b/storage/` against that string, so every such request answers 404. A
consumer who genuinely needs the prefix has to strip it before the router sees
it — a flow step of their own ahead of `wafer-run/router` that rewrites
`req.resource` — which is the piece this release removes.

### Files: bucket names are unique (migration 002) — upgrade with `--run-migrations`

**What changes.** A storage bucket's name is also its folder name in the object
store, and the `buckets` table had no unique index on it. A second user could
therefore create a bucket under a name someone else already held — every
backend's `create_folder` is idempotent, so nothing refused it — and the row
they got granted them read, overwrite and delete access to the first owner's
objects. Bucket names are now unique, and creating one that is taken answers
`409` with "A bucket named … already exists."

**The repair.** Migration `002_bucket_name_unique` deletes duplicate bucket rows
before creating the index, keeping the **earliest** row for each name (by
`created_at`, `id` as the tie-break). That is the access the later rows should
never have had; the folder and its objects stay with the one remaining owner,
and the object-metadata rows of whoever else uploaded into it are left alone —
those blobs are real and still charged to whoever uploaded them.

**Review your collisions before upgrading.** That last sentence has a
user-visible edge: an object the losing user uploaded stays in the winner's
bucket, so they lose access to their own file while their quota keeps being
charged for its bytes. In the case this fixes — a takeover — that is the
correct outcome. If a collision turns out to be two people who each meant to
have their own bucket, sort it out *before* you run the migration: have the
later user download what they need, or rename their bucket (create a new one
and re-upload), because afterwards only the winner can reach the folder.

**Upgrade with `--run-migrations`.** Without it the index is not created, and
the code half alone does not close the hole: the refusal comes from the
database, so a duplicate name is admitted exactly as before. The only signal is
the generic `schema drift; redeploy with --run-migrations to apply` warning each
boot logs for the files block.

**To see what will be deleted**, list the collisions from the admin SQL
explorer before upgrading. This runs on every backend — the per-owner rows come
back one per line rather than through a backend-specific aggregate
(SQLite/D1 has `GROUP_CONCAT`, Postgres has `string_agg`, and neither has the
other):

```sql
SELECT b.name, b.created_by, b.created_at, b.id
FROM impresspress__files__buckets AS b
WHERE EXISTS (
    SELECT 1 FROM impresspress__files__buckets AS other
    WHERE other.name = b.name AND other.id <> b.id
)
ORDER BY b.name, b.created_at, b.id;
```

The first row of each `name` group is the one that survives; the rest are what
the migration deletes.

### Files: uploaded objects download instead of rendering

**What changes.** `GET /b/storage/api/buckets/{bucket}/objects/{key}` and the
public share link `GET /b/storage/direct/{token}` serve bytes and a content type
an uploader chose, from the application's own origin. They now send
`X-Content-Type-Options: nosniff` on every object, and `Content-Disposition:
inline` only for types that cannot carry script — images (not SVG), audio,
video, PDF and plain text. Everything else, `text/html` and `image/svg+xml`
included, is served as an `attachment` with a sandbox `Content-Security-Policy`.

Image and PDF previews are unaffected. What changes for a user is that opening
an uploaded `.html` or `.svg` link downloads the file rather than displaying it.
**No migration.**

### Files: legacy share links (migration 003) — upgrade with `--run-migrations`

A public share link's token used to be a JWT, and the link handler verified
it before it read the share row — so a link stopped working when its JWT
aged out, whatever expiry its owner had picked. From this release a token
is opaque entropy addressing one row, and the row's `expires_at` is the
only thing that ends a link.

That matters on upgrade because the share dialog used to post its expiry
under a field name the server did not read, so almost every share created
through the UI has **no expiry on the row at all**. Reading those tokens as
opaque strings would make every one of those links live again —
permanently, and pointing at files whose owners believe the link is long
gone.

**The JWT's life changed once, so the repair has two arms.** Until
2026-05-14 the share JWT was signed for 365 days; SEC-055 shortened it to
30. Migration `003_legacy_share_token_expiry` gives a legacy row (a
JWT-shaped token, no expiry) 365 days from when it was minted if it
predates that change and 30 days if it does not — reproducing the lifetime
its token actually imposed. A link that is dead today stays dead; a link
minted in the one-year era and still working keeps working to its original
date.

The cutoff is the instant the code changed, not the instant your deployment
adopted it. If you upgraded past 2026-05-14 some time later, shares minted
in that gap really ran the one-year code and will be given 30 days here —
i.e. they expire. That arm errs toward "a dead link stays dead"; re-share
the file if one of them mattered.

**Upgrade with `--run-migrations`.** The code half refuses to serve a share
row that records no end, so without the migration every historical share
link answers "Share link is unavailable" — an outage for every link your
users have already sent, not a leak. The migration is what gives each of
them its correct remaining life back. Until it runs, the only other signal
is the generic `schema drift; redeploy with --run-migrations to apply`
warning each boot logs for the files block.

### Files: every share link now expires

**What changes.** A public share link is an unauthenticated bearer
credential: it is pasted into a chat or a document and never looked at
again. From this release every one of them has an end. The share dialog no
longer offers "Never", and a `POST /b/cloudstorage/shares` that names no
`expires_in_hours` gets the configured maximum rather than an unexpiring
link. A share row that somehow carries no expiry — an import, a restore, a
hand-written row — is refused by the public link rather than served.

**The maximum is yours to set.** `IMPRESSPRESS__FILES__MAX_SHARE_EXPIRY_HOURS`
(Admin → Settings → Variables) defaults to `8760` — one year, the ceiling
explicitly-supplied expiries were already held to. It is read per request,
so raising it for a deployment that genuinely needs long-lived public links
takes effect without a redeploy, and lowering it binds the next share
immediately. Links already issued keep the expiry they were given. An
expiry longer than the maximum is refused with a 400, as before.

**Who is affected.** Anyone who picked "Never" in the share dialog: those
links now get the configured maximum instead. Existing links are unchanged
by this — what bounds them is the repair above, which reproduces the
lifetime their token already had.

### Files: the file-count quota is per bucket, both caps are exact, and `reset_period_days` is gone

**The file-count cap is per bucket, as its name says.** `max_files_per_bucket`
(default `10000`, shown as "Max Files/Bucket" on the storage admin's Quotas tab)
used to be checked against a user's files summed over **all** their buckets,
so filling one bucket blocked uploads everywhere. It is now checked against
the files that user holds in the bucket being uploaded to. This **loosens**
enforcement: a user with several buckets can now store up to
`max_files_per_bucket` files in each. Nothing caps how many files a user holds
across buckets: `max_storage_bytes` is still over everything a user stores,
but it counts bytes, not files, so it bounds that total only by size. No
migration is involved.

**Both caps are exact.** An upload is held to `max_storage_bytes` and
`max_files_per_bucket` by the write that reserves it, as one atomic step, so
uploads running at the same time can no longer each pass against the same
usage and together exceed a cap. A refused upload answers as before: a 400
with `Storage quota exceeded` or `File count limit reached for this bucket
(max N)`. One case still leaves a user over `max_storage_bytes`: an admin's
upload replaces one of their objects, they use the room that frees, and the
admin's upload then fails — their object is put back, and their next upload
is refused until they are under the cap again.

**`reset_period_days` is removed — a breaking change for API clients.** It
was stored and published, but nothing ever enforced a reset period. It is no
longer in `GET /b/cloudstorage/quota`, `GET /b/cloudstorage/admin/quotas` or
the `PATCH /b/cloudstorage/admin/quotas/{id}` response, and a PATCH that names
it is refused with a 400 (`Unknown quota field`) instead of being stored. The
SDK's `getQuota()` type no longer has it. Drop the field from anything that
sends or reads it. The database column is left in place, unused; there is
nothing to do about it.

### Files: an upload's claim on a key is exact (migration 004) — upgrade with `--run-migrations`

**What changes.** An upload claims its `(bucket, key)` row before it stores
the bytes, and a replacement takes over the existing row. That take-over was
conditional on the row's `updated_at` timestamp, so two uploads of one key
whose stamps landed in the same millisecond (or on two Workers isolates whose
clocks disagreed) could both take the row: one row describing one upload while
the blob held the other. Migration `004_object_claim_id` adds a nullable
`claim_id` column to `impresspress__files__objects`; each reservation writes a
random token there, and the take-over, the completion and the rollback of a
failed upload each act only on the reservation the row still carries.
While a rollout is part-way, isolates still running the previous release take
rows over on the old `updated_at` check and leave `claim_id` unchanged, so the
token check cannot catch those take-overs; the gap is transient and closes once
every isolate runs this release.

**When the object is deleted mid-upload.** An upload whose object or bucket is
deleted while its bytes are being stored now answers `409` saying so (not that
another upload took the key), and the bytes it stored are deleted rather than
left recorded and charged nowhere.

**A retry after "Upload stored but could not be recorded" says what holds the
key.** Such an upload leaves its reservation in place for up to an hour, and
the user's retry used to be told "Another upload of this key is in progress".
It now answers `409` with "Your earlier upload of this key, started at …, has
not been recorded", saying when the key is released. The key is still held
for that hour: the row cannot tell a failed upload from one still running.

**Re-running the files migrations is safe for live data.** Adding 004 changes
the hash of the files block's migration set, so the next `--run-migrations`
boot (on Cloudflare, the first deploy of this release) re-runs **all** of them, 001 onwards, over
the existing tables — the way a new auth migration re-runs the auth set. For
files nothing is dropped and nothing live changes: 001 is `CREATE … IF NOT
EXISTS` throughout, 002's duplicate-bucket `DELETE` finds nothing once its
unique index exists, 003 only touches legacy share rows with no expiry, which
its first run already dated, and 004 is an `ADD COLUMN` that PostgreSQL skips
(`IF NOT EXISTS`) and SQLite/D1 answers with a duplicate-column error the
runner treats as done. Share links, buckets, stored objects and uploads in
flight come through byte-identical — pinned on SQLite by `replay_tests` and on
PostgreSQL by the CI step that replays the set over seeded rows. Existing rows
keep a `NULL` `claim_id`, which a take-over matches as it matches a token;
nothing is backfilled, and nobody is signed out.

**Without the migration.** Cloudflare deploys always run it. A native
deployment that skips `--run-migrations` logs the generic `schema drift;
redeploy with --run-migrations to apply` warning for the files block on each
boot, and uploads keep working because strict schema is off by default there
and the column is added on the first upload. If you have turned
`WAFER_RUN__DATABASE__STRICT_SCHEMA` **on**, the migration is not optional:
every upload fails on the missing column until it runs.

### Files: each upload stores its bytes under a key of its own (migration 005) — upgrade with `--run-migrations`

**What was wrong.** Every upload wrote its bytes at the object key, so every
upload of one key wrote the same blob. The claim token (migration 004) kept the
metadata row exact, but not the bytes: an upload whose reservation outlived its
hour and was taken over could still store its bytes after the upload that took
over had finished — and overwrite them. That upload's row, complete and
describing its own upload, then served the first upload's content to everyone
who can read the file.

**What changes.** Each upload stores its bytes under a storage key derived from
the object key and its reservation (`reports/{claim}~q3.pdf` for
`reports/q3.pdf`), and migration `005_object_blob_key` adds a nullable
`blob_key` column to `impresspress__files__objects` naming the blob the row
serves. Downloads, share links and `GET /b/storage/api/buckets/{name}/objects`
resolve an object's bytes through its row. A replacement deletes the blob it
supersedes only after the row points at the new one; an upload that lost its
key, or whose object was deleted, deletes only its own blob. The hourly-TTL
sweep of abandoned uploads now deletes their blobs as well as their rows.

**Existing objects.** Nothing is copied or rewritten. Rows from before 005 keep
a `NULL` `blob_key`, which means their bytes are at the object key — where they
are — and they are served from there until a replacement supersedes them.

**Visible differences.**
- The object listing reads the metadata rows instead of listing storage. It
  names the same object keys, now includes uploads still in flight (as the
  object browser page already did), and its `prefix` filter follows the
  database's `LIKE`: case-insensitive for ASCII on SQLite and D1,
  case-sensitive on PostgreSQL.
- An upload that loses its key to another upload answers `409` "Another upload
  now holds this key, so this upload was not recorded; retry" (it used to say
  it "took too long", which was false when the object had been deleted and the
  key claimed again).
- A blob in storage that no row names is no longer downloadable by its key.
  Such blobs were already charged to nobody and absent from the object browser.

**Re-running the files migrations is safe for live data.** Adding 005 changes
the hash of the files block's migration set, so the next `--run-migrations`
boot (on Cloudflare, the first deploy of this release) re-runs **all** of them,
001 onwards, over the existing tables. As for 004, nothing is dropped and no
live row changes: 005 is an `ADD COLUMN` that PostgreSQL skips (`IF NOT
EXISTS`) and SQLite/D1 answer with a duplicate-column error the runner treats
as done. Pinned on SQLite by `replay_tests` and on PostgreSQL by the CI step
that replays the set over seeded rows. Nobody is signed out; files migrations
touch no auth table.

**Without the migration, and during a rollout.** As for 004: a native
deployment with strict schema off adds the column on the first upload, one with
`WAFER_RUN__DATABASE__STRICT_SCHEMA` on fails every upload until 005 runs, and
Cloudflare deploys always run it. While a rollout is part-way, an isolate still
on the previous release writes a replacement's bytes at the object key and
leaves `blob_key` as it was, so that row keeps serving the blob it named before
rather than the replacement, until the next upload of the key. The bytes that
isolate wrote at the object key are deleted when the object is next replaced,
deleted or swept; the gap closes once every isolate runs this release.

**Blobs that are logged, not reclaimed.** An upload whose bytes were stored but
whose row could not be recorded, and whose reservation another upload has since
taken over, leaves a blob no row names; the sweep cannot find it without
listing storage. It is logged at error level ("upload stored but not recorded")
with its blob key, as is any blob whose delete fails after its row is gone.

### Products: `PLATFORM_COUNTRY` no longer defaults to `US` — set it if you ship

**What changes.** `IMPRESSPRESS__PRODUCTS__PLATFORM_COUNTRY` now has one
default, and it is the empty one its setting has always declared. Checkout and
Payment Links used to default it to `US`, and to fall back to `US` for a value
they could not read, while seller onboarding defaulted it to empty and refused
an unreadable value. There is now one reader, and blank means "not configured"
everywhere.

**Who is affected.** Only a deployment that (a) has never set
`IMPRESSPRESS__PRODUCTS__PLATFORM_COUNTRY`, and (b) sells an offer whose
"collect shipping address" is on and whose "allowed shipping countries" list is
empty. Until now that combination silently produced a Checkout that would
accept a United States address and nothing else — including for a platform
that is not in the United States. It now refuses the checkout with
`this offer collects a shipping address but names no allowed countries`, and
the refusal is recorded against the order so it shows up in the seller
dashboard's recent failures.

Offers that list their own allowed shipping countries are unaffected; that
list always won and still does. Offers that do not collect a shipping address
are unaffected. Seller onboarding is unaffected: it already treated blank as
"no country" and let Stripe infer it.

**What to set.** In Admin → Settings → Products, set **Platform Country** to
your platform's two-letter country code (for example `NZ`), or list the
countries you ship to on each offer. A value that is not two ASCII letters is
now an error rather than a silent `US`, so fix any typo there at the same time.

Related, and not behaviour-changing for a valid configuration:
`IMPRESSPRESS__PRODUCTS__SELLER_APPLICATION_FEE_BPS` is now refused rather than
read as `0` when it is not a whole number of basis points between 0 and 10000.
A deployment whose fee is currently unreadable has been taking **no** platform
fee on connected-account sales; after this release those sales refuse until the
value is corrected.

### Products: every seller pays the current platform fee — check your sellers before upgrading

**What changes.** A seller used to keep the platform application fee
(`IMPRESSPRESS__PRODUCTS__SELLER_APPLICATION_FEE_BPS`) that was in force the
day they started Stripe onboarding. It was stored on their seller account and
nothing ever changed it. A seller onboarded while the fee was `0` was the one
exception: they were charged whatever the platform fee was at the time of each
sale, while their seller pages showed `0.00%`. From this release there is one
fee. Every seller's **new** Checkout Sessions and **newly created** Payment
Links carry the current platform fee, and every seller page, the seller API and
the admin seller pages show that same number. Per-seller rates are not
supported.

**Who is affected.** Sellers onboarded at a non-zero fee that differs from
today's platform fee. They were charged their onboarding rate. After the
upgrade they are charged the current rate. Sellers onboarded at `0`, or at
exactly today's fee, are charged what they were charged before; only the fee
their pages show changes.

**What does not change.** Anything Stripe already holds keeps the fee it was
created with. An existing Payment Link is reused as it is, because its fee is
not part of what identifies it. An existing subscription renews with the
`application_fee_percent` it was created with. Orders already placed are not
touched.

**Find the affected sellers before upgrading.** The stored per-seller fee is
still in the table (it is no longer read), so this read-only query works from
the admin SQL explorer on SQLite, Cloudflare D1 and PostgreSQL alike. Replace
`500` with your current `SELLER_APPLICATION_FEE_BPS`:

```sql
SELECT id, user_id, status, fee_basis_points
FROM impresspress__products__seller_accounts
WHERE fee_basis_points <> 0
  AND fee_basis_points <> 500
ORDER BY fee_basis_points, user_id;
```

Every row returned is a seller whose new checkouts and new Payment Links will
charge a different fee after the upgrade. `fee_basis_points` is the rate they
pay today.

**If you need to keep an old rate.** Set
`IMPRESSPRESS__PRODUCTS__SELLER_APPLICATION_FEE_BPS` to that rate before
upgrading. It then applies to every seller, since there is no per-seller
override. No migration is involved and no flag is needed.

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

### Email: `WAFER_RUN_SHARED__SITE_URL` is gone, and mail is sent under your App Name

**`WAFER_RUN_SHARED__SITE_URL` is no longer a setting.** Its only reader was a
`welcome` email template that nothing ever sent, and it defaulted to the
project's own marketing domain. Both are removed, along with the equally unsent
`payment_failed` template: `email.send_template` now knows `verification` and
`password_reset` only, and any other name is a 400 as an unknown template.

**What to do.** Nothing is required. A deployment that booted an earlier release
still holds a `WAFER_RUN_SHARED__SITE_URL` row in its variables table. It is
harmless — nothing reads it — and it is listed on Admin → Settings → Variables
like any other key no block declares, where you can delete it. Setting
`WAFER_RUN_SHARED__SITE_URL` anywhere now has no effect: native boot no longer
copies it from the environment into the variables table, and no code reads it
on any target.

**The default sender's display name is your App Name.** With
`IMPRESSPRESS__EMAIL__MAILGUN_FROM` unset, mail used to go out as
`Impresspress <noreply@{your Mailgun domain}>` whatever the deployment was
called. It now carries `WAFER_RUN_SHARED__APP_NAME` — quoted, or RFC 2047
encoded when it is not plain ASCII — so recipients see the name you configured.
A `MAILGUN_FROM` you set yourself is sent unchanged, as before.

### Auth: OAuth sign-in now needs a *proven* address, and existing accounts have none

**What changes.** An OAuth identity may only join an existing local account when
both sides have proven the address: the provider asserts it is verified, and the
local row records who proved it. Before this release the callback matched on the
address alone, so anyone who could register `victim@example.com` with a password
— on a default install that is anyone, since `WAFER_RUN__AUTH__REQUIRE_VERIFICATION`
is off and the signup mails nothing — owned the account the victim's Google
sign-in landed in.

`users.email_verified` could not carry that decision. Signup writes it
`!REQUIRE_VERIFICATION`, so with verification off it means "this deployment does
not ask", not "somebody proved it". Migration 013 adds `email_verified_by`, which
names the act: `email_token` for a redeemed verification link, `oauth.<provider>`
for a provider that asserts verification.

**The upgrade consequence.** `email_verified_by` is **not backfilled**, and it
cannot be: a row that predates it may have been verified by a real mailed link or
may be a default-on-signup row, and backfilling would restore the takeover for
every squatted address. So on the first release that has it, **no existing
account can be linked to an OAuth provider** until its address is proven again.
Those users sign in with their passwords exactly as before, and the admin user
list still shows them as verified — that column is the policy flag and has not
changed meaning.

**What a user does about it.** Either, without an operator:

- **Ask for a verification link** — `POST /b/auth/api/resend-verification`, or
  the "resend" link on the verify page — and open it. Both the resend and the
  redemption key on `email_verified_by`, so an account the flag already calls
  verified is still offered a link and still records the proof when it redeems
  one.
- **Reset the password.** A redeemed reset link is mailbox proof of the same
  strength, so `POST /b/auth/api/reset-password` records it too.

Either one makes the account adoptable, permanently. There is nothing for an
operator to run, and no database edit is expected of anybody.

**A related nuisance, not a vulnerability.** A provider that asserts nothing
about the address it returns — Microsoft, whose `email` claim is a mutable tenant
attribute — can still create a local account holding *any* address, including the
one in `WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_EMAIL`. That account is created
unproven, so it is granted no admin role and cannot be adopted by anyone; what it
does is occupy the address, and the real owner then meets "an account already uses
this email address" when they try to link their own provider.

Recovery is **reset, then unlink**, and it takes both halves:

1. **Reset the password** at `/b/auth/forgot-password`. The link goes to the
   address, so only its owner can complete this. They now have a password, and
   the reset records the address proof.
2. **Unlink the other provider** at **Account → Security → Linked accounts**.
   The reset does *not* do this: it revokes their refresh tokens, so they lose
   the session within the access-token lifetime
   (`WAFER_RUN__AUTH__ACCESS_TOKEN_LIFETIME_SECS`, 30 minutes by default) rather
   than at once — but their `provider_links` row survives, and signing in with
   that provider again would put them straight back into the account. Removing
   the link is what evicts them.

That second step is new in this release — before it, nothing anywhere in the
product could remove a provider link. The page refuses to remove an account's
last way in, so set a password (step 1) before unlinking the only link.

### Auth: stored OAuth provider tokens are cleared (migration 014), and the device list empties once

**What changes.** An OAuth sign-in used to store the provider's access token in
`wafer_run__auth__provider_links.access_token`, in the clear. That token is a
live bearer credential for the user's account at Google, GitHub or Microsoft,
usable there by anyone who reads it, and nothing in Impresspress ever read it
back. Sign-ins now write the column empty, and auth migration 014 empties every
existing row, including links nobody signs in through any more. The column
itself stays, empty, and the admin SQL explorer keeps refusing the table.

**Upgrade with `--run-migrations`** to clear the tokens already stored. A
Cloudflare deploy runs migrations through `/_deploy/init` and gets it without
doing anything. A native deployment that skips the flag keeps the old tokens in
the table, and logs the `schema drift` warning for `wafer-run/auth` on each
boot, until it runs.

**What the migration run also does: every device leaves the session list.**
Auth migrations are re-run as a set whenever any auth migration changes, and
migration 012 in that set drops and recreates `wafer_run__auth__sessions`. So
the run that applies 014 also empties that table. Nothing authenticates against
it: it feeds the device list at **Account → Sessions**. After the upgrade that
list is empty, and each device reappears when it next refreshes its tokens.
Until a device reappears, its user cannot revoke that one device from the list;
**changing the password** still signs every device out, because it revokes the
refresh tokens rather than reading the list.

**Correction: this release's note said "nobody is signed out", and that was
wrong.** The reason given was that refresh tokens live in a separate table —
and so they do, in `wafer_run__auth__tokens`, which migration 004 in the same
re-run set opened by dropping. So the 014 upgrade, and every earlier auth
schema change, signed every user out within one access-token lifetime. 004 no
longer drops the table (see the API-key expiry note below), so from this
release onward an auth schema change keeps refresh tokens.

### Auth: an API key's expiry is a timestamp, and broken ones are revoked (migration 015)

**What changes.** `POST /b/auth/api/api-keys` used to store the `expires_at`
string it was sent, exactly as sent, and the lookup on every request compared
that string to the clock as **text**. Text order is time order only within one
format and one offset, so two whole classes of value were read wrong:

- an offset — `2026-09-23T20:00:00+09:00` is 11:00 UTC, but it sorts after
  `2026-09-23T12:00:00Z`, so the key kept authenticating for eight hours after
  it had expired;
- anything that is not a timestamp — `never` sorts after every timestamp there
  will ever be, so a key minted with it **never expired**.

The endpoint now answers `400` for an `expires_at` it cannot read as RFC 3339,
and for one already in the past; it accepts any offset and stores the instant
as `YYYY-MM-DDTHH:MM:SSZ`. The lookup parses the stored value instead of
comparing it as text, and treats an expiry it cannot parse as **expired** — a
key whose end date cannot be read has no enforceable end. A key now expires
*at* the instant it names rather than one second after it.

**Some existing keys stop working the moment you deploy, migrations or not.**
The parsing lookup is in the code half, so it applies immediately. Every stored
expiry that is not RFC 3339 is now read as expired, and RFC 3339 **requires a
UTC offset**. That includes shapes that look perfectly reasonable and that a
browser produces by default:

- `2027-01-31` — what `<input type="date">` posts;
- `2027-01-31T09:00` — what `<input type="datetime-local">` posts;
- `2027-01-31T09:00:00` — a timestamp with the offset left off;
- `2027-01-31T09:00:00+0900` — an ISO 8601 basic-form offset (no colon).

A key carrying any of these stops authenticating at once, **possibly months
before the date it names**. If you integrated against this endpoint with a date
picker, assume your keys are affected and reissue them with an explicit offset
(`2027-01-31T09:00:00Z`). Keys with no expiry at all are unaffected.

**Upgrade with `--run-migrations`** to make the column say so. Migration 015
respells a UTC expiry (`Z`, `z`, `+00:00`, `-00:00`, a space separator) as
`YYYY-MM-DDTHH:MM:SSZ` without moving the instant, and **revokes** every key
whose stored expiry the lookup cannot read, so the admin API-keys tab shows a
revoked key rather than one that quietly stopped working. The revoked set is
exactly the set the lookup refuses: all four shapes above, anything that is not
a timestamp at all, and timestamp-shaped values that name no instant — a day
the month does not have (`2026-02-31`, 29 February outside a leap year), a
field out of range (`T25:00:00Z`, a minute past 59, a second past 60 — 60
is a leap second and is read — an offset past `23:59`), a fraction with no offset after
it (`T12:00:00.5`), or anything trailing the offset. The stored text is left as
it was found on those rows: it is the only record of why the key was revoked.
Keys with no expiry are untouched. A sub-second fraction and a non-zero offset
are left as they stand: both are read correctly, and respelling either needs
arithmetic the migration deliberately does not do.

**Deploying this keeps everyone signed in; the device list empties.** Adding
migration 015 changes the auth block's SQL hash, so the migration run re-runs
the whole auth set. Migration 004 in that set used to open with `DROP TABLE IF
EXISTS wafer_run__auth__tokens`, the refresh-token table, and a refresh with no
stored row is refused — which is how every earlier auth schema change signed
every user out (see the correction in the migration-014 note above). This
release removes that DROP, and the set a migration run applies is the one
compiled into the binary doing the run, so this run already executes the 004
without it: refresh tokens, accounts, passwords, OAuth links and API keys all
survive. What does not is the session/device list at **Account → Sessions**:
migration 012 in the same set drops and recreates it, and it refills as each
device next refreshes its tokens.

### Auth: the OAuth start is rate-limited per IP, in a bucket of its own

**What changes.** `GET /b/auth/oauth/login` writes a PKCE state row on every
request, and it used to spend no rate-limit bucket at all. It now spends its own
IP-keyed bucket, `oauth_start`: **30 starts per 60 seconds per client address**
by default. It does not share the `auth` bucket that login, signup and the
password-reset endpoints spend, so a burst of OAuth starts cannot lock anyone
out of a password login, and loosening the one does not loosen the other.

**Who has to act.** A deployment that raised or disabled
`WAFER_RUN_SHARED__RATE_LIMIT_AUTH` — typically because many users share one
egress address (an office NAT, a campus, a proxy that does not forward the
client IP) — gets none of that headroom on the OAuth start: it is capped at the
30/60 default from the first request after the deploy, and users behind that
address see `429` on the provider buttons. Set the bucket by its own key:

```
WAFER_RUN_SHARED__RATE_LIMIT_OAUTH_START=300/60   # requests/seconds; 0 disables
```

Like every `WAFER_RUN_SHARED__RATE_LIMIT_*` category it is set by key — process
environment or the `variables` table — and no `ConfigVar` declares it, so it
does not appear as a field on any admin settings page. No migration is involved.

### Auth: a refused database call is a 403, not a 500

**What changes.** When WRAP refused one of the auth routes' database calls — a
deployment missing the grant a table needs, or a row guard — the route answered
`500 Internal server error (ref: …)`, the same as an outage. It now answers
`403` (`Access denied`), and a quota the service enforces answers `429`, as every
other block already did. The auth settings and organizations pages answer the
same statuses as a styled page. A genuine fault is still the `500` with a
correlation id.

**Who has to act.** Only an alert or a client that treats a `5xx` from
`/b/auth/*` as the one sign of a misconfigured deployment: a missing grant now
shows up as `403`, and the server log line reads `database access denied`. A
signed-in client that sees `403` from `/b/auth/api/refresh` or `/b/auth/api/me`
should not treat it as a sign-out — the credential is intact; the deployment
refused the read. No migration is involved.

### LLM: a provider can name its token-budget field (migration 002)

**What changes.** Which field carries the output-token budget in a chat request
used to follow the provider's protocol and nothing else: `open_ai` sent
`max_completion_tokens`, `open_ai_compatible` sent `max_tokens`. That is right
for every endpoint but one. Azure OpenAI is configured as
`open_ai_compatible`, and its *reasoning* deployments accept only
`max_completion_tokens` — so that operator had no reachable configuration and
every chat turn came back `400`. Providers now carry an optional
**Token budget field**, on the form and on
`POST`/`PATCH /b/llm/api/providers`, which overrides the protocol's spelling.

**Nothing changes for existing providers.** The column is nullable and empty
means "follow the protocol", which is what every configured provider was
already doing. There is no backfill.

**The provider table has no edit form**, only add / discover / delete, so the
new field is reachable from the admin page when you *create* a provider. An
Azure provider that already exists is changed through
`PATCH /b/llm/api/providers/{id}` (or by re-creating it) until an edit form
exists.

**One admin-API change to know about if you script against it.** On
`PATCH /b/llm/api/providers/{id}`, sending `"key_var": null` used to be
accepted and do nothing; it now clears the variable, the same as the empty
string already did and the same as `"max_tokens_field": null` does. Omitting
the key still leaves the stored value alone.

**Upgrade with `--run-migrations`** to add the column. Cloudflare deploys run
the block's migrations through `/_deploy/init` on every deploy, so a Cloudflare
deployment gets it without doing anything. A native deployment that skips the
flag logs the generic `schema drift; redeploy with --run-migrations to apply`
warning for the llm block on each boot; providers keep working, because a
native deployment leaves `WAFER_RUN__DATABASE__STRICT_SCHEMA` off and the
column is then added on the first provider write. If you have turned strict
schema **on**, the migration is not optional — a provider create or edit will
fail on the missing column until it runs.

### Admin: a user holds each role once (migration 004), and deleting a role revokes it

**What changes.** Role grants (`impresspress__admin__user_roles`) are now unique
per user and role, on every deployment the migration reaches (see the last
paragraph). Before, two logins of the bootstrap-admin address at the same
moment could each grant `admin`, leaving two identical rows — and revoking the
role then deleted one of them, reported success, and left the user an admin
through the other. Deleting a role also left every grant of it in place: its
name kept appearing in the tokens its former holders were issued, and creating
a role of the same name later handed it straight back to them. Deleting a role
now revokes it from everyone who held it and invalidates the access tokens
issued to them while they had it, and the audit row names the role and how many
grants went with it.

**One admin-API change.** `POST /b/admin/api/iam/user-roles` now grants only a
role that exists: a role name with no definition answers `400` ("No role named
… exists. Create the role first.") instead of writing a grant. Create the role
on the Roles tab first if you script against this endpoint.

**The repair deletes rows.** Migration `004_user_roles_unique` deletes
duplicate grant rows before creating the index, keeping **one** row for each
user and role — the least by `created_at`, then `id`, in the database's text
ordering (the earliest, on SQLite; on Postgres under a non-`C` collation the
order is the locale's, which need not be chronological). Which twin survives
does not matter: every row it deletes repeats a grant the kept row still
makes, so nobody gains or loses a role — what goes is the extra row a revoke
could miss.

**Data snapshots.** A `/b/dev` data snapshot exported before this release can
repeat a grant too. Importing it keeps one row per user and role, by the same
rule, rather than failing on the new index.

**Roles you deleted before upgrading are still granted.** The migration does
not touch grants that name a role which no longer exists. To find them, run this
from the admin SQL explorer:

```sql
SELECT ur.user_id, ur.role, ur.id
FROM impresspress__admin__user_roles AS ur
WHERE NOT EXISTS (
    SELECT 1 FROM impresspress__admin__roles AS r WHERE r.name = ur.role
)
ORDER BY ur.role, ur.user_id;
```

and revoke each one with `DELETE /b/admin/api/iam/user-roles/{id}` — or
re-create the role on the Roles tab and delete it again, which now revokes all
of them at once.

**No flag is needed to apply it.** A native deployment runs the admin block's
schema files before every boot, and a Cloudflare deploy runs every block's
migrations through `/_deploy/init`, so the repair and the index are in place on
the first boot or deploy of this release. A native deployment that does not
pass `--run-migrations` still logs the generic `schema drift` warning for the
admin block until it does once; that warning is about the recorded hash, not
the schema.

**Browser installs made before this release do not get it.** A browser install
applies migrations only when it creates its database; nothing passes
`--run-migrations` there, so an install whose admin schema already exists logs
the drift warning and keeps the old table, without the index. Twin grants
remain possible there, as before. The role-delete revocation and the
assign-endpoint check do not depend on the index and apply everywhere.

## The release workflow has never produced a release

Read this before you tag anything. No `v*` tag has ever existed in this
repository or upstream, so the
[Release workflow](../../actions/workflows/release.yml) has never run on a tag
and **no release has ever been published from it**. Its `publish` job has never
executed. Nothing below the dry run is a description of something observed
working end to end.

The only runs this workflow has are the dry runs introduced with it. That is
what the dry run is for: it is not optional pre-flight advice, it is the only
way anyone has ever seen any of this workflow run.

## Pre-Release Checklist

Before tagging a release, verify:

- [ ] `main` branch CI is green (check the [Actions tab](../../actions))
- [ ] Cross-platform builds pass (the `CI Main` workflow runs on every push to `main`)
- [ ] Update `version` in `Cargo.toml` workspace section to match the intended release
- [ ] **Run the release workflow as a dry run and see it green** (below)
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

## Dry run — the pre-flight step

```bash
# Run everything the release does except creating the release.
gh workflow run release.yml --ref main -f dry_run=true

# Watch it.
gh run list --workflow=release.yml --limit 1
gh run watch <run-id>
```

A dry run executes, for real:

1. **`verify-tag`** — reads `version` from `Cargo.toml`'s `[workspace.package]`
   table and prints the tag you must push (`v<version>`). On a branch there is
   no tag to compare, so it reports the expected one; on a tag it fails the run
   if the two disagree.
2. **`build-wasm`** — the `impresspress-web` wasm, via the same
   `build-wasm.yml` every CI run uses.
3. **`build`** — all five cross-compile targets, packaged as `.tar.gz`/`.zip`
   and uploaded as run artifacts.

It does **not** run `publish`, so no GitHub Release, and no tag, is created.
A skipped `publish` does not turn a red run green: a run's conclusion is
failure if any job failed, whatever was skipped afterwards.

Dispatching a branch requires `dry_run: true`; a non-dry-run dispatch must
target a tag, because `gh release create --verify-tag` has nothing to verify
otherwise.

## Creating a Release

```bash
# 1. Make sure you're on main and up to date
git checkout main
git pull

# 2. Dry-run first (see above). Do not skip this — the publish path has never
#    run, so a dry run is the only evidence that anything before it works.
gh workflow run release.yml --ref main -f dry_run=true

# 3. Tag the release. The tag MUST be `v` + the workspace version in
#    Cargo.toml, or the `verify-tag` job fails the run before anything builds.
git tag v0.1.0

# 4. Push the tag — this triggers the release workflow
git push origin v0.1.0
```

The [Release workflow](../../actions/workflows/release.yml) is intended to:
1. Check the tag against `Cargo.toml`'s workspace version and stop if they disagree
2. Build binaries for all 5 platforms (Linux amd64/arm64, macOS amd64/arm64, Windows amd64)
3. Create a GitHub Release (`gh release create --verify-tag`, so the tag must
   already exist — the command will not invent one) with auto-generated notes
   from merged PRs

Step 3 has never executed. If it fails, re-run the failed `Publish Release`
job on that same run — step 2's artifacts are still attached to it, so nothing
rebuilds. A fresh dispatch does NOT reuse them: it starts a new run and
rebuilds all five targets, which is the fallback once the run's artifacts have
expired. Either way, do not retag.

## After Release

- [ ] Verify the [GitHub Release](../../releases) was created with all 5 platform artifacts
- [ ] Download and smoke-test at least one binary
- [ ] Announce in relevant channels if this is a notable release

## Hotfix Process

Branch protection prevents pushing directly to `main` — hotfixes follow the same PR flow:

The tag must equal `v` + `Cargo.toml`'s `[workspace.package] version`, so the
version bump is part of the hotfix PR, not an afterthought — `verify-tag` fails
the run otherwise, before anything builds.

```bash
# 1. Create a hotfix branch
git checkout main && git pull
git checkout -b hotfix/v0.1.1

# 2. Fix the bug AND bump [workspace.package] version to 0.1.1 in Cargo.toml,
#    then commit and push both together
git push -u origin hotfix/v0.1.1

# 3. Open a PR — CI must pass, 1 approval required
gh pr create --title "fix: critical bug description"

# 4. After merge, tag the patch release — v + the version just landed
git checkout main && git pull
git tag v0.1.1
git push origin v0.1.1
```

## Undoing a Release

If a release was tagged by mistake or contains a critical issue:

```bash
# Delete the tag locally and remotely
git tag -d v0.1.0
git push origin --delete v0.1.0
```

Then delete the GitHub Release from the [Releases page](../../releases). Note: users who already downloaded the binary still have it.
