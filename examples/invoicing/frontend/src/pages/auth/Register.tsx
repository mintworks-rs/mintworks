import { useQuery } from '@tanstack/react-query'
import * as React from 'react'
import { Link, useNavigate } from 'react-router-dom'

import type { LegalDoc, LegalKind } from '@mintworks/client'
import { api, solvePow } from '@mintworks/client'
import { Modal } from '~/components/Modal'
import { AuthCard, Button, ErrorBanner, Field, Input, PageSpinner } from '~/components/ui'
import { useT } from '~/i18n'

const KINDS: LegalKind[] = ['TOS', 'PRIVACY']

export function Register() {
	const { t, err, locale } = useT()
	const navigate = useNavigate()
	const [email, setEmail] = React.useState('')
	const [name, setName] = React.useState('')
	const [accepted, setAccepted] = React.useState(false)
	const [error, setError] = React.useState<string | null>(null)
	const [attempts, setAttempts] = React.useState<number | null>(null)
	const [busy, setBusy] = React.useState(false)
	const [reading, setReading] = React.useState<LegalDoc | null>(null)
	// The PoW search is unbounded, so an abandoned form has to stop it: without this, leaving
	// mid-solve spins a core for the life of the tab.
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
			// The consent record is about the exact version presented above, which is why the
			// accepted pairs come from the fetched documents and not a constant.
			await api.post('/api/auth/register', {
				email,
				name: name || null,
				locale,
				consents: docs.data.map((d) => ({ kind: d.kind, version: d.version })),
				pow
			})
			navigate('/register/check-email', { replace: true })
		} catch (e) {
			setError(err(e))
		} finally {
			setAttempts(null)
			setBusy(false)
		}
	}

	if (docs.isLoading) return <PageSpinner />
	if (docs.error) {
		return (
			<AuthCard title={t('auth.createAccount')}>
				<ErrorBanner message={err(docs.error)} />
			</AuthCard>
		)
	}

	return (
		<AuthCard title={t('auth.createAccount')} subtitle={t('auth.register.subtitle')}>
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
				<Field label={t('common.name')} htmlFor="name" hint={t('common.optional')}>
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
						{t('auth.register.accept')}{' '}
						{docs.data?.map((d, i) => (
							<React.Fragment key={d.kind}>
								{i > 0 && ` ${t('auth.register.and')} `}
								{/* The document itself, not `/api/legal/{kind}`, which answers
								    JSON. The consent record is about this exact text. */}
								<button
									type="button"
									// `preventDefault`, or the click also toggles the checkbox it
									// sits inside.
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
					{attempts === null ? t('auth.register.submit') : t('auth.pow', { n: attempts })}
				</Button>
			</form>
			<Modal
				open={reading !== null}
				onClose={() => setReading(null)}
				title={reading ? `${reading.title} (v${reading.version})` : ''}
			>
				{/* Markdown from the server rendered as text: the requirement is the verbatim
				    wording, which neither needs a Markdown dependency nor raw HTML. */}
				<div className="max-h-[60vh] overflow-y-auto whitespace-pre-wrap text-sm text-fg">
					{reading?.body}
				</div>
				<div className="mt-6 flex justify-end">
					<Button variant="secondary" onClick={() => setReading(null)}>
						{t('common.close')}
					</Button>
				</div>
			</Modal>

			<p className="mt-6 text-sm text-fg-muted">
				<Link to="/login" className="text-accent hover:underline">
					{t('auth.haveAccount')}
				</Link>
			</p>
		</AuthCard>
	)
}

// vim: ts=4
