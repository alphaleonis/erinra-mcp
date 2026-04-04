<script lang="ts">
	import Sidebar from '$lib/components/Sidebar.svelte';
	import FilterBar from '$lib/components/FilterBar.svelte';
	import MemoryList from '$lib/components/MemoryList.svelte';
	import Pagination from '$lib/components/Pagination.svelte';
	import ConfirmDialog from '$lib/components/ConfirmDialog.svelte';
	import { createMemoryBrowser } from '$lib/stores/memory-browser.svelte';
	import { archiveMemory, unarchiveMemory, bulkArchiveMemories, bulkUnarchiveMemories } from '$lib/api';
	import * as Tooltip from '$lib/components/ui/tooltip';
	import ArchiveIcon from '@lucide/svelte/icons/archive';
	import ListChecksIcon from '@lucide/svelte/icons/list-checks';
	import XIcon from '@lucide/svelte/icons/x';
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/stores';

	let sidebarOpen: boolean = $state(false);
	let searchInput: string = $state('');
	let confirmOpen: boolean = $state(false);
	let pendingAction: { id: string; action: 'archive' | 'unarchive' } | null = $state(null);
	let listActionError: string | null = $state(null);

	let selectionMode: boolean = $state(false);
	let selectedIds: Set<string> = $state(new Set());
	let bulkConfirmOpen: boolean = $state(false);
	let pendingBulkAction: 'archive' | 'unarchive' | null = $state(null);

	function handleAction(id: string, action: 'archive' | 'unarchive') {
		pendingAction = { id, action };
		confirmOpen = true;
	}

	async function handleConfirmAction() {
		if (!pendingAction) return;
		const { id, action } = pendingAction;
		confirmOpen = false;
		pendingAction = null;
		listActionError = null;
		try {
			if (action === 'archive') {
				await archiveMemory(id);
			} else {
				await unarchiveMemory(id);
			}
			browser.refresh();
		} catch (e) {
			listActionError = e instanceof Error ? e.message : String(e);
		}
	}

	function handleCancelAction() {
		confirmOpen = false;
		pendingAction = null;
	}

	function handleSelectionChange(id: string) {
		const next = new Set(selectedIds);
		if (next.has(id)) {
			next.delete(id);
		} else {
			next.add(id);
		}
		selectedIds = next;
	}

	function handleSelectAll() {
		const allSelected = browser.memories.every((m) => selectedIds.has(m.id));
		if (allSelected) {
			selectedIds = new Set();
		} else {
			selectedIds = new Set(browser.memories.map((m) => m.id));
		}
	}

	function handleBulkAction(action: 'archive' | 'unarchive') {
		pendingBulkAction = action;
		bulkConfirmOpen = true;
	}

	async function handleConfirmBulkAction() {
		if (!pendingBulkAction || selectedIds.size === 0) return;
		const action = pendingBulkAction;
		const ids = [...selectedIds].filter(id => {
			const m = browser.memories.find(mem => mem.id === id);
			return action === 'archive' ? !m?.archived_at : !!m?.archived_at;
		});
		bulkConfirmOpen = false;
		pendingBulkAction = null;
		listActionError = null;
		if (ids.length === 0) return;
		try {
			if (action === 'archive') {
				await bulkArchiveMemories(ids);
			} else {
				await bulkUnarchiveMemories(ids);
			}
			selectedIds = new Set();
			selectionMode = false;
			browser.refresh();
		} catch (e) {
			listActionError = e instanceof Error ? e.message : String(e);
			browser.refresh();
		}
	}

	function handleCancelBulkAction() {
		bulkConfirmOpen = false;
		pendingBulkAction = null;
	}

	const browser = createMemoryBrowser({
		router: {
			navigate: (url, opts) => goto(url || '/', { replaceState: opts?.replaceState ?? false, noScroll: opts?.noScroll ?? true }),
			getSearchParams: () => $page.url.searchParams,
		},
	});

	// Clear selection when memories change (page navigation, filter change)
	let prevMemoryIds: string = $derived(browser.memories.map((m) => m.id).join(','));
	$effect(() => {
		prevMemoryIds; // track dependency
		selectedIds = new Set();
	});

	onMount(() => {
		browser.initialize();
		searchInput = browser.filters.query;

		// Re-sync state from URL on back/forward navigation
		function onPopState() {
			browser.syncFromUrl();
			searchInput = browser.filters.query;
		}
		window.addEventListener('popstate', onPopState);
		return () => window.removeEventListener('popstate', onPopState);
	});
