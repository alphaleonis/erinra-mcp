import { describe, it, expect, vi, beforeEach } from 'vitest';
import { createMemoryBrowser, type RouterAdapter } from './memory-browser.svelte';
import type { ListResult, DiscoverResult, SearchResult } from '$lib/api';

function mockRouter(): RouterAdapter & { lastUrl: string | null } {
	const router = {
		lastUrl: null as string | null,
		navigate(url: string) {
			router.lastUrl = url;
		},
		getSearchParams() {
			return new URLSearchParams();
		},
	};
	return router;
}

function mockListResult(overrides?: Partial<ListResult>): ListResult {
	return {
		memories: [],
		total: 0,
		...overrides,
	};
}

function mockDiscoverResult(): DiscoverResult {
	return {
		projects: [{ name: 'vestige', count: 8 }],
		types: [{ name: 'fact', count: 11 }],
		tags: [{ name: 'rust', count: 3 }],
		relations: [],
		stats: {
			total_memories: 30,
			total_archived: 2,
			storage_size_bytes: 1024000,
			embedding_model: 'nomic-embed-text-v1.5',
		},
	};
}

describe('createMemoryBrowser', () => {
	beforeEach(() => {
		vi.restoreAllMocks();
	});

	it('selectFilter with replace mode sets single value and triggers fetch', async () => {
		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify(mockListResult({ total: 5 })), { status: 200 })
		);
		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		browser.selectFilter('project', 'vestige', false);

		expect(browser.filters.projects).toEqual(['vestige']);

		// Wait for the fetch to complete
		await vi.waitFor(() => {
			expect(fetchSpy).toHaveBeenCalledOnce();
		});

		const calledUrl = fetchSpy.mock.calls[0][0] as string;
		expect(calledUrl).toContain('/api/memories');
		expect(calledUrl).toContain('project=vestige');
	});

	it('selectFilter with replace mode toggles off if already selected', async () => {
		vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify(mockListResult()), { status: 200 })
		);
		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		browser.selectFilter('project', 'vestige', false);
		expect(browser.filters.projects).toEqual(['vestige']);

		browser.selectFilter('project', 'vestige', false);
		expect(browser.filters.projects).toEqual([]);
	});

	it('selectFilter with additive mode adds value to list', async () => {
		vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify(mockListResult()), { status: 200 })
		);
		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		browser.selectFilter('project', 'vestige', false);
		browser.selectFilter('project', 'dotlens', true);
		expect(browser.filters.projects).toEqual(['vestige', 'dotlens']);
	});

	it('selectFilter with additive mode removes value if already present', async () => {
		vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify(mockListResult()), { status: 200 })
		);
		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		browser.selectFilter('project', 'vestige', false);
		browser.selectFilter('project', 'dotlens', true);
		expect(browser.filters.projects).toEqual(['vestige', 'dotlens']);

		browser.selectFilter('project', 'vestige', true);
		expect(browser.filters.projects).toEqual(['dotlens']);
	});

	it('selectFilter resets offset to 0', async () => {
		vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify(mockListResult()), { status: 200 })
		);
		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		browser.goToPage(40);
		expect(browser.offset).toBe(40);

		browser.selectFilter('project', 'vestige', false);
		expect(browser.offset).toBe(0);
	});

	it('removeFilter resets offset to 0', async () => {
		vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify(mockListResult()), { status: 200 })
		);
		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		browser.selectFilter('project', 'vestige', false);
		browser.goToPage(40);
		expect(browser.offset).toBe(40);

		browser.removeFilter('project', 'vestige');
		expect(browser.offset).toBe(0);
	});

	it('clearFilters resets offset to 0', async () => {
		vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify(mockListResult()), { status: 200 })
		);
		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		browser.selectFilter('project', 'vestige', false);
		browser.goToPage(40);
		expect(browser.offset).toBe(40);

		browser.clearFilters();
		expect(browser.offset).toBe(0);
	});

	it('goToPage sets offset and triggers fetch', async () => {
		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify(mockListResult({ total: 100 })), { status: 200 })
		);
		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		browser.goToPage(40);
		expect(browser.offset).toBe(40);

		await vi.waitFor(() => {
			expect(fetchSpy).toHaveBeenCalled();
		});

		const calledUrl = fetchSpy.mock.calls[0][0] as string;
		expect(calledUrl).toContain('offset=40');
	});

	it('every action calls router.navigate with correctly serialized params', async () => {
		vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify(mockListResult()), { status: 200 })
		);
		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		// Default state: empty URL (offset=0, limit=20 omitted)
		browser.selectFilter('project', 'vestige', false);
		expect(router.lastUrl).toBe('?project=vestige');

		// Add type filter
		browser.selectFilter('type', 'fact', true);
		const params1 = new URLSearchParams(router.lastUrl!.substring(1));
		expect(params1.getAll('project')).toEqual(['vestige']);
		expect(params1.getAll('type')).toEqual(['fact']);
		expect(params1.has('offset')).toBe(false); // offset=0 omitted
		expect(params1.has('limit')).toBe(false);  // limit=20 omitted

		// goToPage includes offset
		browser.goToPage(40);
		const params2 = new URLSearchParams(router.lastUrl!.substring(1));
		expect(params2.get('offset')).toBe('40');
		expect(params2.getAll('project')).toEqual(['vestige']);

		// clearFilters clears everything
		browser.clearFilters();
		expect(router.lastUrl).toBe('');
	});

	it('every action triggers a fetch with correct API URL', async () => {
		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify(mockListResult()), { status: 200 })
		);
		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		// selectFilter
		browser.selectFilter('project', 'vestige', false);
		await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalledTimes(1));
		let url = fetchSpy.mock.calls[0][0] as string;
		expect(url).toContain('/api/memories');
		expect(url).toContain('project=vestige');
		expect(url).toContain('limit=20');

		// goToPage
		browser.goToPage(20);
		await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalledTimes(2));
		url = fetchSpy.mock.calls[1][0] as string;
		expect(url).toContain('offset=20');
		expect(url).toContain('project=vestige');

		// removeFilter
		browser.removeFilter('project', 'vestige');
		await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalledTimes(3));
		url = fetchSpy.mock.calls[2][0] as string;
		expect(url).not.toContain('project=');
		expect(url).not.toContain('offset='); // offset reset to 0

		// clearFilters
		browser.selectFilter('tag', 'rust', false);
		browser.clearFilters();
		await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalledTimes(5));
		url = fetchSpy.mock.calls[4][0] as string;
		expect(url).toBe('/api/memories?limit=20');
	});

	it('race condition: two rapid calls, only second result kept', async () => {
		const firstResult = mockListResult({ total: 10, memories: [{ id: 'first', content: 'first', memory_type: 'fact', projects: [], tags: [], created_at: '', updated_at: '', archived_at: null, last_accessed_at: null, access_count: 0, truncated: false }] });
		const secondResult = mockListResult({ total: 20, memories: [{ id: 'second', content: 'second', memory_type: 'fact', projects: [], tags: [], created_at: '', updated_at: '', archived_at: null, last_accessed_at: null, access_count: 0, truncated: false }] });

		let resolveFirst!: (v: Response) => void;
		let resolveSecond!: (v: Response) => void;

		const fetchSpy = vi.spyOn(globalThis, 'fetch')
			.mockImplementationOnce(() => new Promise(r => { resolveFirst = r; }))
			.mockImplementationOnce(() => new Promise(r => { resolveSecond = r; }));

		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		// Fire two rapid calls
		browser.selectFilter('project', 'vestige', false);
		browser.selectFilter('project', 'dotlens', false);

		// Resolve second first, then first
		resolveSecond(new Response(JSON.stringify(secondResult), { status: 200 }));
		await vi.waitFor(() => expect(browser.total).toBe(20));
		expect(browser.memories[0].id).toBe('second');

		// Now resolve first — it should be discarded
		resolveFirst(new Response(JSON.stringify(firstResult), { status: 200 }));
		// Give it a tick to process
		await new Promise(r => setTimeout(r, 10));

		// Still shows second result, first was discarded
		expect(browser.total).toBe(20);
		expect(browser.memories[0].id).toBe('second');
	});

	it('initialize() restores state from URL params and fires both fetches', async () => {
		const listResult = mockListResult({ total: 50 });
		const discoverResult = mockDiscoverResult();

		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async (input) => {
			const url = String(input);
			if (url.includes('/api/discover')) {
				return new Response(JSON.stringify(discoverResult), { status: 200 });
			}
			return new Response(JSON.stringify(listResult), { status: 200 });
		});

		const router = {
			lastUrl: null as string | null,
			navigate(url: string) { router.lastUrl = url; },
			getSearchParams() {
				const params = new URLSearchParams();
				params.set('project', 'vestige');
				params.set('offset', '20');
				return params;
			},
		};

		const browser = createMemoryBrowser({ router });
		browser.initialize();

		expect(browser.filters.projects).toEqual(['vestige']);
		expect(browser.offset).toBe(20);

		await vi.waitFor(() => {
			expect(fetchSpy).toHaveBeenCalledTimes(2);
		});

		// One call to /api/memories, one to /api/discover
		const urls = fetchSpy.mock.calls.map(c => String(c[0]));
		expect(urls.some(u => u.includes('/api/memories'))).toBe(true);
		expect(urls.some(u => u.includes('/api/discover'))).toBe(true);

		// Verify memories URL has correct params
		const memoriesUrl = urls.find(u => u.includes('/api/memories'))!;
		expect(memoriesUrl).toContain('project=vestige');
		expect(memoriesUrl).toContain('offset=20');

		await vi.waitFor(() => {
			expect(browser.total).toBe(50);
			expect(browser.discover).not.toBeNull();
		});
	});

	it('URL round-trip: serialize then restore produces identical params', async () => {
		vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify(mockListResult()), { status: 200 })
		);
		const router = mockRouter();
		const browser1 = createMemoryBrowser({ router });

		// Set up some state
		browser1.selectFilter('project', 'vestige', false);
		browser1.selectFilter('type', 'fact', true);
		browser1.selectFilter('tag', 'rust', true);
		browser1.goToPage(40);

		const serializedUrl = router.lastUrl!;

		// Create a second browser that restores from those params
		const router2 = {
			lastUrl: null as string | null,
			navigate(url: string) { router2.lastUrl = url; },
			getSearchParams() {
				return new URLSearchParams(serializedUrl.substring(1));
			},
		};

		const browser2 = createMemoryBrowser({ router: router2 });
		browser2.initialize();

		expect(browser2.filters.projects).toEqual(['vestige']);
		expect(browser2.filters.types).toEqual(['fact']);
		expect(browser2.filters.tags).toEqual(['rust']);
		expect(browser2.offset).toBe(40);

		// Now trigger an action on browser2 to serialize its state
		browser2.goToPage(40); // same offset, just to trigger URL update
		expect(router2.lastUrl).toBe(serializedUrl);
	});

	it('loading state is true during fetch and error set on failure', async () => {
		let resolveFirst!: (v: Response) => void;
		vi.spyOn(globalThis, 'fetch')
			.mockImplementationOnce(() => new Promise(r => { resolveFirst = r; }));

		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		browser.selectFilter('project', 'vestige', false);
		expect(browser.loading).toBe(true);

		resolveFirst(new Response(JSON.stringify(mockListResult({ total: 5 })), { status: 200 }));
		await vi.waitFor(() => expect(browser.loading).toBe(false));
		expect(browser.listError).toBeNull();
		expect(browser.total).toBe(5);
	});

	it('switches to search mode when query is set', async () => {
		const mockSearchResult: SearchResult = {
			results: [
				{
					memory: {
						id: 'mem-1',
						content: 'Rust error handling',
						memory_type: 'fact',
						projects: ['erinra'],
						tags: ['rust'],
						created_at: '',
						updated_at: '',
						archived_at: null,
						last_accessed_at: null,
						access_count: 0,
						truncated: false,
					},
					outgoing_links: [],
					incoming_links: [],
					score: 0.85,
				},
			],
			total: 1,
		};

		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async (input) => {
			const url = String(input);
			if (url.includes('/api/memories/search')) {
				return new Response(JSON.stringify(mockSearchResult), { status: 200 });
			}
			return new Response(JSON.stringify(mockListResult()), { status: 200 });
		});

		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		browser.setQuery('error handling');

		await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());

		// Should have called the search endpoint, not the list endpoint.
		const calledUrl = fetchSpy.mock.calls[fetchSpy.mock.calls.length - 1][0] as string;
		expect(calledUrl).toContain('/api/memories/search');
		expect(calledUrl).toContain('q=error+handling');
	});

	it('clears search mode when query is set to empty string', async () => {
		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async (input) => {
			const url = String(input);
			if (url.includes('/api/memories/search')) {
				return new Response(JSON.stringify({ results: [], total: 0 }), { status: 200 });
			}
			return new Response(JSON.stringify(mockListResult()), { status: 200 });
		});

		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		// Enter search mode.
		browser.setQuery('test query');
		await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());

		let lastUrl = fetchSpy.mock.calls[fetchSpy.mock.calls.length - 1][0] as string;
		expect(lastUrl).toContain('/api/memories/search');

		// Clear the query.
		browser.setQuery('');
		await vi.waitFor(() => expect(fetchSpy.mock.calls.length).toBeGreaterThanOrEqual(2));

		lastUrl = fetchSpy.mock.calls[fetchSpy.mock.calls.length - 1][0] as string;
		expect(lastUrl).toContain('/api/memories');
		expect(lastUrl).not.toContain('/api/memories/search');
		expect(browser.filters.query).toBe('');
	});

	it('search query round-trips through URL serialization', async () => {
		vi.spyOn(globalThis, 'fetch').mockImplementation(async (input) => {
			const url = String(input);
			if (url.includes('/api/memories/search')) {
				return new Response(JSON.stringify({ results: [], total: 0 }), { status: 200 });
			}
			if (url.includes('/api/discover')) {
				return new Response(JSON.stringify(mockDiscoverResult()), { status: 200 });
			}
			return new Response(JSON.stringify(mockListResult()), { status: 200 });
		});

		const router = mockRouter();
		const browser1 = createMemoryBrowser({ router });

		// Set query and a filter.
		browser1.setQuery('embedding search');
		browser1.selectFilter('project', 'erinra', false);

		const serializedUrl = router.lastUrl!;
		expect(serializedUrl).toContain('q=embedding+search');
		expect(serializedUrl).toContain('project=erinra');

		// Restore from URL in a second browser.
		const router2 = {
			lastUrl: null as string | null,
			navigate(url: string) { router2.lastUrl = url; },
			getSearchParams() {
				return new URLSearchParams(serializedUrl.substring(1));
			},
		};

		const browser2 = createMemoryBrowser({ router: router2 });
		browser2.initialize();

		expect(browser2.filters.query).toBe('embedding search');
		expect(browser2.filters.projects).toEqual(['erinra']);
	});

	it('clearFilters also clears the search query', async () => {
		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async (input) => {
			const url = String(input);
			if (url.includes('/api/memories/search')) {
				return new Response(JSON.stringify({ results: [], total: 0 }), { status: 200 });
			}
			return new Response(JSON.stringify(mockListResult()), { status: 200 });
		});

		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		browser.setQuery('something');
		await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());

		browser.clearFilters();
		await vi.waitFor(() => expect(fetchSpy.mock.calls.length).toBeGreaterThanOrEqual(2));

		expect(browser.filters.query).toBe('');
		const lastUrl = fetchSpy.mock.calls[fetchSpy.mock.calls.length - 1][0] as string;
		expect(lastUrl).not.toContain('/api/memories/search');
	});

	it('toggleIncludeArchived triggers fetch with include_archived=true in API URL', async () => {
		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify(mockListResult()), { status: 200 })
		);
		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		browser.toggleIncludeArchived();

		expect(browser.filters.includeArchived).toBe(true);

		await vi.waitFor(() => {
			expect(fetchSpy).toHaveBeenCalledOnce();
		});

		const calledUrl = fetchSpy.mock.calls[0][0] as string;
		expect(calledUrl).toContain('/api/memories');
		expect(calledUrl).toContain('include_archived=true');
	});

	it('includeArchived is persisted in browser URL as include_archived=true', async () => {
		vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify(mockListResult()), { status: 200 })
		);
		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		browser.toggleIncludeArchived();
		expect(router.lastUrl).toContain('include_archived=true');

		// Toggle off — should not appear in URL
		browser.toggleIncludeArchived();
		expect(router.lastUrl).not.toContain('include_archived');
	});

	it('initialize() restores includeArchived from URL params', async () => {
		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async (input) => {
			const url = String(input);
			if (url.includes('/api/discover')) {
				return new Response(JSON.stringify(mockDiscoverResult()), { status: 200 });
			}
			return new Response(JSON.stringify(mockListResult()), { status: 200 });
		});

		const router = {
			lastUrl: null as string | null,
			navigate(url: string) { router.lastUrl = url; },
			getSearchParams() {
				const params = new URLSearchParams();
				params.set('include_archived', 'true');
				params.set('project', 'vestige');
				return params;
			},
		};

		const browser = createMemoryBrowser({ router });
		browser.initialize();

		expect(browser.filters.includeArchived).toBe(true);
		expect(browser.filters.projects).toEqual(['vestige']);

		await vi.waitFor(() => {
			expect(fetchSpy).toHaveBeenCalledTimes(2);
		});

		// Verify the API call includes include_archived
		const urls = fetchSpy.mock.calls.map(c => String(c[0]));
		const memoriesUrl = urls.find(u => u.includes('/api/memories'))!;
		expect(memoriesUrl).toContain('include_archived=true');
	});

	it('toggleIncludeArchived resets offset to 0', async () => {
		vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify(mockListResult()), { status: 200 })
		);
		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		browser.goToPage(40);
		expect(browser.offset).toBe(40);

		browser.toggleIncludeArchived();
		expect(browser.offset).toBe(0);
	});

	it('clearFilters resets includeArchived to false', async () => {
		vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify(mockListResult()), { status: 200 })
		);
		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		browser.toggleIncludeArchived();
		expect(browser.filters.includeArchived).toBe(true);

		browser.clearFilters();
		expect(browser.filters.includeArchived).toBe(false);
		expect(router.lastUrl).not.toContain('include_archived');
	});

	it('search mode passes include_archived to fetchSearch', async () => {
		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockImplementation(async (input) => {
			const url = String(input);
			if (url.includes('/api/memories/search')) {
				return new Response(JSON.stringify({ results: [], total: 0 }), { status: 200 });
			}
			return new Response(JSON.stringify(mockListResult()), { status: 200 });
		});
		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		// Enable include archived, then set a query to enter search mode
		browser.toggleIncludeArchived();
		browser.setQuery('rust patterns');

		await vi.waitFor(() => {
			const urls = fetchSpy.mock.calls.map(c => String(c[0]));
			const searchUrl = urls.find(u => u.includes('/api/memories/search'));
			expect(searchUrl).toBeDefined();
			expect(searchUrl).toContain('include_archived=true');
		});
	});

	it('refresh() re-fetches current view without changing filters or offset', async () => {
		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify(mockListResult({ total: 10 })), { status: 200 })
		);
		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		// Set some state first
		browser.selectFilter('project', 'vestige', false);
		browser.goToPage(20);
		await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalledTimes(2));

		// Verify state before refresh
		expect(browser.filters.projects).toEqual(['vestige']);
		expect(browser.offset).toBe(20);

		// Now refresh
		browser.refresh();
		await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalledTimes(3));

		// State should be unchanged
		expect(browser.filters.projects).toEqual(['vestige']);
		expect(browser.offset).toBe(20);

		// URL in the API call should match the previous state
		const lastUrl = fetchSpy.mock.calls[2][0] as string;
		expect(lastUrl).toContain('project=vestige');
		expect(lastUrl).toContain('offset=20');
	});

	it('error is set on fetch failure and cleared on success', async () => {
		const fetchSpy = vi.spyOn(globalThis, 'fetch')
			.mockResolvedValueOnce(new Response('Server Error', { status: 500 }))
			.mockResolvedValueOnce(new Response(JSON.stringify(mockListResult({ total: 3 })), { status: 200 }));

		const router = mockRouter();
		const browser = createMemoryBrowser({ router });

		// First call fails
		browser.selectFilter('project', 'vestige', false);
		await vi.waitFor(() => expect(browser.loading).toBe(false));
		expect(browser.listError).toContain('HTTP 500');

		// Second call succeeds and clears error
		browser.selectFilter('type', 'fact', true);
		await vi.waitFor(() => expect(browser.loading).toBe(false));
		expect(browser.listError).toBeNull();
		expect(browser.total).toBe(3);
	});
});
