<script lang="ts">
	import { invalidate } from '$app/navigation';
	import { page } from '$app/state';
	import { onMount } from 'svelte';
	import { ApiError, oidcSignInUrl, signInWithPassword, type Session } from '#lib/api.ts';

	let { session }: { session: Session } = $props();

	const failed = page.url.searchParams.get('sign_in') === 'failed';
	const oidcUrl = $derived(oidcSignInUrl(page.url.pathname));
	// With the provider as the only way in there is nothing to choose, so go there at once;
	// after a failed sign-in, wait for the user instead of sending them round again.
	const straightToOidc = $derived(session.oidc !== null && !session.password && !failed);

	let password = $state('');
	let busy = $state(false);
	let failure = $state<string | null>(failed ? 'Signing in failed. Try again.' : null);

	onMount(() => {
		if (straightToOidc) location.assign(oidcUrl);
	});

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		failure = null;
		try {
			await signInWithPassword(password);
			password = '';
			await invalidate('remora:session');
		} catch (error) {
			const code = error instanceof ApiError ? error.code : null;
			failure =
				code === 'wrong_password'
					? 'Wrong password.'
					: code === 'too_many_requests'
						? 'Too many attempts. Wait a minute and try again.'
						: 'Signing in failed. Try again.';
		} finally {
			busy = false;
		}
	}
</script>

{#if failure}
	<p class="notice error" role="alert">{failure}</p>
{/if}
{#if straightToOidc}
	<p class="muted">Signing in with {session.oidc}…</p>
{/if}
{#if session.oidc}
	<p><a class="button" href={oidcUrl}>Sign in with {session.oidc}</a></p>
{/if}
{#if session.password}
	{#if session.oidc}
		<p class="muted">Or, as the owner:</p>
	{/if}
	<form onsubmit={submit}>
		<label>
			Password
			<!-- svelte-ignore a11y_autofocus -->
			<input
				type="password"
				autocomplete="current-password"
				required
				autofocus={!session.oidc}
				bind:value={password}
			/>
		</label>
		<button disabled={busy}>Sign in</button>
	</form>
{/if}
{#if !session.oidc && !session.password}
	<p>This Remora has no way to sign in.</p>
{/if}
