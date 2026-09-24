// Proof-of-work solver, bundled as its own entry point (see esbuild.config.js)
// and run as a classic Worker so a difficulty-18 search does not freeze the page.
//
// The rule is `saas-auth/src/pow.rs::solved`: sha256(salt_ascii || nonce_decimal_ascii),
// no separator, must have at least `difficulty` leading zero *bits*.

export interface SolveRequest {
	salt: string
	difficulty: number
}

export type SolveResponse =
	| { type: 'progress'; nonce: number }
	| { type: 'done'; nonce: number }
	| { type: 'error'; message: string }

/** Report roughly every this many attempts so the UI can say "Verifying…". */
const PROGRESS_EVERY = 5000

function leadingZeroBits(digest: Uint8Array): number {
	let bits = 0
	for (const byte of digest) {
		// clz32 counts over 32 bits; a byte occupies the low 8, hence the -24.
		bits += Math.clz32(byte) - 24
		if (byte !== 0) break
	}
	return bits
}

addEventListener('message', async (ev) => {
	// A rejection here would be an unhandled rejection in the worker, which never reaches
	// `Worker.onerror` — the caller's promise would never settle. `crypto.subtle` is
	// `undefined` outside a secure context, which is exactly how that happens.
	try {
		const { salt, difficulty } = (ev as MessageEvent<SolveRequest>).data
		const enc = new TextEncoder()
		for (let nonce = 0; ; nonce++) {
			const buf = await crypto.subtle.digest('SHA-256', enc.encode(salt + String(nonce)))
			if (leadingZeroBits(new Uint8Array(buf)) >= difficulty) {
				postMessage({ type: 'done', nonce } satisfies SolveResponse)
				return
			}
			if (nonce % PROGRESS_EVERY === 0)
				postMessage({ type: 'progress', nonce } satisfies SolveResponse)
		}
	} catch (e) {
		const message = e instanceof Error ? e.message : 'Verification failed'
		postMessage({ type: 'error', message } satisfies SolveResponse)
	}
})

// vim: ts=4
