import { describe, it, expect, afterEach, vi } from 'vitest';
import { render, cleanup, within, fireEvent } from '@testing-library/svelte';
import Pagination from './Pagination.svelte';

afterEach(cleanup);

describe('Pagination', () => {
	it('renders "Showing X-Y of N" text', () => {
		const { container } = render(Pagination, {
			props: { total: 45, offset: 0, limit: 20, onPageChange: () => {} }
		});
		const view = within(container);

		expect(view.getByText(/Showing 1\u201320 of 45/)).toBeTruthy();
	});

	it('shows correct range for second page', () => {
		const { container } = render(Pagination, {
			props: { total: 45, offset: 20, limit: 20, onPageChange: () => {} }
		});
		const view = within(container);

		expect(view.getByText(/Showing 21\u201340 of 45/)).toBeTruthy();
	});

	it('clamps end to total on last page', () => {
		const { container } = render(Pagination, {
			props: { total: 45, offset: 40, limit: 20, onPageChange: () => {} }
		});
		const view = within(container);

		expect(view.getByText(/Showing 41\u201345 of 45/)).toBeTruthy();
	});

	it('disables Previous on first page', () => {
		const { container } = render(Pagination, {
			props: { total: 45, offset: 0, limit: 20, onPageChange: () => {} }
		});

		const prevBtn = container.querySelector('[aria-label="Go to previous page"]') as HTMLButtonElement;
		expect(prevBtn.disabled).toBe(true);
	});

	it('disables Next on last page', () => {
		const { container } = render(Pagination, {
			props: { total: 45, offset: 40, limit: 20, onPageChange: () => {} }
		});

		const nextBtn = container.querySelector('[aria-label="Go to next page"]') as HTMLButtonElement;
		expect(nextBtn.disabled).toBe(true);
	});

	it('calls onPageChange with next offset when Next clicked', async () => {
		const onPageChange = vi.fn();
		const { container } = render(Pagination, {
			props: { total: 45, offset: 0, limit: 20, onPageChange }
		});

		const nextBtn = container.querySelector('[aria-label="Go to next page"]') as HTMLButtonElement;
		await fireEvent.click(nextBtn);

		expect(onPageChange).toHaveBeenCalledWith(20);
	});

	it('calls onPageChange with previous offset when Previous clicked', async () => {
		const onPageChange = vi.fn();
		const { container } = render(Pagination, {
			props: { total: 45, offset: 20, limit: 20, onPageChange }
		});

		const prevBtn = container.querySelector('[aria-label="Go to previous page"]') as HTMLButtonElement;
		await fireEvent.click(prevBtn);

		expect(onPageChange).toHaveBeenCalledWith(0);
	});

	it('renders nothing when total is 0', () => {
		const { container } = render(Pagination, {
			props: { total: 0, offset: 0, limit: 20, onPageChange: () => {} }
		});

		// No pagination content rendered
		expect(container.querySelector('[data-slot="pagination"]')).toBeNull();
	});
});
