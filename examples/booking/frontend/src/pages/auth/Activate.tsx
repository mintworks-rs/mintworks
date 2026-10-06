import * as React from 'react'
import { useNavigate, useSearchParams } from 'react-router-dom'

import type { LoginBody } from '@mintworks/client'
import { api, errMsg, useAuth } from '@mintworks/client'
import { AuthCard, Button, ErrorBanner, Field, Input } from '~/components/ui'

export function Activate() {
	const [params] = useSearchParams()
	const token = params.get('token') ?? ''
	const { adopt } = useAuth()
	const navigate = useNavigate()
	const [password, setPassword] = React.useState('')
	const [error, setError] = React.useState<string | null>(null)
	const [busy, setBusy] = React.useState(false)

	async function submit(ev: React.FormEvent) {
		ev.preventDefault()
		setError(null)
		setBusy(true)
		try {
			// Activation answers with the login body and sets the session cookies, so
			// the user lands signed in rather than back at the login form.
			adopt(await api.post<LoginBody>('/api/auth/activate', { token, password }))
			navigate('/', { replace: true })
		} catch (e) {
			setError(errMsg(e))
		} finally {
			setBusy(false)
		}
	}

	if (!token) {
		return (
			<AuthCard title="Activate your account">
				<ErrorBanner message="This link carries no activation token." />
			</AuthCard>
		)
	}

	return (
		<AuthCard title="Activate your account" subtitle="Choose a password to finish.">
			<form onSubmit={submit} className="flex flex-col gap-4">
				<ErrorBanner message={error} />
				<Field label="Password" htmlFor="password" required>
					<Input
						type="password"
						value={password}
						autoComplete="new-password"
						onChange={(e) => setPassword(e.target.value)}
						required
					/>
				</Field>
				<Button type="submit" loading={busy}>
					Activate
				</Button>
			</form>
		</AuthCard>
	)
}

// vim: ts=4
