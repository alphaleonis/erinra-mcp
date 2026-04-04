import { describe, it, expect, afterEach, vi } from 'vitest';
import { render, cleanup, within, fireEvent } from '@testing-library/svelte';
import FilterBar from './FilterBar.svelte';

afterEach(cleanup);

describe('FilterBar', () => {
	it('renders pills for active filters', () => {
		const filters = {
			projects: ['vestige', 'dotlens'],
			types: ['fact'],
			tags: ['rust'],
		};

		const { container } = render(FilterBar, {
			props: { filters, onRemove: () => {}, onClear: () => {} }
		});
		const view = within(container);

		expect(view.getByText('vestige')).toBeTruthy();
		expect(view.getByText('dotlens')).toBeTruthy();
		expect(view.getByText('fact')).toBeTruthy();
		expect(view.getByText('rust')).toBeTruthy();
	});

	it('renders nothing when no filters active', () => {
		const filters = { projects: [], types: [], tags: [] };

		const { container } = render(FilterBar, {
			props: { filters, onRemove: () => {}, onClear: () => {} }
		});

		// Should have no pill buttons
		const buttons = container.querySelectorAll('[data-testid="filter-pill"]');
		expect(buttons.length).toBe(0);
	});

	it('calls onRemove with kind and value when pill x is clicked', async () => {
		const filters = {
			projects: ['vestige'],
			types: [],
			tags: ['rust'],
		};
		const onRemove = vi.fn();

		const { container } = render(FilterBar, {
			props: { filters, onRemove, onClear: () => {} }
		});
		const view = within(container);

		// Click the remove button on the 'vestige' pill
		const vestigePill = view.getByText('vestige').closest('[data-testid="filter-pill"]')!;
		const removeBtn = vestigePill.querySelector('button')!;
		await fireEvent.click(removeBtn);

		expect(onRemove).toHaveBeenCalledWith('project', 'vestige');
	});

	it('calls onClear when clear all button is clicked', async () => {
		const filters = {
			projects: ['vestige'],
			types: [],
			tags: [],
		};
		const onClear = vi.fn();

		const { container } = render(FilterBar, {
			props: { filters, onRemove: () => {}, onClear }
		});
		const view = within(container);

		const clearBtn = view.getByText('Clear all');
		await fireEvent.click(clearBtn);

		expect(onClear).toHaveBeenCalledOnce();
	});
});
