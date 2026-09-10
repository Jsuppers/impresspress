import { describe, expect, it } from "vitest";

import { coverage, NOT_JSON } from "../scripts/api-coverage.mjs";
import { callSites } from "../scripts/sdk-call-sites.mjs";

interface Site {
  service: string;
  method: string;
  path: string;
  raw: string;
  resolved: boolean;
}

/**
 * The freshness gate regenerates `src/generated/api.ts` from the committed
 * OpenAPI snapshots and CI diffs it. It can only see a reshaped response body
 * on an endpoint whose response is described — so a gate applied to a surface
 * where most endpoints describe nothing reports green on exactly the
 * endpoints where drift is possible but invisible. That is not hypothetical:
 * it is how PR #22's seven-response-body reshape passed both existing
 * snapshot gates (`1ccbb452`, "publish the RecordList envelope the JS SDK
 * reads").
 *
 * This file is what stops that from happening again. The call sites are read
 * out of `src/services/*.ts` rather than listed here, so adding a method that
 * hits an undescribed endpoint fails here on the next run.
 */
describe("the type-freshness gate covers the calls the SDK actually makes", () => {
  it("describes a JSON response for every resolvable call site", () => {
    const { covered, uncovered, total } = coverage();

    expect(
      uncovered.map((s: Site) => `${s.method} ${s.path} (${s.service})`),
      "these endpoints are called by the SDK but publish no response schema, so the " +
        "freshness gate cannot see a reshape of them - add `.output(response_schema_of::<T>)` " +
        "to the block's route table",
    ).toEqual([]);
    expect(covered).toHaveLength(total);
  });

  /**
   * Anti-vacuity. Everything above passes trivially if the extractor stops
   * finding call sites — a refactor of `BaseService.request`, a new call
   * shape, a moved directory. The floor is well below today's count and well
   * above zero: it only has to fail when the reader breaks.
   */
  it("still finds the SDK's call sites at all", () => {
    const sites: Site[] = callSites();
    expect(sites.length).toBeGreaterThan(60);
    for (const service of ["auth", "iam", "storage", "extensions"]) {
      expect(
        sites.some((s) => s.service === `${service}.service.ts`),
        `no call sites found in ${service}.service.ts`,
      ).toBe(true);
    }
  });

  /**
   * The unresolvable call sites are the products routes whose path is
   * assembled by `ownerProductPath` / `offerPath` — one interpolation there
   * expands to several path segments, so it cannot be matched against a path
   * template by substitution. They are named rather than silently dropped: an
   * unresolvable call site anywhere else means the reader met a shape it does
   * not understand, and the census is quietly under-counting.
   */
  it("cannot resolve only the helper-assembled products paths", () => {
    const { unresolved } = coverage();
    expect(
      unresolved
        .filter((s: Site) => !s.raw.startsWith("/b/products/${this."))
        .map((s: Site) => `${s.method} ${s.raw}`),
    ).toEqual([]);
  });

  /**
   * Both documented exceptions must still BE call sites. An exception for a
   * call the SDK no longer makes excuses nothing and survives the method
   * being deleted.
   */
  it("excludes exactly the two call sites that do not answer JSON", () => {
    const { notJson } = coverage();
    expect(notJson.map((s: Site) => `${s.method} ${s.path}`).sort()).toEqual(
      NOT_JSON.map((e: { method: string; path: string }) => `${e.method} ${e.path}`).sort(),
    );
  });
});
