/**
 * Static assets module for Impresspress SDK
 * Provides paths to bundled static assets like logos and icons.
 *
 * The art is pixel art (generated in the `site` repo's `brand/` kit): `logo`
 * is the 64x64 mark — display it at a whole multiple of 64px with
 * `image-rendering: pixelated` — and `favicon` carries 16/32/48 px frames.
 * There is no raster wordmark; render the app name as text next to the mark.
 */

// Asset paths relative to the package root
export const IMPRESSPRESS_ASSETS = {
  logo: '@impresspress/sdk/static/logo.png',
  favicon: '@impresspress/sdk/static/favicon.ico',
} as const;

// Helper to get the absolute path to assets
export function getImpresspressAssetPath(asset: keyof typeof IMPRESSPRESS_ASSETS): string {
  return `/node_modules/${IMPRESSPRESS_ASSETS[asset]}`;
}

// Export for convenience
export const impresspressAssets = IMPRESSPRESS_ASSETS;