/**
 * The browser half of WebAuthn, written to be copied: there is no npm package.
 *
 * The server (`crates/auth/src/webauthn.rs`) sends its options in the WebAuthn JSON
 * encoding — base64url strings, unpadded — which `navigator.credentials` does not accept. The
 * ArrayBuffer conversion below is most of what this module exists for; the rest is the two
 * shapes a `get()` can take and the assertion a step-up needs.
 */

import { api } from '../http'
import type { LoginBody, PasskeyView, WaChallenge } from '../types'

/**
 * The browser raises `NotAllowedError` for a dismissed prompt, a timeout and a credential that
 * does not match alike, and the spec forbids telling them apart. To the user, cancelling *is*
 * the answer, so callers show nothing for this and return focus to the password field.
 */
export class PasskeyCancelled extends Error {
	constructor() {
		super('Passkey prompt dismissed')
		this.name = 'PasskeyCancelled'
	}
}

type Json = Record<string, unknown>

function b64urlToBytes(v: string): Uint8Array {
	const b64 = v.replace(/-/g, '+').replace(/_/g, '/')
	const raw = atob(b64.padEnd(Math.ceil(b64.length / 4) * 4, '='))
	const out = new Uint8Array(raw.length)
	for (let i = 0; i < raw.length; i += 1) out[i] = raw.charCodeAt(i)
	return out
}

function bytesToB64url(buf: ArrayBuffer): string {
	let s = ''
	for (const b of new Uint8Array(buf)) s += String.fromCharCode(b)
	return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/g, '')
}

/** `create()` options: only `challenge`, `user.id` and `excludeCredentials[].id` are binary. */
function creationOptions(json: unknown): PublicKeyCredentialCreationOptions {
	const pk = { ...((json as Json).publicKey as Json) }
	const user = pk.user as Json
	pk.challenge = b64urlToBytes(pk.challenge as string)
	pk.user = { ...user, id: b64urlToBytes(user.id as string) }
	if (Array.isArray(pk.excludeCredentials))
		pk.excludeCredentials = (pk.excludeCredentials as Json[]).map((c) => ({
			...c,
			id: b64urlToBytes(c.id as string)
		}))
	return pk as unknown as PublicKeyCredentialCreationOptions
}

/** `get()` options: `challenge` and `allowCredentials[].id`, empty for a usernameless login. */
function requestOptions(json: unknown): PublicKeyCredentialRequestOptions {
	const pk = { ...((json as Json).publicKey as Json) }
	pk.challenge = b64urlToBytes(pk.challenge as string)
	if (Array.isArray(pk.allowCredentials))
		pk.allowCredentials = (pk.allowCredentials as Json[]).map((c) => ({
			...c,
			id: b64urlToBytes(c.id as string)
		}))
	return pk as unknown as PublicKeyCredentialRequestOptions
}

async function prompt<T>(p: Promise<Credential | null>): Promise<T> {
	try {
		return (await p) as T
	} catch (e) {
		if (e instanceof DOMException && e.name === 'NotAllowedError') throw new PasskeyCancelled()
		throw e
	}
}

/** The raw credential as the server's `PublicKeyCredential` expects it: `rawId` and the
 *  response fields re-encoded, `userHandle` null when the credential is not discoverable. */
function assertionJson(cred: PublicKeyCredential) {
	const r = cred.response as AuthenticatorAssertionResponse
	return {
		id: cred.id,
		rawId: bytesToB64url(cred.rawId),
		response: {
			clientDataJSON: bytesToB64url(r.clientDataJSON),
			authenticatorData: bytesToB64url(r.authenticatorData),
			signature: bytesToB64url(r.signature),
			userHandle: r.userHandle === null ? null : bytesToB64url(r.userHandle)
		},
		type: cred.type,
		clientExtensionResults: cred.getClientExtensionResults()
	}
}

/** Where false, render an explicit "Sign in with a passkey" button instead: Safari and older
 *  Firefox never call a conditional `get()`, so a conditional-only screen has no passkey path. */
export async function conditionalUiAvailable(): Promise<boolean> {
	if (typeof PublicKeyCredential === 'undefined') return false
	try {
		return await PublicKeyCredential.isConditionalMediationAvailable()
	} catch {
		return false
	}
}

/** Where false the explicit button must not be rendered either: it opens an empty picker. */
export async function platformAuthenticatorAvailable(): Promise<boolean> {
	if (typeof PublicKeyCredential === 'undefined') return false
	try {
		return await PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable()
	} catch {
		return false
	}
}

/** Enrol a passkey for the signed-in account. Step-up gated; `name` defaults server-side from
 *  the `User-Agent`, because naming a credential before it has been used once is a chore. */
export async function registerPasskey(name?: string): Promise<PasskeyView> {
	const ch = await api.post<WaChallenge>('/api/auth/wa/register/challenge')
	const cred = await prompt<PublicKeyCredential>(
		navigator.credentials.create({ publicKey: creationOptions(ch.options) })
	)
	const r = cred.response as AuthenticatorAttestationResponse
	const registration = {
		id: cred.id,
		rawId: bytesToB64url(cred.rawId),
		response: {
			attestationObject: bytesToB64url(r.attestationObject),
			clientDataJSON: bytesToB64url(r.clientDataJSON),
			transports: r.getTransports?.() ?? []
		},
		type: cred.type,
		clientExtensionResults: cred.getClientExtensionResults()
	}
	return api.post<PasskeyView>('/api/auth/wa/register', { blob: ch.blob, registration, name })
}

/**
 * Usernameless login: the challenge carries an empty `allowCredentials`, and the account
 * comes from the `userHandle` the authenticator returns.
 *
 * `conditional: true` is the inline-autofill variant. It must start before the user acts, and
 * its pending `get()` must be aborted on password submit — otherwise the picker outlives the
 * page the successful login navigated away from.
 */
export async function loginWithPasskey(opts: { conditional?: boolean; signal?: AbortSignal } = {}) {
	const ch = await api.get<WaChallenge>('/api/auth/wa/login/challenge', opts.signal)
	const cred = await prompt<PublicKeyCredential>(
		navigator.credentials.get({
			publicKey: requestOptions(ch.options),
			mediation: opts.conditional === true ? 'conditional' : undefined,
			signal: opts.signal
		})
	)
	return api.post<LoginBody>('/api/auth/wa/login', {
		blob: ch.blob,
		assertion: assertionJson(cred)
	})
}

/**
 * Step-up by passkey. It reuses the *public login challenge*: the assertion's credential recovers
 * the account, so a step-up blob of its own would be a second state variant for nothing.
 */
export async function stepUpWithPasskey(signal?: AbortSignal): Promise<void> {
	const ch = await api.get<WaChallenge>('/api/auth/wa/login/challenge', signal)
	const cred = await prompt<PublicKeyCredential>(
		navigator.credentials.get({ publicKey: requestOptions(ch.options), signal })
	)
	await api.post<void>('/api/auth/step-up', { blob: ch.blob, assertion: assertionJson(cred) })
}

// vim: ts=4
