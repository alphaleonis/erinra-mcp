import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { fetchDiscover, fetchMemory, fetchSearch, archiveMemory, unarchiveMemory, bulkArchiveMemories, bulkUnarchiveMemories, setAuthToken, initAuthFromUrl } from './api';

describe('API client', () => {
	beforeEach(() => {
		vi.restoreAllMocks();
		setAuthToken('');
	});

	it('fetches discover data', async () => {
		const mockDiscover = {
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

		vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify(mockDiscover), { status: 200 })
		);

		const result = await fetchDiscover();
		expect(result.projects[0].name).toBe('vestige');
		expect(result.stats.total_memories).toBe(30);
	});

	it('fetches a single memory by ID', async () => {
		const mockDetail = {
			memory: {
				id: 'abc-123',
				content: 'Some memory content',
				memory_type: 'fact',
				projects: ['vestige'],
				tags: ['rust'],
				created_at: '2025-01-01T00:00:00Z',
				updated_at: '2025-01-01T00:00:00Z',
				archived_at: null,
				last_accessed_at: '2025-01-02T00:00:00Z',
				access_count: 5,
				truncated: false,
			},
			outgoing_links: [],
			incoming_links: [],
		};

		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify(mockDetail), { status: 200 })
		);

		const result = await fetchMemory('abc-123');

		expect(fetchSpy).toHaveBeenCalledOnce();
		const calledUrl = fetchSpy.mock.calls[0][0] as string;
		expect(calledUrl).toBe('/api/memories/abc-123');

		expect(result.memory.id).toBe('abc-123');
		expect(result.outgoing_links).toEqual([]);
		expect(result.incoming_links).toEqual([]);
	});

	it('throws on HTTP error when fetching single memory', async () => {
		vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response('Not Found', { status: 404 })
		);

		await expect(fetchMemory('nonexistent')).rejects.toThrow('HTTP 404');
	});

	it('fetchSearch constructs correct URL with query and filters', async () => {
		const mockSearchResult = {
			results: [
				{
					memory: {
						id: 'mem-1',
						content: 'Rust error handling',
						memory_type: 'fact',
						projects: ['erinra'],
						tags: ['rust'],
						created_at: '2025-01-01T00:00:00Z',
						updated_at: '2025-01-01T00:00:00Z',
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

		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify(mockSearchResult), { status: 200 })
		);

		const result = await fetchSearch({
			q: 'error handling',
			projects: ['erinra'],
			tags: ['rust'],
			limit: 10,
			offset: 5,
		});

		expect(fetchSpy).toHaveBeenCalledOnce();
		const calledUrl = fetchSpy.mock.calls[0][0] as string;
		expect(calledUrl).toContain('/api/memories/search');
		expect(calledUrl).toContain('q=error+handling');
		expect(calledUrl).toContain('project=erinra');
		expect(calledUrl).toContain('tag=rust');
		expect(calledUrl).toContain('limit=10');
		expect(calledUrl).toContain('offset=5');

		expect(result.results).toHaveLength(1);
		expect(result.results[0].score).toBe(0.85);
		expect(result.results[0].memory.id).toBe('mem-1');
		expect(result.total).toBe(1);
	});

	it('fetchSearch with minimal params only sends q', async () => {
		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify({ results: [], total: 0 }), { status: 200 })
		);

		await fetchSearch({ q: 'test' });

		const calledUrl = fetchSpy.mock.calls[0][0] as string;
		expect(calledUrl).toBe('/api/memories/search?q=test');
	});

	it('fetchSearch throws on HTTP error', async () => {
		vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response('Bad Request', { status: 400 })
		);

		await expect(fetchSearch({ q: '' })).rejects.toThrow('HTTP 400');
	});

	it('archiveMemory POSTs to /api/memories/:id/archive', async () => {
		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify({ id: 'abc-123', archived_at: '2025-06-01T00:00:00Z' }), { status: 200 })
		);

		await archiveMemory('abc-123');

		expect(fetchSpy).toHaveBeenCalledOnce();
		const [url, init] = fetchSpy.mock.calls[0];
		expect(url).toBe('/api/memories/abc-123/archive');
		expect((init as RequestInit).method).toBe('POST');
	});

	it('archiveMemory throws on HTTP error', async () => {
		vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response('Not Found', { status: 404 })
		);

		await expect(archiveMemory('nonexistent')).rejects.toThrow('HTTP 404');
	});

	it('unarchiveMemory POSTs to /api/memories/:id/unarchive', async () => {
		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify({ id: 'abc-123' }), { status: 200 })
		);

		await unarchiveMemory('abc-123');

		expect(fetchSpy).toHaveBeenCalledOnce();
		const [url, init] = fetchSpy.mock.calls[0];
		expect(url).toBe('/api/memories/abc-123/unarchive');
		expect((init as RequestInit).method).toBe('POST');
	});

	it('unarchiveMemory throws on HTTP error', async () => {
		vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response('Conflict', { status: 409 })
		);

		await expect(unarchiveMemory('abc-123')).rejects.toThrow('HTTP 409');
	});

	it('bulkArchiveMemories POSTs ids to /api/memories/bulk/archive', async () => {
		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify([]), { status: 200 })
		);

		await bulkArchiveMemories(['id-1', 'id-2', 'id-3']);

		expect(fetchSpy).toHaveBeenCalledOnce();
		const [url, init] = fetchSpy.mock.calls[0];
		expect(url).toBe('/api/memories/bulk/archive');
		expect((init as RequestInit).method).toBe('POST');
		expect((init as RequestInit).headers).toEqual({ 'Content-Type': 'application/json' });
		expect(JSON.parse((init as RequestInit).body as string)).toEqual({ ids: ['id-1', 'id-2', 'id-3'] });
	});

	it('bulkArchiveMemories throws on HTTP error', async () => {
		vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response('Bad Request', { status: 400 })
		);

		await expect(bulkArchiveMemories(['id-1'])).rejects.toThrow('HTTP 400');
	});

	it('bulkUnarchiveMemories POSTs ids to /api/memories/bulk/unarchive', async () => {
		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify([]), { status: 200 })
		);

		await bulkUnarchiveMemories(['id-a', 'id-b']);

		expect(fetchSpy).toHaveBeenCalledOnce();
		const [url, init] = fetchSpy.mock.calls[0];
		expect(url).toBe('/api/memories/bulk/unarchive');
		expect((init as RequestInit).method).toBe('POST');
		expect((init as RequestInit).headers).toEqual({ 'Content-Type': 'application/json' });
		expect(JSON.parse((init as RequestInit).body as string)).toEqual({ ids: ['id-a', 'id-b'] });
	});

	it('bulkUnarchiveMemories throws on HTTP error', async () => {
		vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response('Server Error', { status: 500 })
		);

		await expect(bulkUnarchiveMemories(['id-1'])).rejects.toThrow('HTTP 500');
	});

	it('sends Authorization header when auth token is set', async () => {
		setAuthToken('my-secret-token');

		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify({
				projects: [], types: [], tags: [], relations: [],
				stats: { total_memories: 0, total_archived: 0, storage_size_bytes: 0, embedding_model: 'test' },
			}), { status: 200 })
		);

		await fetchDiscover();

		expect(fetchSpy).toHaveBeenCalledOnce();
		const [, init] = fetchSpy.mock.calls[0];
		const headers = (init as RequestInit)?.headers as Record<string, string> | undefined;
		expect(headers?.['Authorization']).toBe('Bearer my-secret-token');
	});

	it('does not send Authorization header when no auth token is set', async () => {
		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify({
				projects: [], types: [], tags: [], relations: [],
				stats: { total_memories: 0, total_archived: 0, storage_size_bytes: 0, embedding_model: 'test' },
			}), { status: 200 })
		);

		await fetchDiscover();

		expect(fetchSpy).toHaveBeenCalledOnce();
		const [, init] = fetchSpy.mock.calls[0];
		const headers = (init as RequestInit)?.headers as Record<string, string> | undefined;
		expect(headers?.['Authorization']).toBeUndefined();
	});

	it('sends Authorization header on POST requests with existing headers', async () => {
		setAuthToken('post-token');

		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify([]), { status: 200 })
		);

		await bulkArchiveMemories(['id-1']);

		expect(fetchSpy).toHaveBeenCalledOnce();
		const [, init] = fetchSpy.mock.calls[0];
		const headers = (init as RequestInit)?.headers as Record<string, string> | undefined;
		expect(headers?.['Authorization']).toBe('Bearer post-token');
		expect(headers?.['Content-Type']).toBe('application/json');
	});
});

