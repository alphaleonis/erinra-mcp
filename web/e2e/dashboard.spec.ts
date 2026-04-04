import { test, expect } from './fixtures';

test.describe('auth', () => {
	test('API requests without token return 401', async ({ request }) => {
		const response = await request.get('/api/discover');
		expect(response.status()).toBe(401);
		expect(response.headers()['www-authenticate']).toBe('Bearer');
	});

	test('API requests with wrong token return 401', async ({ request }) => {
		const response = await request.get('/api/discover', {
			headers: { Authorization: 'Bearer wrong-token' },
		});
		expect(response.status()).toBe(401);
	});

	test('API requests with valid token return 200', async ({ authedRequest }) => {
		const response = await authedRequest.get('/api/discover');
		expect(response.ok()).toBeTruthy();
	});

	test('SPA loads without token (static assets are unprotected)', async ({ page }) => {
		await page.goto('/');
		await expect(page).toHaveTitle('Erinra Dashboard');
		await expect(page.locator('h1')).toHaveText('Erinra');
	});

	test('SPA with token loads data successfully', async ({ authedPage }) => {
		// authedPage already navigated with ?token=, so API calls should work.
		const list = authedPage.locator('[data-testid="memory-list"]');
		await expect(list).toBeVisible({ timeout: 10_000 });

		const items = list.locator('[data-testid="memory-item"]');
		await expect(items.first()).toBeVisible();
	});

	test('token is stripped from URL after initialization', async ({ page, authToken, baseURL }) => {
		await page.goto(`${baseURL}/?token=${authToken}`);
		await expect(page).toHaveTitle('Erinra Dashboard');

		// Token should be stripped from the URL.
		await expect(page).not.toHaveURL(/token=/);
	});

	test('token persists across page refresh via sessionStorage', async ({ page, authToken, baseURL }) => {
		// First visit with token.
		await page.goto(`${baseURL}/?token=${authToken}`);
		await expect(page).toHaveTitle('Erinra Dashboard');

		// Wait for data to load to confirm auth works.
		const list = page.locator('[data-testid="memory-list"]');
		await expect(list).toBeVisible({ timeout: 10_000 });

		// Refresh the page (no token in URL this time).
		await page.reload();
		await expect(page).toHaveTitle('Erinra Dashboard');

		// Data should still load (token recovered from sessionStorage).
		await expect(list).toBeVisible({ timeout: 10_000 });
		const items = list.locator('[data-testid="memory-item"]');
		await expect(items.first()).toBeVisible();
	});

	test('API calls from SPA include Authorization header', async ({ page, authToken, baseURL }) => {
		// Intercept API requests to verify they include the auth header.
		const apiRequests: { url: string; authorization: string | null }[] = [];
		await page.route('/api/**', async (route) => {
			const headers = route.request().headers();
			apiRequests.push({
				url: route.request().url(),
				authorization: headers['authorization'] ?? null,
			});
			await route.continue();
		});

		await page.goto(`${baseURL}/?token=${authToken}`);
		await expect(page).toHaveTitle('Erinra Dashboard');

		// Wait for API calls to happen.
		const list = page.locator('[data-testid="memory-list"]');
		await expect(list).toBeVisible({ timeout: 10_000 });

		// All API requests should have the Authorization header.
		expect(apiRequests.length).toBeGreaterThan(0);
		for (const req of apiRequests) {
			expect(req.authorization).toBe(`Bearer ${authToken}`);
		}
	});
});

