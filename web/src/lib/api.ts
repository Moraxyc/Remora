// Remora's web API: plain JSON under /api/web/. Changes carry the browser's CSRF token.

export interface Profile {
	id: string;
	name: string;
}

export interface Session {
	signed_in: { user: Profile; csrf: string } | null;
	/** Whether signing in with the owner password is possible. */
	password: boolean;
	/** Whether Remora is reached over https. */
	secure: boolean;
}

export interface Login {
	device: { name: string; platform: number } | null;
	address: string | null;
	/** Seconds since the Unix epoch. */
	asked_at: number;
	confirmed: boolean;
}

export class ApiError extends Error {
	constructor(
		readonly status: number,
		readonly code: string
	) {
		super(`${status} ${code}`);
	}
}

type Fetch = typeof fetch;

async function call(
	fetch: Fetch,
	method: string,
	path: string,
	options: { body?: unknown; csrf?: string | undefined } = {}
): Promise<Response> {
	const headers: Record<string, string> = {};
	if (options.body !== undefined) headers['content-type'] = 'application/json';
	if (options.csrf !== undefined) headers['x-csrf-token'] = options.csrf;
	const response = await fetch(`/api/web/${path}`, {
		method,
		headers,
		...(options.body === undefined ? {} : { body: JSON.stringify(options.body) })
	});
	if (!response.ok) {
		const error: unknown = await response.json().catch(() => null);
		const code =
			typeof error === 'object' && error !== null && 'error' in error
				? String(error.error)
				: 'unknown';
		throw new ApiError(response.status, code);
	}
	return response;
}

export async function session(fetch: Fetch): Promise<Session> {
	return (await call(fetch, 'GET', 'session')).json();
}

export async function signInWithPassword(password: string): Promise<void> {
	await call(fetch, 'POST', 'session', { body: { password } });
}

export async function signOut(csrf: string): Promise<void> {
	await call(fetch, 'DELETE', 'session', { csrf });
}

/** Opening a login counts as scanning its QR code. Null once it is gone. */
export async function login(fetch: Fetch, id: string): Promise<Login | null> {
	try {
		return await (await call(fetch, 'GET', `logins/${encodeURIComponent(id)}`)).json();
	} catch (error) {
		if (error instanceof ApiError && error.status === 404) return null;
		throw error;
	}
}

export async function decideLogin(
	id: string,
	decision: 'confirm' | 'deny',
	csrf: string
): Promise<void> {
	await call(fetch, 'POST', `logins/${encodeURIComponent(id)}`, { body: { decision }, csrf });
}

/** UU's platform numbers. */
export function platformName(platform: number): string {
	return ['', 'Windows', 'Android', 'iOS', 'macOS'][platform] || 'unknown platform';
}
