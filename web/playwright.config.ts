import path from 'node:path';
import { defineConfig } from '@playwright/test';

const PORT = 9877;
const E2E_DATA_DIR = path.resolve(process.cwd(), '.e2e-data');
export const E2E_AUTH_TOKEN = 'e2e-test-token-playwright-9877';

export default defineConfig({
	testDir: 'e2e',
	fullyParallel: true,
	forbidOnly: !!process.env.CI,
	retries: process.env.CI ? 2 : 0,
	workers: 1, // single server instance, sequential tests
	reporter: process.env.CI ? 'github' : 'list',
	use: {
		baseURL: `http://127.0.0.1:${PORT}`,
		trace: 'on-first-retry',
	},
	projects: [
		{
			name: 'chromium',
			use: { browserName: 'chromium' },
		},
	],
	webServer: {
		// Clean stale data, seed the test database, then start the server.
		// Cleanup must happen here (not globalSetup) because webServer starts BEFORE globalSetup.
		command: [
			`rm -rf "${E2E_DATA_DIR}"`,
			`&&`,
			`cargo run --example seed_test_db --features test-utils -- "${E2E_DATA_DIR}"`,
			`&&`,
			`ERINRA_AUTH_TOKEN=${E2E_AUTH_TOKEN}`,
			`cargo run -- --data-dir "${E2E_DATA_DIR}" dash --port ${PORT} --no-open`,
		].join(' '),
		url: `http://127.0.0.1:${PORT}`,
		reuseExistingServer: !process.env.CI,
		cwd: '..',
		// Embedding model loading (~10-30s) added to server startup for search feature.
		timeout: 120_000,
	},
	globalSetup: 'e2e/global-setup.ts',
	globalTeardown: 'e2e/global-teardown.ts',
});
