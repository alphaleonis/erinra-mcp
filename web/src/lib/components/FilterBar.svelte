<script lang="ts">
	import type { FilterKind } from '$lib/stores/memory-browser.svelte';

	interface FilterBarProps {
		filters: { projects: string[]; types: string[]; tags: string[] };
		onRemove: (kind: FilterKind, value: string) => void;
		onClear: () => void;
	}

	let { filters, onRemove, onClear }: FilterBarProps = $props();

	interface Pill {
		kind: FilterKind;
		value: string;
		label: string;
	}

	let pills: Pill[] = $derived.by(() => {
		const result: Pill[] = [];
		for (const p of filters.projects) result.push({ kind: 'project', value: p, label: p });
		for (const t of filters.types) result.push({ kind: 'type', value: t, label: t });
		for (const t of filters.tags) result.push({ kind: 'tag', value: t, label: t });
		return result;
	});

	let hasFilters = $derived(pills.length > 0);
</script>

{#if hasFilters}
	<div class="flex flex-wrap items-center gap-2">
		{#each pills as pill (`${pill.kind}-${pill.value}`)}
			<span
				data-testid="filter-pill"
				class="inline-flex items-center gap-1 rounded-full bg-gray-800 px-3 py-1 text-sm text-gray-200"
			>
				<span class="text-xs text-gray-400">{pill.kind}:</span>
				{pill.value}
				<button
					type="button"
					class="ml-0.5 rounded-full p-0.5 text-gray-400 hover:bg-gray-700 hover:text-gray-200"
					onclick={() => onRemove(pill.kind, pill.value)}
					aria-label="Remove {pill.kind} filter: {pill.value}"
				>
					<svg class="h-3 w-3" fill="none" stroke="currentColor" viewBox="0 0 24 24">
						<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
					</svg>
				</button>
			</span>
		{/each}

		<button
			type="button"
			class="text-xs text-gray-500 hover:text-gray-300"
			onclick={onClear}
		>
			Clear all
		</button>
	</div>
{/if}
