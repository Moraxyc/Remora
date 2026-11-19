import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

// Remora embeds the build and serves it on its own origin, next to the API.
const remora = process.env.REMORA_URL ?? 'http://127.0.0.1:8080';

export default defineConfig({
	plugins: [
		sveltekit({
			// A single-page app: every path that is not a file gets the fallback page.
			adapter: adapter({ fallback: 'app.html' }),
			// Lets the page's inline bootstrap run under a CSP without 'unsafe-inline'.
			csp: {
				mode: 'hash',
				directives: {
					'default-src': ['none'],
					'script-src': ['self'],
					'style-src': ['self'],
					'img-src': ['self'],
					'font-src': ['self'],
					'connect-src': ['self']
				}
			}
		})
	],
	server: {
		proxy: {
			'/api': remora
		}
	}
});
