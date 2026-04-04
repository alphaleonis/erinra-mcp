import { test as base, expect } from '@playwright/test';
import { E2E_AUTH_TOKEN } from '../playwright.config';

/**
 * Extended test fixture that provides the daemon auth token.
 *
 * - `authToken`: the known test token (set via ERINRA_AUTH_TOKEN env var)
 * - `authedPage`: a Page that has already navigated with `?token=` so the SPA stores it
 * - `authedRequest`: an APIRequestContext with the Authorization header pre-set
 */
export const test = base.extend<{
	authToken: string;
	authedPage: import('@playwright/test').Page;
	authedRequest: import('@playwright/test').APIRequestContext;
}>({
	authToken: async ({}, use) => {
		await use(E2E_AUTH_TOKEN);
	},

	authedPage: async ({ page, authToken, baseURL }, use) => {
		// Navigate with token so initAuthFromUrl() stores it in sessionStorage.
		await page.goto(`${baseURL}/?token=${authToken}`);
		// Wait for the SPA to initialize (token gets stripped from URL).
		await expect(page).toHaveTitle('Erinra Dashboard');
		await use(page);
	},

	authedRequest: async ({ playwright, baseURL, authToken }, use) => {
		const context = await playwright.request.newContext({
			baseURL,
			extraHTTPHeaders: {
				Authorization: `Bearer ${authToken}`,
			},
		});
		await use(context);
		await context.dispose();
	},
});

export { expect };
