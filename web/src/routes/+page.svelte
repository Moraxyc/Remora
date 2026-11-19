<script lang="ts">
	import { invalidate } from '$app/navigation';
	import { signOut } from '#lib/api.ts';
	import SignIn from '#lib/SignIn.svelte';
	import type { PageProps } from './$types';

	let { data }: PageProps = $props();

	async function leave(csrf: string) {
		await signOut(csrf);
		await invalidate('remora:session');
	}
</script>

<svelte:head><title>Remora</title></svelte:head>

<h1>Remora</h1>
{#if data.session.signed_in}
	{@const signedIn = data.session.signed_in}
	<p>Signed in as {signedIn.user.name}.</p>
	<button class="secondary" onclick={() => leave(signedIn.csrf)}>Sign out</button>
{:else}
	<SignIn session={data.session} />
{/if}
