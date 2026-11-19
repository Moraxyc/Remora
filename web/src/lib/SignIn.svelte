<script lang="ts">
	import { invalidate } from '$app/navigation';
	import { ApiError, signInWithPassword, type Session } from '#lib/api.ts';

	let { session }: { session: Session } = $props();

	let password = $state('');
	let busy = $state(false);
	let failure = $state<string | null>(null);

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		failure = null;
		try {
			await signInWithPassword(password);
			password = '';
			await invalidate('remora:session');
		} catch (error) {
			failure =
				error instanceof ApiError && error.code === 'wrong_password'
					? 'Wrong password.'
					: 'Signing in failed. Try again.';
		} finally {
			busy = false;
		}
	}
</script>

{#if session.password}
	<form onsubmit={submit}>
		{#if failure}
			<p class="notice error" role="alert">{failure}</p>
		{/if}
		<label>
			Password
			<!-- svelte-ignore a11y_autofocus -->
			<input
				type="password"
				autocomplete="current-password"
				required
				autofocus
				bind:value={password}
			/>
		</label>
		<button disabled={busy}>Sign in</button>
	</form>
{:else}
	<p>This Remora has no way to sign in.</p>
{/if}
