import { authFetch, fetchDiscover, fetchSearch, type Memory, type ListResult, type DiscoverResult } from '$lib/api';

export type FilterKind = 'project' | 'type' | 'tag';

export interface FilterState {
	query: string;
	projects: string[];
	types: string[];
	tags: string[];
	includeArchived: boolean;
}

export interface RouterAdapter {
	navigate(url: string, options?: { replaceState?: boolean; noScroll?: boolean }): void;
	getSearchParams(): URLSearchParams;
}

export interface MemoryBrowserOptions {
	router?: RouterAdapter;
	limit?: number;
}

export interface MemoryBrowser {
	readonly filters: FilterState;
	readonly memories: Memory[];
	readonly total: number;
	readonly offset: number;
	readonly limit: number;
	readonly loading: boolean;
	readonly listError: string | null;
	readonly discover: DiscoverResult | null;
	readonly discoverError: string | null;
	readonly scores: Map<string, number>;

	selectFilter(kind: FilterKind, name: string, additive: boolean): void;
	removeFilter(kind: FilterKind, value: string): void;
	clearFilters(): void;
	goToPage(newOffset: number): void;
	setQuery(query: string): void;
	toggleIncludeArchived(): void;
	initialize(): void;
	syncFromUrl(): void;
	refresh(): void;
}

export function createMemoryBrowser(options?: MemoryBrowserOptions): MemoryBrowser {
	const router = options?.router;
	const pageLimit = options?.limit ?? 20;

	let filters = $state<FilterState>({
		query: '',
		projects: [],
		types: [],
		tags: [],
		includeArchived: false,
	});
	let memories = $state<Memory[]>([]);
	let total = $state(0);
	let offset = $state(0);
	let loading = $state(false);
	let listError = $state<string | null>(null);
	let discover = $state<DiscoverResult | null>(null);
	let discoverError = $state<string | null>(null);
	let scores = $state<Map<string, number>>(new Map());

	let fetchSeq = 0;

	function kindToList(kind: FilterKind): string[] {
		switch (kind) {
			case 'project': return filters.projects;
			case 'type': return filters.types;
			case 'tag': return filters.tags;
		}
	}

	function isSearchMode(): boolean {
		return filters.query.trim().length > 0;
	}

	function buildApiUrl(): string {
		const params = new URLSearchParams();
		if (isSearchMode()) {
			params.set('q', filters.query.trim());
		}
		for (const p of filters.projects) params.append('project', p);
		for (const t of filters.types) params.append('type', t);
		for (const t of filters.tags) params.append('tag', t);
		if (filters.includeArchived) params.set('include_archived', 'true');
		params.set('limit', String(pageLimit));
		if (offset !== 0) params.set('offset', String(offset));
		const base = isSearchMode() ? '/api/memories/search' : '/api/memories';
		const qs = params.toString();
		return qs ? `${base}?${qs}` : base;
	}

	function buildBrowserUrl(): string {
		const params = new URLSearchParams();
		if (filters.query.trim()) params.set('q', filters.query.trim());
		for (const p of filters.projects) params.append('project', p);
		for (const t of filters.types) params.append('type', t);
		for (const t of filters.tags) params.append('tag', t);
		if (filters.includeArchived) params.set('include_archived', 'true');
		if (offset !== 0) params.set('offset', String(offset));
		if (pageLimit !== 20) params.set('limit', String(pageLimit));
		const qs = params.toString();
		return qs ? `?${qs}` : '';
	}

	async function fetchMemories() {
		const seq = ++fetchSeq;
		loading = true;
		try {
			if (isSearchMode()) {
				const result = await fetchSearch({
					q: filters.query.trim(),
					projects: filters.projects.length ? filters.projects : undefined,
					types: filters.types.length ? filters.types : undefined,
					tags: filters.tags.length ? filters.tags : undefined,
					include_archived: filters.includeArchived || undefined,
					limit: pageLimit,
					offset: offset !== 0 ? offset : undefined,
				});
				if (seq !== fetchSeq) return;
				memories = result.results.map((h) => h.memory);
				total = result.total;
				const newScores = new Map<string, number>();
				for (const h of result.results) {
					newScores.set(h.memory.id, h.score);
				}
				scores = newScores;
			} else {
				const url = buildApiUrl();
				const res = await authFetch(url);
				if (seq !== fetchSeq) return;
				if (!res.ok) {
					const body = await res.text().catch(() => '(no body)');
					throw new Error(`HTTP ${res.status}: ${body}`);
				}
				const result: ListResult = await res.json();
				if (seq !== fetchSeq) return;
				memories = result.memories;
				total = result.total;
				scores = new Map();
			}
			listError = null;
		} catch (e) {
			if (seq !== fetchSeq) return;
			listError = e instanceof Error ? e.message : String(e);
		} finally {
			if (seq === fetchSeq) loading = false;
		}
	}

	function updateUrl() {
		if (router) {
			const url = buildBrowserUrl();
			router.navigate(url, { replaceState: false, noScroll: true });
		}
	}

	function selectFilter(kind: FilterKind, name: string, additive: boolean) {
		const list = kindToList(kind);
		if (additive) {
			const idx = list.indexOf(name);
			if (idx !== -1) {
				list.splice(idx, 1);
			} else {
				list.push(name);
			}
		} else {
			if (list.length === 1 && list[0] === name) {
				list.length = 0;
			} else {
				list.length = 0;
				list.push(name);
			}
		}
		offset = 0;
		updateUrl();
		fetchMemories();
	}

	function removeFilter(kind: FilterKind, value: string) {
		const list = kindToList(kind);
		const idx = list.indexOf(value);
		if (idx !== -1) {
			list.splice(idx, 1);
		}
		offset = 0;
		updateUrl();
		fetchMemories();
	}

	function clearFilters() {
		filters.query = '';
		filters.projects.length = 0;
		filters.types.length = 0;
		filters.tags.length = 0;
		filters.includeArchived = false;
		offset = 0;
		updateUrl();
		fetchMemories();
	}

	function setQuery(query: string) {
		filters.query = query;
		offset = 0;
		updateUrl();
		fetchMemories();
	}

	function toggleIncludeArchived() {
		filters.includeArchived = !filters.includeArchived;
		offset = 0;
		updateUrl();
		fetchMemories();
	}

	function goToPage(newOffset: number) {
		offset = newOffset;
		updateUrl();
		fetchMemories();
	}

	function syncFromUrl() {
		if (!router) return;
		const params = router.getSearchParams();
		filters.query = params.get('q') ?? '';
		filters.projects = params.getAll('project');
		filters.types = params.getAll('type');
		filters.tags = params.getAll('tag');
		filters.includeArchived = params.get('include_archived') === 'true';
		offset = parseInt(params.get('offset') ?? '0', 10) || 0;
		fetchMemories();
	}

	function initialize() {
		syncFromUrl();
		fetchDiscoverData();
	}

	async function fetchDiscoverData() {
		try {
			discover = await fetchDiscover();
			discoverError = null;
		} catch (e) {
			discoverError = e instanceof Error ? e.message : String(e);
		}
	}

	return {
		get filters() { return filters; },
		get memories() { return memories; },
		get total() { return total; },
		get offset() { return offset; },
		get limit() { return pageLimit; },
		get loading() { return loading; },
		get listError() { return listError; },
		get discover() { return discover; },
		get discoverError() { return discoverError; },
		get scores() { return scores; },
		selectFilter,
		removeFilter,
		clearFilters,
		goToPage,
		setQuery,
		toggleIncludeArchived,
		initialize,
		syncFromUrl,
		refresh() {
			fetchMemories();
		},
	};
}
