// SPDX-License-Identifier: MPL-2.0
import type { ErrorBody } from './types'

const REFRESH_URL = '/api/auth/refresh'

/** `errCode` is kept as its own field, never collapsed into the message: the
 *  step-up flow branches on `E-AUTH-STEPUP` and registration on `E-CORE-POW`. */
export class ServerError extends Error {
	errCode: string
	errStr: string
	httpStatus: number
	fields?: Record<string, string>
	constructor(
		errCode: string,
		errStr: string,
		httpStatus: number,
		fields?: Record<string, string>
	) {
		super(errStr || errCode)
		this.errCode = errCode
		this.errStr = errStr
		this.httpStatus = httpStatus
		this.fields = fields
	}
}

/** Human-readable message from any thrown error (for toasts and banners). */
export function errMsg(e: unknown): string {
	if (e instanceof ServerError) return e.errStr || e.errCode
	if (e instanceof Error) return e.message
	return 'Unexpected error'
}

function safeJson(text: string): unknown {
	try {
		return JSON.parse(text)
	} catch {
		return undefined
	}
}

async function send(
	method: string,
	url: string,
	body: unknown,
	signal?: AbortSignal
): Promise<Response> {
	return fetch(url, {
		method,
		// No token in JavaScript: login and refresh set HttpOnly cookies and the
		// SPA is served same-origin by the same binary.
		credentials: 'same-origin',
		headers: body !== undefined ? { 'Content-Type': 'application/json' } : {},
		body: body !== undefined ? JSON.stringify(body) : undefined,
		signal
	})
}

/** What a refresh attempt proved. Only `rejected` means the session is gone: a 5xx or a
 *  dropped connection says nothing about it, and treating the two alike signed a user out
 *  mid-storno over one bad response. */
type RefreshResult = 'ok' | 'rejected' | 'failed'

// One shared in-flight refresh: a screen that fires three queries at once would
// otherwise spend three refresh tokens and race itself out of a session.
let refreshing: Promise<RefreshResult> | null = null

// Deliberately unsignalled: it is shared, so whichever query happened to trigger it must not
// be able to abort the refresh every other caller is waiting on.
function refreshOnce(): Promise<RefreshResult> {
	if (!refreshing) {
		refreshing = send('POST', REFRESH_URL, {})
			.then(
				(r): RefreshResult =>
					r.ok ? 'ok' : r.status === 401 || r.status === 403 ? 'rejected' : 'failed'
			)
			.catch((): RefreshResult => 'failed')
			.finally(() => {
				refreshing = null
			})
	}
	return refreshing
}

// A failed refresh is discovered here, on whichever query happened to run, and
// `AuthContext` is the only thing that can act on it: without this hook `me` stays
// populated, the signed-in shell keeps rendering and no path leads back to /login.
let onAuthLost: (() => void) | null = null

export function setOnAuthLost(fn: (() => void) | null) {
	onAuthLost = fn
}

/** The refresh half of the 401 path, for a caller doing its own `fetch` (the agent event stream).
 *  Goes through the shared in-flight refresh; `true` means retry once. */
export async function refreshAfter401(): Promise<boolean> {
	const outcome = await refreshOnce()
	if (outcome === 'rejected') onAuthLost?.()
	return outcome === 'ok'
}

/** A 401 the caller is meant to handle itself. `E-AUTH-STEPUP` wants a password re-entry and
 *  `E-AUTH-CREDENTIALS` has no session behind it, so refreshing either turns a recoverable
 *  prompt into a sign-out. */
const TERMINAL_401 = ['E-AUTH-STEPUP', 'E-AUTH-CREDENTIALS']

async function read(res: Response): Promise<unknown> {
	if (res.status === 204) return undefined
	const text = await res.text()
	return text ? safeJson(text) : undefined
}

function errCodeOf(parsed: unknown): string {
	return (parsed as ErrorBody | undefined)?.error?.errCode ?? ''
}

async function request<R>(
	method: string,
	url: string,
	body?: unknown,
	signal?: AbortSignal
): Promise<R> {
	let res = await send(method, url, body, signal)
	let parsed = await read(res)

	// Exactly one refresh-and-retry, and never on the refresh call itself —
	// otherwise a dead session recurses until the stack gives out.
	if (res.status === 401 && url !== REFRESH_URL && !TERMINAL_401.includes(errCodeOf(parsed))) {
		const outcome = await refreshOnce()
		if (outcome === 'ok' && method === 'GET') {
			res = await send(method, url, body, signal)
			parsed = await read(res)
		} else if (outcome === 'ok') {
			// A successful refresh on a non-GET is not replayed: /cancel issues a numbered legal
			// document, and nothing replays that unasked. But the original 401 would tell the
			// user their credentials failed for an operation that would now succeed.
			throw new ServerError('E-AUTH-RETRY', 'Your session was renewed. Try that again.', 401)
		} else if (outcome === 'rejected') {
			// The refresh token itself was refused, which is the only proof the session is over.
			onAuthLost?.()
		} else if (outcome === 'failed') {
			throw new ServerError(
				'E-CORE-UNAVAILABLE',
				'The server could not be reached. Try again.',
				503
			)
		}
	}

	// `null`, not `undefined`: React Query rejects a `queryFn` that resolves to `undefined`
	// ("data is undefined"), so a 204 landed every such query in `isError`.
	if (res.status === 204) return null as R

	if (!res.ok) {
		const e = parsed as ErrorBody | undefined
		throw new ServerError(
			e?.error?.errCode ?? `E-HTTP-${res.status}`,
			e?.error?.errStr ?? res.statusText,
			res.status,
			e?.error?.fields
		)
	}
	return parsed as R
}

/** `request` for a body that is not JSON — the PDF. Fetched rather than navigated to: the route
 *  is behind `require_auth`, so a bare <a> bypassed the refresh-and-retry below and rendered the
 *  error envelope as a page with no way back to the SPA. */
async function blob(url: string, signal?: AbortSignal): Promise<Blob> {
	let res = await send('GET', url, undefined, signal)
	if (res.status === 401) {
		const outcome = await refreshOnce()
		if (outcome === 'ok') res = await send('GET', url, undefined, signal)
		else if (outcome === 'rejected') onAuthLost?.()
	}
	if (!res.ok) {
		const e = safeJson(await res.text()) as ErrorBody | undefined
		throw new ServerError(
			e?.error?.errCode ?? `E-HTTP-${res.status}`,
			e?.error?.errStr ?? res.statusText,
			res.status,
			e?.error?.fields
		)
	}
	return res.blob()
}

export const api = {
	blob,
	get: <R>(url: string, signal?: AbortSignal) => request<R>('GET', url, undefined, signal),
	post: <R>(url: string, body?: unknown, signal?: AbortSignal) =>
		request<R>('POST', url, body ?? {}, signal),
	patch: <R>(url: string, body?: unknown, signal?: AbortSignal) =>
		request<R>('PATCH', url, body ?? {}, signal),
	put: <R>(url: string, body?: unknown, signal?: AbortSignal) =>
		request<R>('PUT', url, body ?? {}, signal),
	delete: <R>(url: string, signal?: AbortSignal) => request<R>('DELETE', url, undefined, signal)
}

// vim: ts=4
