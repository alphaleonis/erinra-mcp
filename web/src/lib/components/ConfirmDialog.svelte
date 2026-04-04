<script lang="ts">
	import * as AlertDialog from '$lib/components/ui/alert-dialog';

	interface ConfirmDialogProps {
		open: boolean;
		title: string;
		description: string;
		confirmLabel?: string;
		onconfirm: () => void;
		oncancel: () => void;
	}

	let { open = $bindable(), title, description, confirmLabel = 'Confirm', onconfirm, oncancel }: ConfirmDialogProps = $props();
</script>

<AlertDialog.Root bind:open>
	<AlertDialog.Content>
		<AlertDialog.Header>
			<AlertDialog.Title>{title}</AlertDialog.Title>
			<AlertDialog.Description>{description}</AlertDialog.Description>
		</AlertDialog.Header>
		<AlertDialog.Footer>
			<AlertDialog.Cancel onclick={oncancel}>Cancel</AlertDialog.Cancel>
			<AlertDialog.Action onclick={onconfirm}>{confirmLabel}</AlertDialog.Action>
		</AlertDialog.Footer>
	</AlertDialog.Content>
</AlertDialog.Root>
