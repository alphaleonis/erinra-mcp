// --- Auth token management ---

let authToken = '';
const STORAGE_KEY = 'erinra_auth_token';

/** Set the Bearer token used for all API requests. */
export function setAuthToken(token: string): void {
	authToken = token;
}

/** Initialize auth token from URL query parameter (call once on app startup).
 *  Stores the token in sessionStorage so it survives page refreshes,
 *  and strips it from the URL to avoid leaking in browser history. */
export function initAuthFromUrl(): void {
	if (typeof window !== 'undefined') {
		const params = new URLSearchParams(window.location.search);
		const token = params.get('token');
		if (token) {
			authToken = token;
			sessionStorage.setItem(STORAGE_KEY, token);
			// Strip token from URL to avoid leaking in history/address bar
			params.delete('token');
			const clean = params.toString();
			const newUrl = window.location.pathname + (clean ? `?${clean}` : '');
			window.history.replaceState({}, '', newUrl);
		} else {
			authToken = sessionStorage.getItem(STORAGE_KEY) ?? '';
		}
	}
}

/** Build fetch init with auth header injected if token is set. */
export function authFetch(url: string, init?: RequestInit): Promise<Response> {
	if (authToken) {
		const headers: Record<string, string> = {
			...(init?.headers as Record<string, string> ?? {}),
			'Authorization': `Bearer ${authToken}`,
		};
		return fetch(url, { ...init, headers });
	}
	return fetch(url, init);
}

export interface Memory {
	id: string;
	content: string;
	memory_type: string | null;
	projects: string[];
	tags: string[];
	created_at: string;
	updated_at: string;
	archived_at: string | null;
	last_accessed_at: string | null;
	access_count: number;
	truncated: boolean;
}

export interface MemoryLink {
	id: string;
	source_id: string;
	target_id: string;
	relation: string;
	created_at: string;
	content: string | null;
}

export interface MemoryDetail {
	memory: Memory;
	outgoing_links: MemoryLink[];
	incoming_links: MemoryLink[];
}

export interface ListResult {
	memories: Memory[];
	total: number;
}

export interface NameCount {
	name: string;
	count: number;
}

export interface DiscoverResult {
	projects: NameCount[];
	types: NameCount[];
	tags: NameCount[];
	relations: NameCount[];
	stats: {
		total_memories: number;
		total_archived: number;
		storage_size_bytes: number;
		embedding_model: string;
	};
}

export interface SearchHit {
	memory: Memory;
	outgoing_links: MemoryLink[];
	incoming_links: MemoryLink[];
	score: number;
}

export interface SearchResult {
	results: SearchHit[];
	total: number;
}

export interface SearchParams {
	q: string;
	projects?: string[];
	types?: string[];
	tags?: string[];
	include_archived?: boolean;
	include_global?: boolean;
	limit?: number;
	offset?: number;
}

export async function fetchSearch(params: SearchParams): Promise<SearchResult> {
	const urlParams = new URLSearchParams();
	urlParams.set('q', params.q);
	if (params.projects) {
		for (const p of params.projects) urlParams.append('project', p);
	}
	if (params.types) {
		for (const t of params.types) urlParams.append('type', t);
	}
	if (params.tags) {
		for (const t of params.tags) urlParams.append('tag', t);
	}
	if (params.include_archived) urlParams.set('include_archived', 'true');
	if (params.include_global === false) urlParams.set('include_global', 'false');
	if (params.limit !== undefined) urlParams.set('limit', String(params.limit));
	if (params.offset !== undefined && params.offset !== 0) urlParams.set('offset', String(params.offset));
	const res = await authFetch(`/api/memories/search?${urlParams}`);
	if (!res.ok) {
		const body = await res.text().catch(() => '(no body)');
		throw new Error(`HTTP ${res.status}: ${body}`);
	}
	return res.json();
}

export async function fetchDiscover(): Promise<DiscoverResult> {
	const res = await authFetch('/api/discover');
	if (!res.ok) {
		const body = await res.text().catch(() => '(no body)');
		throw new Error(`HTTP ${res.status}: ${body}`);
	}
	return res.json();
}

export async function fetchMemory(id: string): Promise<MemoryDetail> {
	const res = await authFetch(`/api/memories/${encodeURIComponent(id)}`);
	if (!res.ok) {
		const body = await res.text().catch(() => '(no body)');
		throw new Error(`HTTP ${res.status}: ${body}`);
	}
	return res.json();
}

export async function archiveMemory(id: string): Promise<void> {
	const res = await authFetch(`/api/memories/${encodeURIComponent(id)}/archive`, { method: 'POST' });
	if (!res.ok) {
		const body = await res.text().catch(() => '(no body)');
		throw new Error(`HTTP ${res.status}: ${body}`);
	}
}

export async function unarchiveMemory(id: string): Promise<void> {
	const res = await authFetch(`/api/memories/${encodeURIComponent(id)}/unarchive`, { method: 'POST' });
	if (!res.ok) {
		const body = await res.text().catch(() => '(no body)');
		throw new Error(`HTTP ${res.status}: ${body}`);
	}
}

export async function bulkArchiveMemories(ids: string[]): Promise<void> {
	if (ids.length === 0) return;
	const res = await authFetch('/api/memories/bulk/archive', {
		method: 'POST',
		headers: { 'Content-Type': 'application/json' },
		body: JSON.stringify({ ids }),
	});
	if (!res.ok) {
		const body = await res.text().catch(() => '(no body)');
		throw new Error(`HTTP ${res.status}: ${body}`);
	}
}

export async function bulkUnarchiveMemories(ids: string[]): Promise<void> {
	if (ids.length === 0) return;
	const res = await authFetch('/api/memories/bulk/unarchive', {
		method: 'POST',
		headers: { 'Content-Type': 'application/json' },
		body: JSON.stringify({ ids }),
	});
	if (!res.ok) {
		const body = await res.text().catch(() => '(no body)');
		throw new Error(`HTTP ${res.status}: ${body}`);
	}
}
