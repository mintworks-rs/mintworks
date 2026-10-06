import * as React from 'react'
import { Link } from 'react-router-dom'

import { api, solvePow } from '@mintworks/client'
import { AuthCard, Button, ErrorBanner, Field, Input } from '~/components/ui'
import { useT } from '~/i18n'

export function ResetRequest() {
	const { t, err } = useT()
	const [email, setEmail] = React.useState('')
	const [sent, setSent] = React.useState(false)
	const [error, setError] = React.useState<string | null>(null)
	const [attempts, setAttempts] = React.useState<number | null>(null)
	const [busy, setBusy] = React.useState(false)
	// The PoW search is unbounded, so an abandoned form has to stop it: without this, leaving
	// mid-solve spins a core for the life of the tab.
	const solving = React.useRef<AbortController | null>(null)
	React.useEffect(() => () => solving.current?.abort(), [])

	async function submit(ev: React.FormEvent) {
		ev.preventDefault()
		setError(null)
		setBusy(true)
		try {
			solving.current = new AbortController()
			const pow = await solvePow('password-reset', setAttempts, solving.current.signal)
			await api.post('/api/auth/password/reset-request', { email, pow })
			setSent(true)
		} catch (e) {
			setError(err(e))
		} finally {
			setAttempts(null)
			setBusy(false)
		}
	}

	if (sent) {
		return (
			<AuthCard title={t('auth.checkEmail')}>
				{/* The route answers the same way for an unknown address, so this screen must
				    not confirm that the account exists. */}
				<p className="text-sm text-fg-muted">{t('auth.reset.sent')}</p>
				<p className="mt-6 text-sm text-fg-muted">
					<Link to="/login" className="text-accent hover:underline">
						{t('auth.backToSignIn')}
					</Link>
				</p>
			</AuthCard>
		)
	}

	return (
		<AuthCard title={t('auth.reset.request')}>
			<form onSubmit={submit} className="flex flex-col gap-4">
				<ErrorBanner message={error} />
				<Field label={t('common.email')} htmlFor="email" required>
					<Input
						type="email"
						value={email}
						autoComplete="email"
						onChange={(e) => setEmail(e.target.value)}
						required
					/>
				</Field>
				<Button type="submit" loading={busy} aria-live="polite">
					{attempts === null ? t('auth.reset.send') : t('auth.pow', { n: attempts })}
				</Button>
			</form>
		</AuthCard>
	)
}

// vim: ts=4
