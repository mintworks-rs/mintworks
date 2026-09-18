import { api } from '~/api/client'
import type { PowChallenge, PowProof, PowScope } from '~/api/types'
import { config } from '~/config'
import type { SolveResponse } from '~/lib/pow.worker'

/**
 * Fetch a challenge for `scope` and solve it in the worker.
 *
 * `salt`, `exp` and `sig` are echoed back verbatim — the server re-signs them
 * with the difficulty it reads at verification time, so a challenge cannot be
 * edited. Only `nonce` is ours.
 *
 * `signal` aborts both halves. The search is unbounded (`pow.worker.ts`), so without it
 * navigating away mid-solve left a worker spinning a core for the life of the tab.
 */
export async function solvePow(
	scope: PowScope,
	onProgress?: (attempts: number) => void,
	signal?: AbortSignal
): Promise<PowProof> {
	const ch = await api.get<PowChallenge>(`/api/pow/challenge?scope=${scope}`, signal)
	const left = Date.parse(ch.exp) - Date.now()
	if (left <= 0) throw new Error(EXPIRED)
	// The deadline aborts the search rather than being checked only before it: a slow device
	// spent minutes on a dead challenge and then posted a proof the server answers
	// `E-CORE-POW`, which neither caller retries — a dead end with nothing on screen to act on.
	const deadline = AbortSignal.timeout(left)
	const nonce = await solve(
		ch,
		onProgress,
		signal ? AbortSignal.any([signal, deadline]) : deadline
	)
	return { salt: ch.salt, exp: ch.exp, sig: ch.sig, nonce }
}

const EXPIRED = 'The verification challenge expired before it was solved; try again.'

/** A timed-out solve is not a cancelled one, and the caller shows the message verbatim. */
function abortMessage(signal?: AbortSignal): string {
	return (signal?.reason as Error | undefined)?.name === 'TimeoutError'
		? EXPIRED
		: 'Verification cancelled'
}

function solve(
	ch: PowChallenge,
	onProgress?: (attempts: number) => void,
	signal?: AbortSignal
): Promise<number> {
	return new Promise((resolve, reject) => {
		// Every terminal path goes through these two, so a late `onerror` after a `done`
		// cannot double-settle and no path can leave the caller's button stuck at `busy`.
		let settled = false
		let worker: Worker | undefined
		const onAbort = () => {
			worker?.terminate()
			fail(abortMessage(signal))
		}
		const release = () => signal?.removeEventListener('abort', onAbort)
		const fail = (msg: string) => {
			if (settled) return
			settled = true
			release()
			reject(new Error(msg))
		}
		if (signal?.aborted === true) {
			fail(abortMessage(signal))
			return
		}
		try {
			worker = new Worker(config.powWorkerURL)
		} catch {
			fail('Could not run the verification worker')
			return
		}
		const w = worker
		w.onmessage = (ev: MessageEvent<SolveResponse>) => {
			if (ev.data.type === 'progress') {
				onProgress?.(ev.data.nonce)
				return
			}
			w.terminate()
			if (ev.data.type === 'error') {
				fail(ev.data.message)
				return
			}
			if (settled) return
			settled = true
			release()
			resolve(ev.data.nonce)
		}
		w.onerror = () => {
			w.terminate()
			fail('Could not run the verification worker')
		}
		signal?.addEventListener('abort', onAbort)
		w.postMessage({ salt: ch.salt, difficulty: ch.difficulty })
	})
}

// vim: ts=4