</script>

<div class="flex h-screen flex-col bg-gray-950 text-gray-100">
	<!-- Header -->
	<header class="flex shrink-0 items-center border-b border-gray-800 px-4 py-3">
		<button
			type="button"
			class="mr-3 rounded-md p-1.5 text-gray-400 hover:bg-gray-800 hover:text-gray-200 md:hidden"
			onclick={() => (sidebarOpen = !sidebarOpen)}
			aria-label="Toggle sidebar"
		>
			<svg class="h-5 w-5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
				{#if sidebarOpen}
					<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
				{:else}
					<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 6h16M4 12h16M4 18h16" />
				{/if}
			</svg>
		</button>
		<h1 class="mr-4 text-lg font-semibold">Erinra</h1>
		<form
			class="flex-1 max-w-md"
			onsubmit={(e: SubmitEvent) => { e.preventDefault(); browser.setQuery(searchInput); }}
		>
			<input
				type="search"
				bind:value={searchInput}
				placeholder="Search memories..."
				class="w-full rounded-lg border border-gray-700 bg-gray-900 px-3 py-1.5 text-sm text-gray-200 placeholder-gray-500 focus:border-blue-500 focus:outline-none"
				data-testid="search-input"
			/>
		</form>
		<div class="ml-3 flex items-center gap-1">
			<Tooltip.Root>
				<Tooltip.Trigger
					class="rounded-md p-1.5 transition-colors {browser.filters.includeArchived ? 'bg-gray-700 text-gray-200' : 'text-gray-400 hover:bg-gray-800 hover:text-gray-200'}"
					onclick={() => browser.toggleIncludeArchived()}
					data-testid="toggle-archived"
				>
					<ArchiveIcon class="size-4" />
				</Tooltip.Trigger>
				<Tooltip.Portal>
					<Tooltip.Content>
						{browser.filters.includeArchived ? 'Hide archived' : 'Include archived'}
					</Tooltip.Content>
				</Tooltip.Portal>
			</Tooltip.Root>
			<Tooltip.Root>
				<Tooltip.Trigger
					class="rounded-md p-1.5 transition-colors {selectionMode ? 'bg-gray-700 text-gray-200' : 'text-gray-400 hover:bg-gray-800 hover:text-gray-200'}"
					onclick={() => {
						selectionMode = !selectionMode;
						if (!selectionMode) selectedIds = new Set();
					}}
					data-testid="toggle-selection"
				>
					{#if selectionMode}
						<XIcon class="size-4" />
					{:else}
						<ListChecksIcon class="size-4" />
					{/if}
				</Tooltip.Trigger>
				<Tooltip.Portal>
					<Tooltip.Content>
						{selectionMode ? 'Cancel selection' : 'Select memories'}
					</Tooltip.Content>
				</Tooltip.Portal>
			</Tooltip.Root>
		</div>
	</header>

	{#if browser.discoverError}
		<div class="p-4">
			<div class="rounded-lg border border-red-800 bg-red-950 p-4 text-red-200">
				<p class="font-medium">Failed to load data</p>
				<p class="text-sm mt-1">{browser.discoverError}</p>
			</div>
		</div>
	{:else if browser.discover}
		<div class="flex min-h-0 flex-1">
			<!-- Sidebar: mobile overlay -->
			{#if sidebarOpen}
				<div class="md:hidden">
					<!-- backdrop -->
					<button
						type="button"
						class="fixed inset-0 z-20 bg-black/50"
						onclick={() => (sidebarOpen = false)}
						aria-label="Close sidebar"
					></button>
					<!-- panel -->
					<div class="fixed inset-y-0 left-0 z-30 w-64 border-r border-gray-800 bg-gray-950 p-4 pt-16">
						<Sidebar
							projects={browser.discover.projects}
							types={browser.discover.types}
							selectedProjects={browser.filters.projects}
							selectedTypes={browser.filters.types}
							onFilterSelect={browser.selectFilter}
						/>
					</div>
				</div>
			{/if}

			<!-- Sidebar: desktop -->
			<div class="hidden w-60 shrink-0 border-r border-gray-800 p-4 md:block">
				<Sidebar
					projects={browser.discover.projects}
					types={browser.discover.types}
					selectedProjects={browser.filters.projects}
					selectedTypes={browser.filters.types}
					onFilterSelect={browser.selectFilter}
				/>
			</div>

			<!-- Content area -->
			<main class="flex-1 overflow-y-auto p-6">
					{#if browser.filters.projects.length > 0 || browser.filters.types.length > 0 || browser.filters.tags.length > 0}
					<div class="mb-4">
						<FilterBar
							filters={browser.filters}
							onRemove={browser.removeFilter}
							onClear={browser.clearFilters}
						/>
					</div>
					{/if}

				{#if browser.listError}
					<div class="mb-4 rounded-lg border border-red-800 bg-red-950 p-3 text-sm text-red-200">
						Failed to load memories: {browser.listError}
					</div>
				{/if}

				{#if listActionError}
					<div class="mb-4 rounded-lg border border-red-800 bg-red-950 p-3 text-sm text-red-200">
						Action failed: {listActionError}
					</div>
				{/if}

				{#if selectedIds.size > 0}
					<div class="mb-4 flex items-center gap-3 rounded-lg border border-gray-700 bg-gray-900 px-4 py-2" data-testid="bulk-action-bar">
						<span class="text-sm text-gray-300">{selectedIds.size} selected</span>
						{#if browser.memories.some((m) => selectedIds.has(m.id) && !m.archived_at)}
							<button
								type="button"
								class="rounded-md bg-gray-700 px-3 py-1 text-sm font-medium text-gray-200 transition-colors hover:bg-gray-600"
								onclick={() => handleBulkAction('archive')}
								data-testid="bulk-archive-btn"
							>
								Archive
							</button>
						{/if}
						{#if browser.memories.some((m) => selectedIds.has(m.id) && m.archived_at)}
							<button
								type="button"
								class="rounded-md bg-gray-700 px-3 py-1 text-sm font-medium text-gray-200 transition-colors hover:bg-gray-600"
								onclick={() => handleBulkAction('unarchive')}
								data-testid="bulk-unarchive-btn"
							>
								Unarchive
							</button>
						{/if}
					</div>
				{/if}

				<MemoryList
					memories={browser.memories}
					total={browser.total}
					loading={browser.loading}
					scores={browser.scores}
					onAction={handleAction}
					selectable={selectionMode}
					{selectedIds}
					onSelectionChange={handleSelectionChange}
					onSelectAll={handleSelectAll}
				/>

				<div class="mt-4">
					<Pagination
						total={browser.total}
						offset={browser.offset}
						limit={browser.limit}
						onPageChange={browser.goToPage}
					/>
				</div>
			</main>
		</div>
	{:else}
		<div class="flex flex-1 items-center justify-center">
			<p class="text-gray-400">Loading...</p>
		</div>
	{/if}
</div>

{#if pendingAction}
	<ConfirmDialog
		bind:open={confirmOpen}
		title={(pendingAction.action === 'archive' ? 'Archive' : 'Unarchive') + ' Memory'}
		description={pendingAction.action === 'archive'
			? 'This memory will be archived. You can unarchive it later.'
			: 'This memory will be restored to your active memories.'}
		confirmLabel={pendingAction.action === 'archive' ? 'Archive' : 'Unarchive'}
		onconfirm={handleConfirmAction}
		oncancel={handleCancelAction}
	/>
{/if}

{#if pendingBulkAction}
	<ConfirmDialog
		bind:open={bulkConfirmOpen}
		title={(pendingBulkAction === 'archive' ? 'Archive' : 'Unarchive') + ' ' + selectedIds.size + ' Memories'}
		description={pendingBulkAction === 'archive'
			? `${selectedIds.size} memories will be archived. You can unarchive them later.`
			: `${selectedIds.size} memories will be restored to your active memories.`}
		confirmLabel={pendingBulkAction === 'archive' ? 'Archive' : 'Unarchive'}
		onconfirm={handleConfirmBulkAction}
		oncancel={handleCancelBulkAction}
	/>
{/if}
