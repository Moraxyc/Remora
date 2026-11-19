import { login } from '#lib/api.ts';
import type { PageLoad } from './$types';

export const load: PageLoad = async ({ fetch, params }) => ({
	id: params.id,
	login: await login(fetch, params.id)
});
