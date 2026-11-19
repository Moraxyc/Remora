import { browsers, clientSessions, devices } from '#lib/api.ts';
import type { PageLoad } from './$types';

export const load: PageLoad = async ({ fetch, parent, depends }) => {
	depends('remora:account');
	const { session } = await parent();
	if (!session.signed_in) {
		return { devices: [], clientSessions: [], browsers: [] };
	}
	const [deviceList, clientSessionList, browserList] = await Promise.all([
		devices(fetch),
		clientSessions(fetch),
		browsers(fetch)
	]);
	return {
		devices: deviceList,
		clientSessions: clientSessionList,
		browsers: browserList
	};
};
