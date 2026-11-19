import { session } from '#lib/api.ts';
import type { LayoutLoad } from './$types';

// Rendered in the browser only: Remora serves the built files and has no JavaScript runtime.
export const ssr = false;
export const prerender = false;

export const load: LayoutLoad = async ({ fetch, depends }) => {
	depends('remora:session');
	return { session: await session(fetch) };
};
