import * as React from 'react'
import { useNavigate, useSearchParams } from 'react-router-dom'

import { api, errMsg } from '@mintworks/client'
import { AuthCard, Button, ErrorBanner, Field, Input } from '~/components/ui'

/** No `code`/`recoveryCode`: the demo enrols no second factor. */
export function Reset() {
	const [params] = useSearchParams()
	const token = params.get('token') ?? ''
	const navigate = useNavigate()
	const [password, setPassword] = React.useState('')
	const [error, setError] = React.useState<string | null>(null)
	const [busy, setBusy] = React.useState(false)

	async function submit(ev: React.FormEvent) {
		ev.preventDefault()
		setError(null)
		setBusy(true)
		try {
			await api.post('/api/auth/password/reset', { token, password })
			navigate('/login', { replace: true })
		} catch (e) {
			setError(errMsg(e))
		} finally {
			setBusy(false)
		}
	}

	if (!token) {
		return (
			<AuthCard title="Choose a new password">
				<ErrorBanner message="This link carries no reset token." />
			</AuthCard>
		)
	}

	return (
		<AuthCard title="Choose a new password">
			<form onSubmit={submit} className="flex flex-col gap-4">
				<ErrorBanner message={error} />
				<Field label="New password" htmlFor="password" required>
					<Input
						type="password"
						value={password}
						autoComplete="new-password"
						onChange={(e) => setPassword(e.target.value)}
						required
					/>
				</Field>
				<Button type="submit" loading={busy}>
					Set password
				</Button>
			</form>
		</AuthCard>
	)
}

// vim: ts=4
