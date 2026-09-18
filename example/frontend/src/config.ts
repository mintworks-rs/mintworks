// The SPA is always same-origin — served by the backend in production, proxied by the dev
// server in development — which is why `credentials: 'same-origin'` in api/client.ts works.
export const config = {
	/** Bundled by esbuild as its own entry point; see esbuild.config.js. */
	powWorkerURL: '/pow.worker.js'
}

// vim: ts=4
