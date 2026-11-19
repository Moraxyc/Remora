<script lang="ts">
	import '../app.css';
	import type { LayoutProps } from './$types';

	let { data, children }: LayoutProps = $props();
</script>

<main>
	{#if !data.session.secure}
		<p class="notice">
			Remora is reached over plain HTTP, so this page and your sign-in travel unencrypted.
		</p>
	{/if}
	{@render children()}
</main>
