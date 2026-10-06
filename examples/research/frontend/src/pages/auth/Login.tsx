import * as React from 'react'
import { Link, Navigate, useNavigate } from 'react-router-dom'

import { ERRORS_EN, errText, useAuth } from '@mintworks/client'
import { AuthCard, Button, ErrorBanner, Field, Input } from '~/components/ui'

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
			setError(errText(e, ERRORS_EN))
		} finally {
			setBusy(false)
		}
	}

	return (
		<AuthCard title="Sign in">
			<ErrorBanner message={error} />
			<form onSubmit={submit} className="mt-3 flex flex-col gap-4">
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
			<p className="mt-6 text-sm text-fg-muted">
				<Link to="/register" className="text-accent hover:underline">
					Create an account
				</Link>
			</p>
		</AuthCard>
	)
}

// vim: ts=4
