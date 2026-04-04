<script lang="ts">
	import { page } from '$app/stores';
	import { goto } from '$app/navigation';
	import { fetchMemory, archiveMemory, unarchiveMemory, type MemoryDetail } from '$lib/api';
	import { renderMarkdown } from '$lib/markdown';
	import ConfirmDialog from '$lib/components/ConfirmDialog.svelte';
	import * as Tooltip from '$lib/components/ui/tooltip';
	import CopyIcon from '@lucide/svelte/icons/copy';
	import CheckIcon from '@lucide/svelte/icons/check';
	import 'highlight.js/styles/github-dark.css';

	let copied = $state(false);
	function copyId(id: string) {
		navigator.clipboard.writeText(id);
		copied = true;
		setTimeout(() => { copied = false; }, 1500);
	}

	let detail: MemoryDetail | null = $state(null);
	let error: string | null = $state(null);
	let loading: boolean = $state(true);
	let confirmOpen: boolean = $state(false);
	let actionLoading: boolean = $state(false);
	let actionError: string | null = $state(null);

	function formatDate(iso: string): string {
		const d = new Date(iso);
		return d.toLocaleDateString(undefined, {
			year: 'numeric',
			month: 'short',
			day: 'numeric',
			hour: '2-digit',
			minute: '2-digit',
		});
	}

	function truncateId(id: string): string {
		return id.slice(0, 8);
	}

	function navigateToFilter(kind: string, value: string) {
		goto(`/?${kind}=${encodeURIComponent(value)}`);
	}

	async function loadMemory(id: string) {
		loading = true;
		detail = null;
		error = null;
		actionError = null;
		try {
			detail = await fetchMemory(id);
			error = null;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			loading = false;
		}
	}

	async function handleConfirmAction() {
		if (!detail) return;
		const memory = detail.memory;
		const id = memory.id;
		const isArchived = !!memory.archived_at;

		confirmOpen = false;
		actionLoading = true;
		actionError = null;
		try {
			if (isArchived) {
				await unarchiveMemory(id);
			} else {
				await archiveMemory(id);
			}
			loadMemory(id);
		} catch (e) {
			actionError = e instanceof Error ? e.message : String(e);
		} finally {
			actionLoading = false;
		}
	}

	$effect(() => {
		const id = $page.params.id as string;
		loadMemory(id);
	});
</script>

