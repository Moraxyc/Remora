<script lang="ts">
	import { decideLogin, platformName } from '#lib/api.ts';
	import SignIn from '#lib/SignIn.svelte';
	import type { PageProps } from './$types';

	let { data }: PageProps = $props();

	let outcome = $state<'confirmed' | 'denied' | 'gone' | null>(null);
	let busy = $state(false);

	async function decide(decision: 'confirm' | 'deny', csrf: string) {
		busy = true;
		try {
			await decideLogin(data.id, decision, csrf);
			outcome = decision === 'confirm' ? 'confirmed' : 'denied';
		} catch {
			outcome = 'gone';
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head><title>Log in? · Remora</title></svelte:head>

{#if data.login === null || outcome === 'gone'}
	<h1>Login expired</h1>
	<p>This QR code has expired or has been used. Show a new one on the device.</p>
{:else if data.login.confirmed || outcome === 'confirmed'}
	<h1>Logged in</h1>
	<p>The device is logging in. You can close this page.</p>
{:else if outcome === 'denied'}
	<h1>Denied</h1>
	<p>The device was not logged in. You can close this page.</p>
{:else if !data.session.signed_in}
	<h1>Sign in</h1>
	<p>Sign in to decide whether this device may log in.</p>
	<SignIn session={data.session} />
{:else}
	{@const login = data.login}
	{@const signedIn = data.session.signed_in}
	<h1>Log in?</h1>
	<p>A device asks to log in to Remora as you, {signedIn.user.name}.</p>
	<dl>
		<dt>Device</dt>
		<dd>
			{#if login.device}
				{login.device.name || 'Unnamed device'} ({platformName(login.device.platform)})
			{:else}
				Unregistered device
			{/if}
		</dd>
		<dt>Address</dt>
		<dd>{login.address ?? 'unknown'}</dd>
		<dt>Asked at</dt>
		<dd>{new Date(login.asked_at * 1000).toLocaleString()}</dd>
	</dl>
	<p class="muted">Log it in only if you asked for this yourself, just now.</p>
	<button disabled={busy} onclick={() => decide('confirm', signedIn.csrf)}>Log in</button>
	<button class="secondary" disabled={busy} onclick={() => decide('deny', signedIn.csrf)}>
		Deny
	</button>
{/if}
