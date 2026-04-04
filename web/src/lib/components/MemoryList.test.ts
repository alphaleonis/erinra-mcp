import { describe, it, expect, vi, afterEach } from 'vitest';
import { render, cleanup, within, fireEvent } from '@testing-library/svelte';
import MemoryList from './MemoryList.svelte';
import type { Memory } from '$lib/api';

afterEach(cleanup);

const sampleMemories: Memory[] = [
	{
		id: 'mem-1',
		content: 'Vestige uses FSRS-6 spaced repetition to naturally decay memories over time.',
		memory_type: 'fact',
		projects: ['vestige'],
		tags: ['rust', 'architecture'],
		created_at: '2025-06-01T12:00:00Z',
		updated_at: '2025-06-01T12:00:00Z',
		archived_at: null,
		last_accessed_at: '2025-06-01T14:00:00Z',
		access_count: 5,
		truncated: false,
	},
	{
		id: 'mem-2',
		content: 'The dashboard uses Svelte 5 runes for state management with $state and $derived.',
		memory_type: 'pattern',
		projects: ['vestige', 'dotlens'],
		tags: ['svelte'],
		created_at: '2025-06-02T12:00:00Z',
		updated_at: '2025-06-02T12:00:00Z',
		archived_at: null,
		last_accessed_at: null,
		access_count: 3,
		truncated: false,
	},
];

