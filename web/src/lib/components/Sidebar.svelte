<script lang="ts">
	import type { FilterKind } from '$lib/stores/memory-browser.svelte';

	interface NameCount {
		name: string;
		count: number;
	}

	interface SidebarProps {
		projects: NameCount[];
		types: NameCount[];
		onFilterSelect?: (kind: FilterKind, name: string, additive: boolean) => void;
		selectedProjects?: string[];
		selectedTypes?: string[];
	}

	let {
		projects,
		types,
		onFilterSelect,
		selectedProjects = [],
		selectedTypes = [],
	}: SidebarProps = $props();

	function handleClick(kind: FilterKind, name: string, event: MouseEvent) {
		onFilterSelect?.(kind, name, event.ctrlKey || event.metaKey);
	}
</script>

<aside class="flex flex-col gap-6 overflow-y-auto">
	<section>
		<h2 class="text-xs font-semibold uppercase tracking-wider text-gray-500 mb-2 px-2">
			Projects
		</h2>
		<ul class="space-y-0.5">
			{#each projects as item}
				<li>
					<button
						type="button"
						class="flex w-full items-center justify-between rounded-md px-2 py-1.5 text-sm transition-colors hover:bg-gray-800 {selectedProjects.includes(item.name) ? 'bg-gray-800 text-gray-100' : 'text-gray-300'}"
						onclick={(e: MouseEvent) => handleClick('project', item.name, e)}
					>
						<span class="truncate">{item.name}</span>
						<span class="ml-2 shrink-0 text-xs text-gray-500">{item.count}</span>
					</button>
				</li>
			{/each}
			{#if projects.length === 0}
				<li class="px-2 text-sm text-gray-600">No projects yet</li>
			{/if}
		</ul>
	</section>

	<section>
		<h2 class="text-xs font-semibold uppercase tracking-wider text-gray-500 mb-2 px-2">
			Types
		</h2>
		<ul class="space-y-0.5">
			{#each types as item}
				<li>
					<button
						type="button"
						class="flex w-full items-center justify-between rounded-md px-2 py-1.5 text-sm transition-colors hover:bg-gray-800 {selectedTypes.includes(item.name) ? 'bg-gray-800 text-gray-100' : 'text-gray-300'}"
						onclick={(e: MouseEvent) => handleClick('type', item.name, e)}
					>
						<span class="truncate">{item.name}</span>
						<span class="ml-2 shrink-0 text-xs text-gray-500">{item.count}</span>
					</button>
				</li>
			{/each}
			{#if types.length === 0}
				<li class="px-2 text-sm text-gray-600">No types yet</li>
			{/if}
		</ul>
	</section>
</aside>