test.describe('dashboard', () => {
	test('loads without CSP errors', async ({ authedPage: page }) => {
		const cspErrors: string[] = [];
		page.on('console', (msg) => {
			if (msg.text().includes('Content-Security-Policy')) {
				cspErrors.push(msg.text());
			}
		});

		// authedPage already navigated, but reload to capture console from start.
		await page.reload();
		await expect(page).toHaveTitle('Erinra Dashboard');
		expect(cspErrors).toEqual([]);
	});

	test('shows the header', async ({ authedPage: page }) => {
		await expect(page.locator('h1')).toHaveText('Erinra');
	});

	test('displays memory list', async ({ authedPage: page }) => {
		const list = page.locator('[data-testid="memory-list"]');
		await expect(list).toBeVisible({ timeout: 10_000 });

		const items = list.locator('[data-testid="memory-item"]');
		await expect(items.first()).toBeVisible();
		expect(await items.count()).toBeGreaterThanOrEqual(1);
	});

	test('sidebar shows projects and types', async ({ authedPage: page }) => {
		const sidebar = page.locator('aside');
		await expect(sidebar.getByText('Projects')).toBeVisible({ timeout: 10_000 });

		await expect(sidebar.getByRole('button', { name: 'erinra' })).toBeVisible();
		await expect(sidebar.getByRole('button', { name: 'vestige' })).toBeVisible();
	});

	test('navigates to memory detail page', async ({ authedPage: page }) => {
		const firstItem = page.locator('[data-testid="memory-item"]').first();
		await expect(firstItem).toBeVisible({ timeout: 10_000 });
		await firstItem.click();

		await expect(page).toHaveURL(/\/memory\//);
		await expect(page.getByText('Back to list')).toBeVisible();
		await expect(page.getByText('Created')).toBeVisible();
		await expect(page.getByText('Updated')).toBeVisible();
	});

	test('detail page back link returns to list', async ({ authedPage: page }) => {
		const firstItem = page.locator('[data-testid="memory-item"]').first();
		await expect(firstItem).toBeVisible({ timeout: 10_000 });
		await firstItem.click();
		await expect(page).toHaveURL(/\/memory\//);

		await page.getByText('Back to list').click();
		await expect(page).toHaveURL('/');
	});

	test('clicking a link on detail page updates content', async ({ authedPage: page, authedRequest }) => {
		const memoriesResp = await authedRequest.get('/api/memories?limit=20');
		const { memories } = await memoriesResp.json();

		let sourceId: string | null = null;
		let targetContent: string | null = null;
		for (const mem of memories) {
			const detailResp = await authedRequest.get(`/api/memories/${mem.id}`);
			const detail = await detailResp.json();
			if (detail.outgoing_links.length > 0) {
				sourceId = mem.id;
				const targetResp = await authedRequest.get(`/api/memories/${detail.outgoing_links[0].target_id}`);
				const target = await targetResp.json();
				targetContent = target.memory.content.slice(0, 40);
				break;
			}
		}
		expect(sourceId).not.toBeNull();

		await page.goto(`/memory/${sourceId}`);
		await expect(page.getByText('Links')).toBeVisible({ timeout: 10_000 });

		const contentBefore = await page.locator('.prose').textContent();

		const linkEl = page.locator('section a[href^="/memory/"]').first();
		await linkEl.click();

		await expect(page).not.toHaveURL(`/memory/${sourceId}`);
		await expect(page).toHaveURL(/\/memory\//);
		await expect(page.locator('.prose')).not.toHaveText(contentBefore!);
		await expect(page.locator('.prose')).toContainText(targetContent!);
	});

	test('API discover endpoint returns valid JSON', async ({ authedRequest }) => {
		const response = await authedRequest.get('/api/discover');
		expect(response.ok()).toBeTruthy();

		const data = await response.json();
		expect(data).toHaveProperty('projects');
		expect(data).toHaveProperty('types');
		expect(data).toHaveProperty('tags');
		expect(data).toHaveProperty('stats');
	});

	test('API memories endpoint returns seeded data', async ({ authedRequest }) => {
		const response = await authedRequest.get('/api/memories');
		expect(response.ok()).toBeTruthy();

		const data = await response.json();
		expect(data.total).toBeGreaterThanOrEqual(6);
		expect(data.memories.length).toBeGreaterThanOrEqual(1);
	});

	test('search bar triggers search and shows results with scores', async ({ authedPage: page }) => {
		const list = page.locator('[data-testid="memory-list"]');
		await expect(list).toBeVisible({ timeout: 10_000 });

		const searchInput = page.locator('[data-testid="search-input"]');
		await expect(searchInput).toBeVisible();
		await searchInput.fill('embedding');
		await searchInput.press('Enter');

		await expect(list).toBeVisible({ timeout: 10_000 });

		const items = list.locator('[data-testid="memory-item"]');
		await expect(items.first()).toBeVisible();

		const scores = list.locator('[data-testid="score"]');
		await expect(scores.first()).toBeVisible();

		await expect(page).toHaveURL(/q=embedding/);
	});

	test('API search endpoint returns results', async ({ authedRequest }) => {
		const response = await authedRequest.get('/api/memories/search?q=SQLite');
		expect(response.ok()).toBeTruthy();

		const data = await response.json();
		expect(data.results).toBeDefined();
		expect(data.total).toBeGreaterThanOrEqual(1);
		expect(data.results[0].score).toBeGreaterThan(0);
		expect(data.results[0].memory).toHaveProperty('id');
		expect(data.results[0].memory).toHaveProperty('content');
	});

	test('API search endpoint returns 400 without q param', async ({ authedRequest }) => {
		const response = await authedRequest.get('/api/memories/search');
		expect(response.status()).toBe(400);
	});
});
