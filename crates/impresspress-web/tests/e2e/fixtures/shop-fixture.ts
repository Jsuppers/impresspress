/**
 * One product and one offer, as the products admin API takes them.
 *
 * Two specs stock a shop and they must stock the *same* shop: `webmcp.spec.ts`
 * seeds it over HTTP against the native server (to have something for
 * `get_product` / `preview_price` to answer with), and `dev-workspace.spec.ts`
 * has an in-page agent create it through the `shop_*` tools inside the browser
 * sandbox. Same wire shape, two very different transports — which is exactly
 * the pair that drifts if each keeps its own literal.
 *
 * The offer is deliberately the interesting kind rather than a flat price:
 * `pricing_model: "components"` with one typed customer input (`pages`,
 * integer, 1–20) priced at 1500 minor units each. A fixed-price offer would
 * exercise none of the typed-input machinery the products block exists for,
 * and `webmcp.spec.ts` prices it (3 × 1500 = 4500) as its `preview_price`
 * assertion.
 *
 * Field names come from `products/contracts.rs`
 * (`CreateProductRequest`) and the hand-written `offer_definition_schema`
 * in `products/mod.rs`; both are published in
 * `impresspress-core/tests/snapshots/dev.tools.json` as the `shop_*` tools'
 * `inputSchema`, which is what an agent actually sees.
 */

/**
 * The product. No `status`: the API creates it as `draft`, and a draft is
 * invisible to shoppers — which is the state
 * `dev-workspace.spec.ts` then flips with `shop_update_product`, and the
 * reason a caller that wants it live immediately (`webmcp.spec.ts`) has to
 * say so explicitly.
 */
export const SHOP_PRODUCT = {
  name: 'Custom print',
  slug: 'custom-print',
  description: 'Made to order, priced by the page.',
  currency: 'nzd',
  fulfillment_kind: 'manual',
};

/**
 * The offer, minus the `product_id` the route carries. Created as `draft` and
 * unpurchasable until `shop_publish_offer` (or
 * `POST …/offers/{id}/publish`) publishes it.
 */
export const SHOP_OFFER = {
  name: 'Custom print',
  mode: 'payment',
  currency: 'nzd',
  pricing_model: 'components',
  usage_type: 'licensed',
  billing_scheme: 'per_unit',
  tax_behavior: 'exclusive',
  variables: [
    {
      key: 'pages',
      kind: 'integer',
      label: 'Pages',
      required: true,
      minimum: '1',
      maximum: '20',
      step: '1',
      sort_order: 0,
    },
  ],
  components: [
    {
      key: 'pages',
      label: 'Printed pages',
      sort_order: 0,
      required: true,
      amount: { type: 'per_unit', input: 'pages', unit_amount_minor: 1500 },
    },
  ],
  checkout: {},
};

/**
 * [`SHOP_PRODUCT`] with a per-run-unique `name`/`slug`, live on creation.
 *
 * Only the native-server spec needs this: its database outlives the run, and
 * `slug` is unique per owner among non-deleted products, so a second run of
 * the same suite against the same server would collide. The browser sandbox
 * creates a fresh OPFS database per Playwright context, so it uses the plain
 * constant and gets a readable, stable slug.
 */
export function uniqueShopProduct(stamp: string) {
  return {
    ...SHOP_PRODUCT,
    name: `${SHOP_PRODUCT.name} ${stamp}`,
    slug: `${SHOP_PRODUCT.slug}-${stamp}`,
    status: 'active',
  };
}
