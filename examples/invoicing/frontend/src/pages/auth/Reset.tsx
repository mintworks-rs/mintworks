// SPDX-License-Identifier: MIT-0
import * as React from 'react'
import { useNavigate, useSearchParams } from 'react-router-dom'

import { api } from '@mintworks/client'
import { AuthCard, Button, ErrorBanner, Field, Input } from '~/components/ui'
import { useT } from '~/i18n'

/** No `code`/`recoveryCode` branch: this screen is reached from a mailed link, and the
 *  second factor is asked for at sign-in, not here. */
export function Reset() {
	const { t, err } = useT()
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
			setError(err(e))
		} finally {
			setBusy(false)
		}
	}

	if (!token) {
		return (
			<AuthCard title={t('auth.reset.title')}>
				<ErrorBanner message={t('auth.reset.noToken')} />
			</AuthCard>
		)
	}

	return (
		<AuthCard title={t('auth.reset.title')}>
			<form onSubmit={submit} className="flex flex-col gap-4">
				<ErrorBanner message={error} />
				<Field label={t('auth.reset.new')} htmlFor="password" required>
					<Input
						type="password"
						value={password}
						autoComplete="new-password"
						onChange={(e) => setPassword(e.target.value)}
						required
					/>
				</Field>
				<Button type="submit" loading={busy}>
					{t('auth.reset.submit')}
				</Button>
			</form>
		</AuthCard>
	)
}

// vim: ts=4
