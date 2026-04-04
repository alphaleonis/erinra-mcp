import { describe, it, expect, vi, afterEach } from 'vitest';
import { render, cleanup, screen, fireEvent } from '@testing-library/svelte';
import ConfirmDialog from './ConfirmDialog.svelte';

afterEach(cleanup);

describe('ConfirmDialog', () => {
	it('renders title and description when open', async () => {
		render(ConfirmDialog, {
			props: {
				open: true,
				title: 'Archive Memory',
				description: 'This memory will be archived.',
				onconfirm: () => {},
				oncancel: () => {},
			},
		});

		// bits-ui AlertDialog renders in a portal, so query the full document
		expect(screen.getByText('Archive Memory')).toBeTruthy();
		expect(screen.getByText('This memory will be archived.')).toBeTruthy();
	});

	it('renders default confirm label "Confirm" and a cancel button', async () => {
		render(ConfirmDialog, {
			props: {
				open: true,
				title: 'Test',
				description: 'Test description',
				onconfirm: () => {},
				oncancel: () => {},
			},
		});

		expect(screen.getByText('Confirm')).toBeTruthy();
		expect(screen.getByText('Cancel')).toBeTruthy();
	});

	it('renders custom confirm label when provided', async () => {
		render(ConfirmDialog, {
			props: {
				open: true,
				title: 'Test',
				description: 'Test description',
				confirmLabel: 'Archive',
				onconfirm: () => {},
				oncancel: () => {},
			},
		});

		expect(screen.getByText('Archive')).toBeTruthy();
	});

	it('calls onconfirm when confirm button is clicked', async () => {
		const onconfirm = vi.fn();

		render(ConfirmDialog, {
			props: {
				open: true,
				title: 'Test',
				description: 'Test description',
				onconfirm,
				oncancel: () => {},
			},
		});

		await fireEvent.click(screen.getByText('Confirm'));
		expect(onconfirm).toHaveBeenCalledOnce();
	});

	it('calls oncancel when cancel button is clicked', async () => {
		const oncancel = vi.fn();

		render(ConfirmDialog, {
			props: {
				open: true,
				title: 'Test',
				description: 'Test description',
				onconfirm: () => {},
				oncancel,
			},
		});

		await fireEvent.click(screen.getByText('Cancel'));
		expect(oncancel).toHaveBeenCalledOnce();
	});
});
