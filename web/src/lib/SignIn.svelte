<script lang="ts">
	import { invalidate } from '$app/navigation';
	import { page } from '$app/state';
	import { ApiError, oidcSignInUrl, signInWithPassword, type Session } from '#lib/api.ts';

	let { session }: { session: Session } = $props();

	let password = $state('');
	let busy = $state(false);
	let failure = $state<string | null>(
		page.url.searchParams.get('sign_in') === 'failed' ? 'Signing in failed. Try again.' : null
	);

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
{#if session.oidc}
	<p><a class="button" href={oidcSignInUrl(page.url.pathname)}>Sign in with {session.oidc}</a></p>
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
