// SPDX-License-Identifier: MIT-0
import * as React from 'react'
import { useNavigate, useSearchParams } from 'react-router-dom'

import type { LoginBody } from '@mintworks/client'
import { api, useAuth } from '@mintworks/client'
import { AuthCard, Button, ErrorBanner, Field, Input } from '~/components/ui'
import { useT } from '~/i18n'

export function Activate() {
	const { t, err } = useT()
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
			// Activation answers with the login body and sets the session cookies, so the user
			// lands signed in rather than back at the login form.
			adopt(await api.post<LoginBody>('/api/auth/activate', { token, password }))
			navigate('/', { replace: true })
		} catch (e) {
			setError(err(e))
		} finally {
			setBusy(false)
		}
	}

	if (!token) {
		return (
			<AuthCard title={t('auth.activate')}>
				<ErrorBanner message={t('auth.activate.noToken')} />
			</AuthCard>
		)
	}

	return (
		<AuthCard title={t('auth.activate')} subtitle={t('auth.activate.subtitle')}>
			<form onSubmit={submit} className="flex flex-col gap-4">
				<ErrorBanner message={error} />
				<Field label={t('common.password')} htmlFor="password" required>
					<Input
						type="password"
						value={password}
						autoComplete="new-password"
						onChange={(e) => setPassword(e.target.value)}
						required
					/>
				</Field>
				<Button type="submit" loading={busy}>
					{t('auth.activate.submit')}
				</Button>
			</form>
		</AuthCard>
	)
}

// vim: ts=4
