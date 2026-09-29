import { useQuery } from '@tanstack/react-query'
import * as React from 'react'
import { Link, useNavigate } from 'react-router-dom'

import type { LegalDoc, LegalKind } from '@saas-framework/client'
import { ERRORS_EN, api, errText, solvePow } from '@saas-framework/client'
import { AuthCard, Button, ErrorBanner, Field, Input, Modal, PageSpinner } from '~/components/ui'

const KINDS: LegalKind[] = ['TOS', 'PRIVACY']

export function Register() {
	const navigate = useNavigate()
	const [email, setEmail] = React.useState('')
	const [name, setName] = React.useState('')
	const [accepted, setAccepted] = React.useState(false)
	const [error, setError] = React.useState<string | null>(null)
	const [attempts, setAttempts] = React.useState<number | null>(null)
	const [busy, setBusy] = React.useState(false)
	const [reading, setReading] = React.useState<LegalDoc | null>(null)
	// The PoW search is unbounded: leaving mid-solve would spin a core for the life of the tab.
	const solving = React.useRef<AbortController | null>(null)
	React.useEffect(() => () => solving.current?.abort(), [])

	const docs = useQuery({
		queryKey: ['legal'],
		queryFn: ({ signal }) =>
			Promise.all(KINDS.map((k) => api.get<LegalDoc>(`/api/legal/${k}`, signal)))
	})

	async function submit(ev: React.FormEvent) {
		ev.preventDefault()
		if (!docs.data) return
		setError(null)
		setBusy(true)
		try {
			solving.current = new AbortController()
			const pow = await solvePow('register', setAttempts, solving.current.signal)
			// The consent record is about the exact version presented, hence the fetched pairs.
			await api.post('/api/auth/register', {
				email,
				name: name || null,
				locale: 'en',
				consents: docs.data.map((d) => ({ kind: d.kind, version: d.version })),
				pow
			})
			navigate('/check-email', { replace: true })
		} catch (e) {
			setError(errText(e, ERRORS_EN))
		} finally {
			setAttempts(null)
			setBusy(false)
		}
	}

	if (docs.isLoading) return <PageSpinner />
	if (docs.error) {
		return (
			<AuthCard title="Create an account">
				<ErrorBanner message={errText(docs.error, ERRORS_EN)} />
			</AuthCard>
		)
	}

	return (
		<AuthCard
			title="Create an account"
			subtitle="You will set a password from the activation link."
		>
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
				<Field label="Name" htmlFor="name" hint="Optional.">
					<Input
						type="text"
						value={name}
						autoComplete="name"
						onChange={(e) => setName(e.target.value)}
					/>
				</Field>
				<label className="flex items-start gap-2 text-sm text-fg">
					<input
						type="checkbox"
						checked={accepted}
						onChange={(e) => setAccepted(e.target.checked)}
						className="mt-1 h-4 w-4 accent-accent"
						required
					/>
					<span>
						I accept the{' '}
						{docs.data?.map((d, i) => (
							<React.Fragment key={d.kind}>
								{i > 0 && ' and the '}
								<button
									type="button"
									// `preventDefault`, or the click also toggles the checkbox.
									onClick={(e) => {
										e.preventDefault()
										setReading(d)
									}}
									className="text-accent hover:underline"
								>
									{d.title}
								</button>{' '}
								<span className="text-fg-muted">(v{d.version})</span>
							</React.Fragment>
						))}
					</span>
				</label>
				<Button type="submit" loading={busy} disabled={!accepted} aria-live="polite">
					{attempts === null
						? 'Create account'
						: `Verifying you're human… (${attempts} tries)`}
				</Button>
			</form>
			<Modal
				open={reading !== null}
				onClose={() => setReading(null)}
				title={reading ? `${reading.title} (v${reading.version})` : ''}
			>
				{/* Rendered as text: the requirement is the verbatim wording. */}
				<div className="max-h-[60vh] overflow-y-auto whitespace-pre-wrap text-sm text-fg">
					{reading?.body}
				</div>
				<div className="mt-6 flex justify-end">
					<Button variant="secondary" onClick={() => setReading(null)}>
						Close
					</Button>
				</div>
			</Modal>

			<p className="mt-6 text-sm text-fg-muted">
				<Link to="/login" className="text-accent hover:underline">
					Already have an account?
				</Link>
			</p>
		</AuthCard>
	)
}

// vim: ts=4
