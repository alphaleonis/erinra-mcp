<script lang="ts">
	import * as PaginationUI from '$lib/components/ui/pagination';

	interface PaginationProps {
		total: number;
		offset: number;
		limit: number;
		onPageChange: (newOffset: number) => void;
	}

	let { total, offset, limit, onPageChange }: PaginationProps = $props();

	let currentPage = $derived(Math.floor(offset / limit) + 1);
	let start = $derived(total === 0 ? 0 : offset + 1);
	let end = $derived(Math.min(offset + limit, total));
</script>

{#if total > 0}
<div class="flex flex-col items-center gap-2">
	<PaginationUI.Root
		count={total}
		perPage={limit}
		page={currentPage}
		onPageChange={(page) => onPageChange((page - 1) * limit)}
	>
		{#snippet children({ pages })}
			<PaginationUI.Content>
				<PaginationUI.Item>
					<PaginationUI.PrevButton />
				</PaginationUI.Item>
				{#each pages as page (page.key)}
					{#if page.type === 'ellipsis'}
						<PaginationUI.Item>
							<PaginationUI.Ellipsis />
						</PaginationUI.Item>
					{:else}
						<PaginationUI.Item>
							<PaginationUI.Link {page} isActive={page.value === currentPage} />
						</PaginationUI.Item>
					{/if}
				{/each}
				<PaginationUI.Item>
					<PaginationUI.NextButton />
				</PaginationUI.Item>
			</PaginationUI.Content>
		{/snippet}
	</PaginationUI.Root>
	<span class="text-xs text-muted-foreground">
		Showing {start}&ndash;{end} of {total}
	</span>
</div>
{/if}
