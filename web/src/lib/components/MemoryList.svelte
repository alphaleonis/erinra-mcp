<script lang="ts">
	import type { Memory } from '$lib/api';
	import { Checkbox } from '$lib/components/ui/checkbox';
	import * as Tooltip from '$lib/components/ui/tooltip';
	import ArchiveIcon from '@lucide/svelte/icons/archive';
	import ArchiveRestoreIcon from '@lucide/svelte/icons/archive-restore';

	interface MemoryListProps {
		memories: Memory[];
		total: number;
		loading?: boolean;
		scores?: Map<string, number>;
		onAction?: (id: string, action: 'archive' | 'unarchive') => void;
		selectable?: boolean;
		selectedIds?: Set<string>;
		onSelectionChange?: (id: string) => void;
		onSelectAll?: () => void;
	}

	let { memories, total, loading = false, scores, onAction, selectable = false, selectedIds, onSelectionChange, onSelectAll }: MemoryListProps = $props();


</script>

{#if loading}
	<div class="flex items-center justify-center py-12">
		<p class="text-gray-400">Loading memories...</p>
	</div>
{:else if memories.length === 0}
	<div class="flex items-center justify-center py-12">
		<p class="text-gray-500">No memories found</p>
	</div>
{:else}
{#if selectable}
	<div class="mb-2 flex items-center gap-2 px-1">
		<Checkbox
			data-testid="select-all"
			checked={memories.length > 0 && memories.every((m) => selectedIds?.has(m.id))}
			onCheckedChange={() => onSelectAll?.()}
		/>
		<span class="text-xs text-gray-400">Select all</span>
	</div>
{/if}
<div class="flex flex-col gap-2" data-testid="memory-list">
	{#each memories as memory (memory.id)}
		<a
			href="/memory/{memory.id}"
			class="group block rounded-lg border border-gray-800 bg-gray-900 px-4 py-3 transition-colors hover:border-gray-700 hover:bg-gray-800{memory.archived_at ? ' opacity-50' : ''}"
			data-testid="memory-item"
		>
			<div class="flex items-start gap-3">
				{#if selectable}
					<Checkbox
						data-testid="select-memory"
						checked={selectedIds?.has(memory.id) ?? false}
						onCheckedChange={() => onSelectionChange?.(memory.id)}
						onclick={(e: MouseEvent) => e.stopPropagation()}
						class="mt-0.5 shrink-0"
					/>
				{/if}
				<p class="flex-1 text-sm text-gray-200 leading-relaxed line-clamp-2 lg:line-clamp-1">{memory.content}</p>
				{#if scores?.has(memory.id)}
					<span class="shrink-0 rounded-md bg-emerald-950 px-2 py-0.5 text-xs font-medium text-emerald-300" data-testid="score">
						{scores.get(memory.id)?.toFixed(2)}
					</span>
				{/if}
				{#if onAction}
					<Tooltip.Root>
						<Tooltip.Trigger
							class="shrink-0 rounded-md p-1 text-gray-400 opacity-0 transition-[opacity,colors] group-hover:opacity-100 hover:bg-gray-700 hover:text-gray-200 focus-visible:opacity-100"
							data-testid="action-btn"
							onclick={(e: MouseEvent) => {
								e.preventDefault();
								e.stopPropagation();
								onAction(memory.id, memory.archived_at ? 'unarchive' : 'archive');
							}}
						>
							{#if memory.archived_at}
								<ArchiveRestoreIcon class="size-4" />
							{:else}
								<ArchiveIcon class="size-4" />
							{/if}
						</Tooltip.Trigger>
						<Tooltip.Portal>
							<Tooltip.Content>
								{memory.archived_at ? 'Unarchive' : 'Archive'}
							</Tooltip.Content>
						</Tooltip.Portal>
					</Tooltip.Root>
				{/if}
			</div>
			<div class="mt-2 flex flex-wrap items-center gap-1.5">
				{#if memory.archived_at}
					<span class="rounded-md bg-amber-950 px-2 py-0.5 text-xs font-medium text-amber-300">
						Archived
					</span>
				{/if}
				{#if memory.memory_type}
					<span class="rounded-md bg-gray-800 px-2 py-0.5 text-xs font-medium text-gray-300">
						{memory.memory_type}
					</span>
				{/if}
				{#each memory.projects as project}
					<span class="rounded-md bg-blue-950 px-2 py-0.5 text-xs font-medium text-blue-300">
						{project}
					</span>
				{/each}
			</div>
		</a>
	{/each}
</div>
{/if}
