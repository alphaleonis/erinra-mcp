<script lang="ts">
	import '../app.css';
	import { initAuthFromUrl } from '$lib/api';

	initAuthFromUrl();

	let { children } = $props();
</script>

<svelte:head>
	<title>Erinra Dashboard</title>
</svelte:head>

{@render children()}
