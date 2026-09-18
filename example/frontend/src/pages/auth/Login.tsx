import * as React from 'react'
import { Link, Navigate, useNavigate } from 'react-router-dom'

import { errMsg } from '~/api/client'
import { useAuth } from '~/auth/AuthContext'
import { AuthCard, Button, ErrorBanner, Field, Input } from '~/components/ui'

/** No TOTP branch: the framework's second-factor routes stay mounted, the demo
 *  never enrols one, so `POST /api/auth/login/totp` is unreachable from here. */
export function Login() {
	const { me, login } = useAuth()
	const navigate = useNavigate()
	const [email, setEmail] = React.useState('')
	const [password, setPassword] = React.useState('')
	const [error, setError] = React.useState<string | null>(null)
	const [busy, setBusy] = React.useState(false)

	if (me) return <Navigate to="/" replace />

	async function submit(ev: React.FormEvent) {
		ev.preventDefault()
		setError(null)
		setBusy(true)
		try {
			await login(email, password)
			navigate('/', { replace: true })
		} catch (e) {
			setError(errMsg(e))
		} finally {
			setBusy(false)
		}
	}

	return (
		<AuthCard title="Sign in">
			<form onSubmit={submit} className="flex flex-col gap-4">
				<ErrorBanner message={error} />
				<Field label="Email" htmlFor="email" required>
					<Input
						type="email"
						value={email}
						autoComplete="username"
						onChange={(e) => setEmail(e.target.value)}
						required
					/>
				</Field>
				<Field label="Password" htmlFor="password" required>
					<Input
						type="password"
						value={password}
						autoComplete="current-password"
						onChange={(e) => setPassword(e.target.value)}
						required
					/>
				</Field>
				<Button type="submit" loading={busy}>
					Sign in
				</Button>
			</form>
			<p className="mt-6 text-sm text-slate-500">
				<Link to="/register" className="text-brand-700 hover:underline">
					Create an account
				</Link>
				{' · '}
				<Link to="/password/reset-request" className="text-brand-700 hover:underline">
					Forgot your password?
				</Link>
			</p>
		</AuthCard>
	)
}

// vim: ts=4
