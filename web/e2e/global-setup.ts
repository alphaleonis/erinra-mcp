import { execSync } from 'node:child_process';

/**
 * Playwright global setup: build the frontend SPA.
 *
 * Note: data dir cleanup happens in the webServer command (playwright.config.ts)
 * because the webServer starts BEFORE globalSetup.
 */
export default function globalSetup() {
	// Build the SPA so the debug server has assets to serve.
	execSync('npm run build', { cwd: process.cwd(), stdio: 'inherit' });
}
