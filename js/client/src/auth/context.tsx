// SPDX-License-Identifier: MPL-2.0
import { useQueryClient } from '@tanstack/react-query'
import * as React from 'react'

import { api, errMsg, probeSession, ServerError, setOnAuthLost } from '../http'
import type { LoginBody } from '../types'
import { solvePow } from '../pow'

interface AuthApi {
	/** The login body minus its tokens: account, org, orgs, consentsRequired. */
	me: Session | null
	loading: boolean
	/** Set when the last `/api/auth/me` failed for a reason that is *not* "no session":
	 *  a 5xx or a dropped connection. `me` is untouched, so the shell must offer a retry
	 *  rather than treat the user as anonymous. */
	error: string | null
	login: (email: string, password: string) => Promise<void>
	logout: () => Promise<void>
	/** Re-read GET /api/auth/me — after accepting consents, say. */
	reload: (signal?: AbortSignal) => Promise<void>
	/** Adopt a login body a route already received (activation returns one). */
	adopt: (body: LoginBody) => void
}

/** The login body minus its tokens. The cookies carry the session; these three exist for
 *  non-browser clients. Dropped at the boundary so an XSS cannot read a 7-day credential out
 *  of the context, where it would outlive the tab. */
export type Session = Omit<LoginBody, 'accessToken' | 'refreshToken' | 'expiresIn'>

const session = ({ accessToken, refreshToken, expiresIn, ...rest }: LoginBody): Session => rest

const AuthCtx = React.createContext<AuthApi | null>(null)

export function AuthProvider({ children }: { children: React.ReactNode }) {
	const [me, setMe] = React.useState<Session | null>(null)
	const [loading, setLoading] = React.useState(true)
	const [error, setError] = React.useState<string | null>(null)
	const qc = useQueryClient()
	// The PoW search is unbounded, so an abandoned sign-in has to stop it — and this is the
	// path a user reaches only after `auth.pow_after_failures` failures, the likeliest to be
	// abandoned. Same shape as Register.tsx and ResetRequest.tsx.
	const solving = React.useRef<AbortController | null>(null)
	React.useEffect(() => () => solving.current?.abort(), [])

	// The cache is keyed by nothing account-specific, so anything left in it after a session
	// ends is the previous account's data on the next one's screens until each refetch lands.
	const endSession = React.useCallback(() => {
		setMe(null)
		setError(null)
		qc.clear()
	}, [qc])

	React.useEffect(() => {
		setOnAuthLost(endSession)
		return () => setOnAuthLost(null)
	}, [endSession])

	// There is no token in JavaScript to inspect, so the only way to learn whether
	// a session exists is to ask: the HttpOnly cookie decides, and a 401 here is
	// the ordinary anonymous case, not an error worth surfacing.
	const reload = React.useCallback(async (signal?: AbortSignal) => {
		try {
			setMe(session(await api.get<LoginBody>('/api/auth/me', signal)))
			setError(null)
		} catch (e) {
			// An abort is this component unmounting, not a dead session — collapsing it into
			// `setMe(null)` logged the user out of the render that replaced this one.
			if (signal?.aborted) throw e
			// Only a refusal proves there is no session. `client.ts` keeps a transport failure
			// distinct from a rejected response for exactly this reason, and discarding that
			// here sent a signed-in user to /login over one 500 at boot.
			if (e instanceof ServerError && (e.httpStatus === 401 || e.httpStatus === 403)) {
				setMe(null)
				setError(null)
				return
			}
			setError(errMsg(e))
			throw e
		}
	}, [])

	React.useEffect(() => {
		const ac = new AbortController()
		// Refresh first: with no cookie at all it is one 204, where `/me` alone logged two red
		// 401s. Only that 204 skips `/me`; a refused or failed refresh can leave a valid access cookie.
		probeSession()
			.then((ask) => {
				if (ac.signal.aborted) return
				if (ask) return reload(ac.signal)
				setMe(null)
				setError(null)
			})
			.catch(() => {})
			.finally(() => {
				if (!ac.signal.aborted) setLoading(false)
			})
		return () => ac.abort()
	}, [reload])

	const login = React.useCallback(
		async (email: string, password: string) => {
			let body: LoginBody
			try {
				body = await api.post<LoginBody>('/api/auth/login', { email, password })
			} catch (e) {
				// `pow` turns mandatory once this address has spent `auth.pow_after_failures`
				// tokens from the failed-login bucket; the server says so with E-CORE-POW
				// rather than up front, so the only way to find out is to be told.
				if (!(e instanceof ServerError) || e.errCode !== 'E-CORE-POW') throw e
				solving.current?.abort()
				solving.current = new AbortController()
				const pow = await solvePow('login', undefined, solving.current.signal)
				body = await api.post<LoginBody>('/api/auth/login', { email, password, pow })
			}
			// Cleared on login too: a second account can sign in without the first logging out.
			qc.clear()
			setMe(session(body))
			setError(null)
		},
		[qc]
	)

	const logout = React.useCallback(async () => {
		try {
			await api.post('/api/auth/logout')
		} finally {
			endSession()
		}
	}, [endSession])

	// Stable identity, like `login`/`logout`/`reload`: an inline arrow here changed on every
	// `loading` flip, and a consumer's effect listing it as a dependency restarted — minting a
	// second QR session, aborting the passkey picker — whenever boot finished.
	const adopt = React.useCallback((b: LoginBody) => setMe(session(b)), [])

	const value = React.useMemo<AuthApi>(
		() => ({
			me,
			loading,
			error,
			login,
			logout,
			reload,
			adopt
		}),
		[me, loading, error, login, logout, reload, adopt]
	)
	return <AuthCtx.Provider value={value}>{children}</AuthCtx.Provider>
}

export function useAuth(): AuthApi {
	const api = React.useContext(AuthCtx)
	if (!api) throw new Error('useAuth used outside AuthProvider')
	return api
}

// vim: ts=4
