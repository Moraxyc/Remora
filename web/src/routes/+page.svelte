<script lang="ts">
	import { invalidate } from '$app/navigation';
	import {
		ApiError,
		platformName,
		revokeBrowser,
		revokeClientSession,
		signOut,
		unbindDevice,
		wakeDevice
	} from '#lib/api.ts';
	import SignIn from '#lib/SignIn.svelte';
	import type { PageProps } from './$types';

	let { data }: PageProps = $props();

	let busy = $state<string | null>(null);
	let error = $state<string | null>(null);
	let success = $state<string | null>(null);

	const wakeErrors: Record<string, string> = {
		device_online: 'That device is online already.',
		no_lan_report: 'That device has not reported its LAN yet.',
		no_wake_helper: 'No online device on the same LAN can wake it.',
		too_many_requests: 'Too many wake requests. Wait a moment and try again.'
	};

	async function wake(id: string, csrf: string) {
		busy = `wake:${id}`;
		error = null;
		success = null;
		try {
			await wakeDevice(id, csrf);
			success = 'Wake request sent. The device may take a minute to come online.';
		} catch (caught) {
			error =
				(caught instanceof ApiError && wakeErrors[caught.code]) ||
				'Could not wake that device. Refresh and try again.';
		} finally {
			busy = null;
		}
	}

	function formatTime(seconds: number): string {
		return new Date(seconds * 1000).toLocaleString();
	}

	async function run(key: string, action: () => Promise<void>, refresh: string) {
		busy = key;
		error = null;
		success = null;
		try {
			await action();
			await invalidate(refresh);
		} catch {
			error = 'Could not complete that action. Refresh and try again.';
		} finally {
			busy = null;
		}
	}
</script>

<svelte:head><title>Remora</title></svelte:head>

{#if data.session.signed_in}
	{@const signedIn = data.session.signed_in}
	<header class="topbar">
		<div>
			<h1>Remora</h1>
			<p class="muted">Signed in as {signedIn.user.name}.</p>
		</div>
		<button
			class="secondary"
			disabled={busy !== null}
			onclick={() => run('signout', () => signOut(signedIn.csrf), 'remora:session')}
		>
			Sign out
		</button>
	</header>

	{#if error}
		<p class="notice error" role="alert">{error}</p>
	{/if}
	{#if success}
		<p class="notice" role="status">{success}</p>
	{/if}

	<section aria-labelledby="devices-heading">
		<h2 id="devices-heading">Devices</h2>
		{#if data.devices.length === 0}
			<p class="muted">No devices bound to your account.</p>
		{:else}
			<ul class="items">
				{#each data.devices as device (device.id)}
					<li class="item">
						{#if device.wallpaper}
							<img class="wallpaper" src={device.wallpaper} alt="" />
						{/if}
						<div class="item-info">
							<div class="item-title">
								<span>{device.name || 'Unnamed device'}</span>
								<span class="badge" class:online={device.online}>
									{device.online ? 'Online' : 'Offline'}
								</span>
							</div>
							<div class="item-meta">
								{platformName(device.platform)}
							</div>
						</div>
						{#if !device.online && device.wakeable}
							<button disabled={busy !== null} onclick={() => wake(device.id, signedIn.csrf)}>
								Wake
							</button>
						{/if}
						<button
							class="secondary"
							disabled={busy !== null}
							onclick={() =>
								run(
									`device:${device.id}`,
									() => unbindDevice(device.id, signedIn.csrf),
									'remora:account'
								)}
						>
							Unbind
						</button>
					</li>
				{/each}
			</ul>
		{/if}
	</section>

	<section aria-labelledby="clients-heading">
		<h2 id="clients-heading">Client logins</h2>
		{#if data.clientSessions.length === 0}
			<p class="muted">No active client logins.</p>
		{:else}
			<ul class="items">
				{#each data.clientSessions as client (client.id)}
					<li class="item">
						<div class="item-info">
							<div class="item-title">
								{#if client.device}
									{client.device.name || 'Unnamed device'} ({platformName(
										client.device.platform
									)})
								{:else}
									Viewer or unbound host
								{/if}
							</div>
							<div class="item-meta">
								{#if client.created !== null}
									Logged in {formatTime(client.created)}
								{:else}
									Creation time unknown
								{/if}
							</div>
						</div>
						<button
							class="secondary"
							disabled={busy !== null}
							onclick={() =>
								run(
									`client:${client.id}`,
									() => revokeClientSession(client.id, signedIn.csrf),
									'remora:account'
								)}
						>
							Revoke
						</button>
					</li>
				{/each}
			</ul>
		{/if}
	</section>

	<section aria-labelledby="browsers-heading">
		<h2 id="browsers-heading">Browsers</h2>
		<ul class="items">
			{#each data.browsers as browser (browser.id)}
				<li class="item">
					<div class="item-info">
						<div class="item-title">
							<span>Signed in {formatTime(browser.created)}</span>
							{#if browser.current}
								<span class="badge online">This browser</span>
							{/if}
						</div>
						<div class="item-meta">Expires {formatTime(browser.expires)}</div>
					</div>
					<button
						class="secondary"
						disabled={busy !== null}
						onclick={() =>
							run(
								`browser:${browser.id}`,
								() => revokeBrowser(browser.id, signedIn.csrf),
								browser.current ? 'remora:session' : 'remora:account'
							)}
					>
						{browser.current ? 'Sign out' : 'Revoke'}
					</button>
				</li>
			{/each}
		</ul>
	</section>
{:else}
	<h1>Remora</h1>
	<SignIn session={data.session} />
{/if}
