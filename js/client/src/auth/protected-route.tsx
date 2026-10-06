// SPDX-License-Identifier: MPL-2.0
import type * as React from 'react'
import { Navigate } from 'react-router-dom'

import type { LegalKind } from '../types'
import { useAuth } from './context'

/**
 * The gate's branching, without its markup: the package ships no components, so every
 * rendered state is the application's. Passing nothing but `children` still works — the
 * states then render blank rather than a spinner or a retry.
 */
export interface ProtectedRouteProps {
	children: React.ReactNode
	/** Where an anonymous visitor is sent. */
	loginPath?: string
	/** Rendered while `GET /api/auth/me` is in flight. */
	fallback?: React.ReactNode
	onError?: (error: string, retry: () => void) => React.ReactNode
	consentGate?: (kinds: LegalKind[]) => React.ReactNode
}

export function ProtectedRoute({
	children,
	loginPath = '/login',
	fallback = null,
	onError,
	consentGate
}: ProtectedRouteProps) {
	const { me, loading, error, reload } = useAuth()
	if (loading) return <>{fallback}</>
	// A 5xx or a dropped connection on /api/auth/me says nothing about the session, so it must
	// not read as "anonymous": redirecting here signed a user out over one bad response at boot.
	if (!me && error) return <>{onError?.(error, () => void reload().catch(() => {}))}</>
	if (!me) return <Navigate to={loginPath} replace />
	// The backend gates every application route; without a gate the user gets
	// E-AUTH-CONSENT-REQUIRED on every screen and no way to resolve it.
	if (me.consentsRequired.length > 0) return <>{consentGate?.(me.consentsRequired)}</>
	return <>{children}</>
}

// vim: ts=4