describe('initAuthFromUrl', () => {
	let originalLocation: Location;
	let mockSessionStorage: Record<string, string>;
	let replaceStateSpy: ReturnType<typeof vi.fn>;

	beforeEach(() => {
		vi.restoreAllMocks();
		setAuthToken('');

		mockSessionStorage = {};
		vi.stubGlobal('sessionStorage', {
			getItem: vi.fn((key: string) => mockSessionStorage[key] ?? null),
			setItem: vi.fn((key: string, value: string) => { mockSessionStorage[key] = value; }),
			removeItem: vi.fn((key: string) => { delete mockSessionStorage[key]; }),
		});

		replaceStateSpy = vi.fn();
		originalLocation = window.location;
	});

	afterEach(() => {
		vi.unstubAllGlobals();
	});

	function setWindowLocation(url: string) {
		const parsed = new URL(url, 'http://localhost');
		Object.defineProperty(window, 'location', {
			value: {
				search: parsed.search,
				pathname: parsed.pathname,
				href: parsed.href,
			},
			writable: true,
			configurable: true,
		});
		Object.defineProperty(window, 'history', {
			value: { replaceState: replaceStateSpy },
			writable: true,
			configurable: true,
		});
	}

	it('reads token from URL, stores in sessionStorage, and strips from URL', async () => {
		setWindowLocation('http://localhost:3000/?token=abc123');

		initAuthFromUrl();

		// Token should be stored in sessionStorage
		expect(sessionStorage.setItem).toHaveBeenCalledWith('erinra_auth_token', 'abc123');
		// Token should be stripped from URL
		expect(replaceStateSpy).toHaveBeenCalledWith({}, '', '/');
	});

	it('reads token from sessionStorage when no URL param', async () => {
		mockSessionStorage['erinra_auth_token'] = 'stored-token';
		setWindowLocation('http://localhost:3000/');

		initAuthFromUrl();

		// Should read from sessionStorage
		expect(sessionStorage.getItem).toHaveBeenCalledWith('erinra_auth_token');
		// Should NOT call replaceState (no URL change needed)
		expect(replaceStateSpy).not.toHaveBeenCalled();
	});

	it('fetches use token from sessionStorage after initAuthFromUrl', async () => {
		mockSessionStorage['erinra_auth_token'] = 'persisted-token';
		setWindowLocation('http://localhost:3000/');

		initAuthFromUrl();

		const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify({
				projects: [], types: [], tags: [], relations: [],
				stats: { total_memories: 0, total_archived: 0, storage_size_bytes: 0, embedding_model: 'test' },
			}), { status: 200 })
		);

		await fetchDiscover();

		const [, init] = fetchSpy.mock.calls[0];
		const headers = (init as RequestInit)?.headers as Record<string, string> | undefined;
		expect(headers?.['Authorization']).toBe('Bearer persisted-token');
	});

	it('preserves other URL query params when stripping token', async () => {
		setWindowLocation('http://localhost:3000/?foo=bar&token=abc123&baz=qux');

		initAuthFromUrl();

		// Token should be stripped but other params preserved
		expect(replaceStateSpy).toHaveBeenCalledWith({}, '', '/?foo=bar&baz=qux');
	});
});
