import type * as React from 'react'
import { Navigate } from 'react-router-dom'

import { useAuth } from '~/auth/AuthContext'
import { Button, ErrorBanner, PageSpinner } from '~/components/ui'
import { ConsentRequired } from '~/pages/ConsentRequired'

export function ProtectedRoute({ children }: { children: React.ReactNode }) {
	const { me, loading, error, reload } = useAuth()
	if (loading) return <PageSpinner />
	// A 5xx or a dropped connection on /api/auth/me says nothing about the session, so it must
	// not read as "anonymous": redirecting here signed a user out over one bad response at boot.
	if (!me && error)
		return (
			<div className="mx-auto mt-16 max-w-sm space-y-3 px-4">
				<ErrorBanner message={error} />
				<Button onClick={() => void reload().catch(() => {})}>Try again</Button>
			</div>
		)
	if (!me) return <Navigate to="/login" replace />
	// The backend gates every application route (example/backend/src/routes.rs); without this
	// the user gets E-AUTH-CONSENT-REQUIRED on every screen and no way to resolve it.
	if (me.consentsRequired.length > 0) return <ConsentRequired kinds={me.consentsRequired} />
	return <>{children}</>
}

// vim: ts=4