<div class="min-h-screen bg-gray-950 text-gray-100">
	<!-- Header -->
	<header class="border-b border-gray-800 px-4 py-3">
		<div class="mx-auto flex max-w-4xl items-center gap-3">
			<a
				href="/"
				class="flex items-center gap-1.5 rounded-md px-2 py-1 text-sm text-gray-400 transition-colors hover:bg-gray-800 hover:text-gray-200"
			>
				<svg class="h-4 w-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
					<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15 19l-7-7 7-7" />
				</svg>
				Back to list
			</a>
			<h1 class="flex-1 text-lg font-semibold">Erinra</h1>
			{#if detail && !loading}
				<button
					type="button"
					class="rounded-md px-3 py-1 text-sm text-gray-400 transition-colors hover:bg-gray-800 hover:text-gray-200 disabled:opacity-50"
					disabled={actionLoading}
					onclick={() => { actionError = null; confirmOpen = true; }}
				>
					{detail.memory.archived_at ? 'Unarchive' : 'Archive'}
				</button>
			{/if}
		</div>
	</header>

	<main class="mx-auto max-w-4xl px-4 py-6">
		{#if loading}
			<div class="flex items-center justify-center py-24">
				<p class="text-gray-400">Loading memory...</p>
			</div>
		{:else if error}
			<div class="rounded-lg border border-red-800 bg-red-950 p-4 text-red-200">
				<p class="font-medium">Failed to load memory</p>
				<p class="mt-1 text-sm">{error}</p>
			</div>
		{:else if detail}
			{@const memory = detail.memory}

			<!-- Archived banner -->
			{#if memory.archived_at}
				<div class="mb-4 rounded-lg border border-amber-700 bg-amber-950 px-4 py-2.5 text-amber-200">
					<span class="font-medium">Archived</span>
					<span class="ml-1 text-sm text-amber-300">on {formatDate(memory.archived_at)}</span>
				</div>
			{/if}

			<!-- Action error -->
			{#if actionError}
				<div class="mb-4 rounded-lg border border-red-800 bg-red-950 px-4 py-2.5 text-red-200">
					<p class="text-sm">{actionError}</p>
				</div>
			{/if}

			<!-- Metadata section -->
			<section class="mb-6">
				<div class="flex flex-wrap items-center gap-2">
					{#if memory.memory_type}
						<button
							type="button"
							class="rounded-md bg-gray-800 px-2.5 py-1 text-sm font-medium text-gray-300 transition-colors hover:bg-gray-700 hover:text-gray-100"
							onclick={() => navigateToFilter('type', memory.memory_type!)}
						>
							{memory.memory_type}
						</button>
					{/if}
					{#each memory.projects as project}
						<button
							type="button"
							class="rounded-md bg-blue-950 px-2.5 py-1 text-sm font-medium text-blue-300 transition-colors hover:bg-blue-900 hover:text-blue-100"
							onclick={() => navigateToFilter('project', project)}
						>
							{project}
						</button>
					{/each}
					{#each memory.tags as tag}
						<button
							type="button"
							class="rounded-md bg-emerald-950 px-2.5 py-1 text-sm font-medium text-emerald-300 transition-colors hover:bg-emerald-900 hover:text-emerald-100"
							onclick={() => navigateToFilter('tag', tag)}
						>
							{tag}
						</button>
					{/each}
				</div>
			</section>

			<!-- Timestamps and access stats -->
			<section class="mb-6 grid grid-cols-2 gap-4 sm:grid-cols-4">
				<div>
					<p class="text-xs font-medium uppercase tracking-wider text-gray-500">Created</p>
					<p class="mt-0.5 text-sm text-gray-300">{formatDate(memory.created_at)}</p>
				</div>
				<div>
					<p class="text-xs font-medium uppercase tracking-wider text-gray-500">Updated</p>
					<p class="mt-0.5 text-sm text-gray-300">{formatDate(memory.updated_at)}</p>
				</div>
				<div>
					<p class="text-xs font-medium uppercase tracking-wider text-gray-500">Access count</p>
					<p class="mt-0.5 text-sm text-gray-300">{memory.access_count}</p>
				</div>
				<div>
					<p class="text-xs font-medium uppercase tracking-wider text-gray-500">Last accessed</p>
					<p class="mt-0.5 text-sm text-gray-300">
						{memory.last_accessed_at ? formatDate(memory.last_accessed_at) : 'Never'}
					</p>
				</div>
			</section>

			<!-- Content -->
			<section class="mb-8">
				<div class="prose-invert prose prose-sm max-w-none rounded-lg border border-gray-800 bg-gray-900 p-5">
					{@html renderMarkdown(memory.content)}
				</div>
			</section>

			<!-- Links -->
			{#if detail.outgoing_links.length > 0 || detail.incoming_links.length > 0}
				<section>
					<h2 class="mb-3 text-sm font-semibold uppercase tracking-wider text-gray-500">Links</h2>
					<div class="flex flex-col gap-1.5">
						{#each detail.outgoing_links as link}
							<a
								href="/memory/{link.target_id}"
								class="flex items-center gap-2 rounded-md border border-gray-800 bg-gray-900 px-3 py-2 text-sm transition-colors hover:border-gray-700 hover:bg-gray-800"
							>
								<span class="text-blue-400" title="Outgoing link">&rarr;</span>
								<span class="font-medium text-gray-300">{link.relation}</span>
								{#if link.content}
									<span class="truncate text-xs text-gray-400">{link.content}</span>
								{:else}
									<span class="font-mono text-xs text-gray-500">{truncateId(link.target_id)}</span>
								{/if}
							</a>
						{/each}
						{#each detail.incoming_links as link}
							<a
								href="/memory/{link.source_id}"
								class="flex items-center gap-2 rounded-md border border-gray-800 bg-gray-900 px-3 py-2 text-sm transition-colors hover:border-gray-700 hover:bg-gray-800"
							>
								<span class="text-emerald-400" title="Incoming link">&larr;</span>
								<span class="font-medium text-gray-300">{link.relation}</span>
								{#if link.content}
									<span class="truncate text-xs text-gray-400">{link.content}</span>
								{:else}
									<span class="font-mono text-xs text-gray-500">{truncateId(link.source_id)}</span>
								{/if}
							</a>
						{/each}
					</div>
				</section>
			{/if}

			<!-- Memory ID -->
			<footer class="mt-8 border-t border-gray-800 pt-4">
				<div class="flex items-center gap-1.5">
					<p class="font-mono text-xs text-gray-600">{memory.id}</p>
					<Tooltip.Root>
						<Tooltip.Trigger
							class="rounded p-0.5 text-gray-600 transition-colors hover:text-gray-400"
							onclick={() => copyId(memory.id)}
						>
							{#if copied}
								<CheckIcon class="size-3.5" />
							{:else}
								<CopyIcon class="size-3.5" />
							{/if}
						</Tooltip.Trigger>
						<Tooltip.Portal>
							<Tooltip.Content>
								{copied ? 'Copied!' : 'Copy ID'}
							</Tooltip.Content>
						</Tooltip.Portal>
					</Tooltip.Root>
				</div>
			</footer>
		{/if}
	</main>
</div>

{#if detail}
	<ConfirmDialog
		bind:open={confirmOpen}
		title={(detail.memory.archived_at ? 'Unarchive' : 'Archive') + ' Memory'}
		description={detail.memory.archived_at
			? 'This memory will be restored to your active memories.'
			: 'This memory will be archived. You can unarchive it later.'}
		confirmLabel={detail.memory.archived_at ? 'Unarchive' : 'Archive'}
		onconfirm={handleConfirmAction}
		oncancel={() => { confirmOpen = false; }}
	/>
{/if}
