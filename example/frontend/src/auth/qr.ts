/**
 * The desktop half of QR login.
 *
 * `init` is anonymous and answers with a secret that must never leave this browser: it goes
 * back in the `x-qr-secret` header, never the query string, which is what stops a bystander who
 * photographs the code from collecting the tokens it produces. It does not stop a *forwarded*
 * code — there the attacker is the initiator — and the match code plus the device details on
 * the approving screen are the human defence against that.
 */

import { api } from '~/api/client'
import type { LoginBody, QrDetails, QrInit, QrPending } from '~/api/types'

/** The server's cap on one long poll. Asking for less only spends more requests. */
export const QR_POLL_SECONDS = 30

/** Starts a session. Nothing about the caller is bound: the account is whoever approves. */
export function initQr(): Promise<QrInit> {
	return api.post<QrInit>('/api/auth/qr/init')
}

/** What the approving phone is about to let in: browser and IP, never the match code. */
export function qrDetails(sessionId: string): Promise<QrDetails> {
	return api.get<QrDetails>(`/api/auth/qr/${sessionId}/details`)
}

/** `approved: false` is a denial; either way the session is answered and cannot be answered
 *  again. `matchCode` is typed off the initiating screen, which is why `details` does not
 *  return it. */
export function respondQr(sessionId: string, approved: boolean, matchCode: string): Promise<void> {
	return api.post<void>(`/api/auth/qr/${sessionId}/respond`, { approved, matchCode })
}

/**
 * Long-poll until the session resolves. A plain `fetch`, not `api.*`: those take no headers,
 * and the secret is the whole credential.
 *
 * The server holds each request up to `wait` seconds and **re-reads the state on every wake**,
 * so a missed notification costs one window and never an answer. That is why this loop has no
 * backoff and no deadline of its own — the session's TTL and `signal` are the only two ways out.
 *
 * Resolves with the login body on approval, or the terminal status. A session the server has
 * already swept answers 404, which is the same thing a waited-out TTL means to this end.
 */
export async function pollQr(
	sessionId: string,
	secret: string,
	signal: AbortSignal
): Promise<LoginBody | QrPending> {
	const res = await fetch(`/api/auth/qr/${sessionId}/status?wait=${QR_POLL_SECONDS}`, {
		headers: { 'x-qr-secret': secret },
		credentials: 'same-origin',
		signal
	})
	if (res.status === 404 || res.status === 401) return 'expired'
	if (!res.ok) throw new Error(`The sign-in request failed (${res.status}).`)
	const body = (await res.json()) as LoginBody | { status: QrPending }
	return 'status' in body ? body.status : body
}

// vim: ts=4
