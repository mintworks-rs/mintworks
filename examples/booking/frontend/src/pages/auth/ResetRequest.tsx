import * as React from 'react'
import { Link } from 'react-router-dom'

import { api, errMsg, solvePow } from '@saas-framework/client'
import { AuthCard, Button, ErrorBanner, Field, Input } from '~/components/ui'

export function ResetRequest() {
	const [email, setEmail] = React.useState('')
	const [sent, setSent] = React.useState(false)
	const [error, setError] = React.useState<string | null>(null)
	const [attempts, setAttempts] = React.useState<number | null>(null)
	const [busy, setBusy] = React.useState(false)
	// The PoW search is unbounded, so an abandoned form has to stop it: without this,
	// leaving mid-solve spins a core for the life of the tab.
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
			setError(errMsg(e))
		} finally {
			setAttempts(null)
			setBusy(false)
		}
	}

	if (sent) {
		return (
			<AuthCard title="Check your email">
				{/* The route answers the same way for an unknown address, so this screen
				    must not confirm that the account exists. */}
				<p className="text-sm text-slate-600">
					If that address has an account, a reset link is on its way.
				</p>
				<p className="mt-6 text-sm text-slate-500">
					<Link to="/login" className="text-brand-700 hover:underline">
						Back to sign in
					</Link>
				</p>
			</AuthCard>
		)
	}

	return (
		<AuthCard title="Reset your password">
			<form onSubmit={submit} className="flex flex-col gap-4">
				<ErrorBanner message={error} />
				<Field label="Email" htmlFor="email" required>
					<Input
						type="email"
						value={email}
						autoComplete="email"
						onChange={(e) => setEmail(e.target.value)}
						required
					/>
				</Field>
				<Button type="submit" loading={busy} aria-live="polite">
					{attempts === null
						? 'Send reset link'
						: `Verifying you're human… (${attempts} tries)`}
				</Button>
			</form>
		</AuthCard>
	)
}

// vim: ts=4