describe('MemoryList', () => {
	it('renders memory content snippets', () => {
		const { container } = render(MemoryList, { props: { memories: sampleMemories, total: 2 } });
		const view = within(container);

		expect(view.getByText(/Vestige uses FSRS-6/)).toBeTruthy();
		expect(view.getByText(/dashboard uses Svelte 5/)).toBeTruthy();
	});

	it('renders type badges', () => {
		const { container } = render(MemoryList, { props: { memories: sampleMemories, total: 2 } });
		const view = within(container);

		expect(view.getByText('fact')).toBeTruthy();
		expect(view.getByText('pattern')).toBeTruthy();
	});

	it('renders project badges', () => {
		const { container } = render(MemoryList, { props: { memories: sampleMemories, total: 2 } });
		const view = within(container);

		// 'vestige' appears twice (both memories), 'dotlens' once
		const vestiges = view.getAllByText('vestige');
		expect(vestiges.length).toBe(2);
		expect(view.getByText('dotlens')).toBeTruthy();
	});

	it('shows empty state when no memories', () => {
		const { container } = render(MemoryList, { props: { memories: [], total: 0 } });
		const view = within(container);

		expect(view.getByText('No memories found')).toBeTruthy();
	});

	it('shows loading state', () => {
		const { container } = render(MemoryList, { props: { memories: [], total: 0, loading: true } });
		const view = within(container);

		expect(view.getByText('Loading memories...')).toBeTruthy();
		// Should NOT show "No memories found" while loading
		expect(view.queryByText('No memories found')).toBeNull();
	});

	it('shows relevance score when scores map is provided', () => {
		const scores = new Map<string, number>([
			['mem-1', 0.85],
			['mem-2', 0.72],
		]);

		const { container } = render(MemoryList, {
			props: { memories: sampleMemories, total: 2, scores },
		});
		const view = within(container);

		expect(view.getByText('0.85')).toBeTruthy();
		expect(view.getByText('0.72')).toBeTruthy();
	});

	it('does not show scores when scores map is empty', () => {
		const { container } = render(MemoryList, {
			props: { memories: sampleMemories, total: 2, scores: new Map() },
		});

		// No score-related elements should be present.
		const scoreElements = container.querySelectorAll('[data-testid="score"]');
		expect(scoreElements.length).toBe(0);
	});

	it('applies dimmed styling to archived memory items', () => {
		const memoriesWithArchived: Memory[] = [
			{
				...sampleMemories[0],
				archived_at: '2025-06-10T12:00:00Z',
			},
			sampleMemories[1], // not archived
		];

		const { container } = render(MemoryList, {
			props: { memories: memoriesWithArchived, total: 2 },
		});

		const items = container.querySelectorAll('[data-testid="memory-item"]');
		expect(items.length).toBe(2);

		// First item (archived) should have opacity-50
		expect(items[0].className).toContain('opacity-50');

		// Second item (not archived) should NOT have opacity-50
		expect(items[1].className).not.toContain('opacity-50');
	});

	it('renders action buttons for active memories when onAction is provided', () => {
		const onAction = vi.fn();
		const { container } = render(MemoryList, {
			props: { memories: sampleMemories, total: 2, onAction },
		});

		// Both sample memories are active (archived_at is null)
		const archiveBtns = container.querySelectorAll('[data-testid="action-btn"]');
		expect(archiveBtns.length).toBe(2);
	});

	it('renders action buttons for archived and active memories when onAction is provided', () => {
		const onAction = vi.fn();
		const memoriesWithArchived: Memory[] = [
			{
				...sampleMemories[0],
				archived_at: '2025-06-10T12:00:00Z',
			},
			sampleMemories[1], // not archived
		];

		const { container } = render(MemoryList, {
			props: { memories: memoriesWithArchived, total: 2, onAction },
		});

		const btns = container.querySelectorAll('[data-testid="action-btn"]');
		expect(btns.length).toBe(2);
	});

	it('does not render action buttons when onAction is not provided', () => {
		const { container } = render(MemoryList, {
			props: { memories: sampleMemories, total: 2 },
		});

		const btns = container.querySelectorAll('button[data-testid="action-btn"]');
		expect(btns.length).toBe(0);
	});

	it('fires onAction with correct id and action when button is clicked', async () => {
		const onAction = vi.fn();
		const { container } = render(MemoryList, {
			props: { memories: sampleMemories, total: 2, onAction },
		});

		const btns = container.querySelectorAll('button[data-testid="action-btn"]');
		await fireEvent.click(btns[0]);

		expect(onAction).toHaveBeenCalledOnce();
		expect(onAction).toHaveBeenCalledWith('mem-1', 'archive');
	});

	it('renders checkboxes when selectable is true', () => {
		const selectedIds = new Set<string>(['mem-1']);
		const { container } = render(MemoryList, {
			props: { memories: sampleMemories, total: 2, selectable: true, selectedIds, onSelectionChange: vi.fn(), onSelectAll: vi.fn() },
		});

		const checkboxes = container.querySelectorAll('[data-testid="select-memory"]');
		expect(checkboxes.length).toBe(2);

		// mem-1 is selected, mem-2 is not
		expect(checkboxes[0].getAttribute('data-state')).toBe('checked');
		expect(checkboxes[1].getAttribute('data-state')).toBe('unchecked');
	});

	it('clicking checkbox fires onSelectionChange with correct id', async () => {
		const onSelectionChange = vi.fn();
		const { container } = render(MemoryList, {
			props: { memories: sampleMemories, total: 2, selectable: true, selectedIds: new Set<string>(), onSelectionChange, onSelectAll: vi.fn() },
		});

		const checkboxes = container.querySelectorAll('[data-testid="select-memory"]');
		await fireEvent.click(checkboxes[1]);

		expect(onSelectionChange).toHaveBeenCalledOnce();
		expect(onSelectionChange).toHaveBeenCalledWith('mem-2');
	});

	it('renders select-all checkbox that fires onSelectAll', async () => {
		const onSelectAll = vi.fn();
		const { container } = render(MemoryList, {
			props: { memories: sampleMemories, total: 2, selectable: true, selectedIds: new Set<string>(), onSelectionChange: vi.fn(), onSelectAll },
		});

		const selectAll = container.querySelector('[data-testid="select-all"]');
		expect(selectAll).toBeTruthy();

		await fireEvent.click(selectAll!);
		expect(onSelectAll).toHaveBeenCalledOnce();
	});

	it('select-all checkbox is checked when all visible memories are selected', () => {
		const selectedIds = new Set<string>(['mem-1', 'mem-2']);
		const { container } = render(MemoryList, {
			props: { memories: sampleMemories, total: 2, selectable: true, selectedIds, onSelectionChange: vi.fn(), onSelectAll: vi.fn() },
		});

		const selectAll = container.querySelector('[data-testid="select-all"]');
		expect(selectAll!.getAttribute('data-state')).toBe('checked');
	});

	it('select-all checkbox is unchecked when not all visible memories are selected', () => {
		const selectedIds = new Set<string>(['mem-1']);
		const { container } = render(MemoryList, {
			props: { memories: sampleMemories, total: 2, selectable: true, selectedIds, onSelectionChange: vi.fn(), onSelectAll: vi.fn() },
		});

		const selectAll = container.querySelector('[data-testid="select-all"]');
		expect(selectAll!.getAttribute('data-state')).toBe('unchecked');
	});

	it('does not render select-all checkbox when selectable is false', () => {
		const { container } = render(MemoryList, {
			props: { memories: sampleMemories, total: 2 },
		});

		const selectAll = container.querySelector('[data-testid="select-all"]');
		expect(selectAll).toBeNull();
	});

	it('does not render checkboxes when selectable is false', () => {
		const { container } = render(MemoryList, {
			props: { memories: sampleMemories, total: 2 },
		});

		const checkboxes = container.querySelectorAll('[data-testid="select-memory"]');
		expect(checkboxes.length).toBe(0);
	});

	it('renders "Archived" badge for memories with archived_at set', () => {
		const memoriesWithArchived: Memory[] = [
			{
				...sampleMemories[0],
				archived_at: '2025-06-10T12:00:00Z',
			},
			sampleMemories[1], // not archived
		];

		const { container } = render(MemoryList, {
			props: { memories: memoriesWithArchived, total: 2 },
		});
		const view = within(container);

		const badges = view.getAllByText('Archived');
		expect(badges.length).toBe(1);

		// The badge should use amber styling
		expect(badges[0].className).toContain('bg-amber-950');
		expect(badges[0].className).toContain('text-amber-300');
	});
});
