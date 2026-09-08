import { test, expect } from '@playwright/test';
import { ADMIN_STATE_PATH, loginAsAdmin } from './fixtures/auth';

/**
 * Behaviour cover for the delegated-action rule.
 *
 * Page markup carries no `on*=` attribute: a control declares
 * `data-action="<verb>"` plus its `data-*` operands, and a delegated listener
 * reads them back. `crates/impresspress-core/src/ui/assets/chrome.js` states
 * the rule and owns the shared verbs; a Rust test
 * (`ui::tests::pages_carry_no_event_handler_attributes`) enforces that no page
 * emits a handler attribute.
 *
 * What neither of those can see is whether the listeners actually fire. These
 * three cases are the ones with a visible effect, so a screenshot would not
 * catch a break either:
 *
 * 1. the database table filter, which used to be a 430-character minified
 *    `oninput` attribute — the longest in the tree;
 * 2. `modal-open` / `modal-close`, which replaced 16 `openModal('…')` /
 *    `closeModal('…')` attribute strings;
 * 3. `reveal-toggle` with its label operands, which replaced two copies of a
 *    hand-written password-reveal handler.
 *
 * They also prove the load-order assumption: `chrome.js` is `defer`red from
 * `<head>`, so its listeners must be installed before a user can click.
 */
test.describe('delegated actions', () => {
  test.use({ storageState: ADMIN_STATE_PATH });

  test('the database table filter hides non-matching rows and shows the empty hint', async ({
    page,
  }) => {
    await loginAsAdmin(page);
    await page.goto('/b/admin/database', { waitUntil: 'networkidle' });

    const rows = page.locator('[data-db-table]');
    const shown = page.locator('[data-db-table]:not([hidden])');
    await expect(rows.first()).toBeVisible();
    const total = await rows.count();
    expect(total).toBeGreaterThan(1);

    // Every deployment has the auth block's users table, and its full name
    // matches nothing else.
    const users = page.locator('[data-db-table="wafer_run__auth__users"]');
    const filter = page.locator('#db-filter');

    await filter.fill('wafer_run__auth__users');
    await expect(users).toBeVisible();
    await expect(shown).toHaveCount(1);
    await expect(page.locator('#db-filter-empty')).toBeHidden();

    // A query nothing matches collapses every group and reveals the hint.
    await filter.fill('zzz-no-such-table');
    await expect(page.locator('#db-filter-empty')).toBeVisible();
    await expect(users).toBeHidden();

    // Clearing restores the full list.
    await filter.fill('');
    await expect(users).toBeVisible();
    await expect(shown).toHaveCount(total);
    await expect(page.locator('#db-filter-empty')).toBeHidden();
  });

  test('modal-open and modal-close drive the create-variable modal', async ({ page }) => {
    await loginAsAdmin(page);
    await page.goto('/b/admin/variables', { waitUntil: 'networkidle' });

    const modal = page.locator('#create-var');
    await expect(modal).toBeHidden();

    await page.locator('[data-action="modal-open"][data-modal-target="create-var"]').click();
    await expect(modal).toBeVisible();

    await modal.locator('[data-action="modal-close"][data-modal-target="create-var"]').click();
    await expect(modal).toBeHidden();

    // The close button `components::modal` renders is the same verb.
    await page.locator('[data-action="modal-open"][data-modal-target="create-var"]').click();
    await expect(modal).toBeVisible();
    await modal.locator('button.modal-close').click();
    await expect(modal).toBeHidden();
  });

  test('reveal-toggle unmasks a secret field and swaps its accessible name', async ({ page }) => {
    await loginAsAdmin(page);
    await page.goto('/b/admin/email', { waitUntil: 'networkidle' });

    const field = page.locator('#IMPRESSPRESS__EMAIL__MAILGUN_API_KEY');
    const toggle = page.locator(
      '[data-action="reveal-toggle"][data-reveal-target="IMPRESSPRESS__EMAIL__MAILGUN_API_KEY"]',
    );

    await expect(field).toHaveAttribute('type', 'password');
    await expect(toggle).toHaveAttribute('aria-label', 'Reveal value');

    await toggle.click();
    await expect(field).toHaveAttribute('type', 'text');
    await expect(toggle).toHaveAttribute('aria-label', 'Hide value');

    await toggle.click();
    await expect(field).toHaveAttribute('type', 'password');
    await expect(toggle).toHaveAttribute('aria-label', 'Reveal value');
  });
});
