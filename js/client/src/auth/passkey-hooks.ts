// SPDX-License-Identifier: MPL-2.0
// The passkey screens' lifecycle, shared so the apps keep only their markup.

import * as React from 'react'

import type { LoginBody } from '../types'
import {
	conditionalUiAvailable,
	loginWithPasskey,
	platformAuthenticatorAvailable
} from './webauthn'

/**
 * Conditional UI for a login screen. It starts *before* the user does anything — that is what
 * makes the browser offer a passkey inside the `username webauthn` field — so it runs while
 * `skip` is false, and `abort()` must be called on password submit and before an explicit
 * `loginWithPasskey()`: only one `get()` may be in flight, and a second one is refused as
 * NotAllowedError, which `loginWithPasskey` maps to the silent `PasskeyCancelled`.
 *
 * `available` gates the explicit button: without a platform authenticator it opens an empty picker.
 */
export function useConditionalPasskey(opts: {
	skip: boolean
	onLogin: (body: LoginBody) => void
}): { available: boolean; abort: () => void } {
	const [available, setAvailable] = React.useState(false)
	const pending = React.useRef<AbortController | null>(null)
	// A ref, so an inline `onLogin` does not restart the pending `get()` on every render.
	const onLogin = React.useRef(opts.onLogin)
	onLogin.current = opts.onLogin

	React.useEffect(() => {
		if (opts.skip) return
		const ac = new AbortController()
		pending.current = ac
		let live = true
		void (async () => {
			setAvailable(await platformAuthenticatorAvailable())
			if (!(await conditionalUiAvailable())) return
			try {
				const body = await loginWithPasskey({ conditional: true, signal: ac.signal })
				if (live) onLogin.current(body)
			} catch {
				// Opportunistic: a refusal, a dismissed picker or this abort must all leave the
				// password form untouched. The explicit button reports errors; this does not.
			}
		})()
		return () => {
			live = false
			ac.abort()
		}
	}, [opts.skip])

	const abort = React.useCallback(() => pending.current?.abort(), [])
	return { available, abort }
}

/** Whether to offer "Use a passkey", re-read each time `enabled` turns true (a dialog opening). */
export function usePasskeyAvailable(enabled: boolean): boolean {
	const [available, setAvailable] = React.useState(false)
	React.useEffect(() => {
		if (enabled) void platformAuthenticatorAvailable().then(setAvailable)
	}, [enabled])
	return available
}

// vim: ts=4
