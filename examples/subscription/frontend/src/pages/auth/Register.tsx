// SPDX-License-Identifier: MIT-0
import { useQuery } from '@tanstack/react-query'
import * as React from 'react'
import { Link, useNavigate, useSearchParams } from 'react-router-dom'

import type { LegalDoc, LegalKind } from '@mintworks/client'
import { api, errMsg, register, solvePow } from '@mintworks/client'
import { Modal } from '~/components/Modal'
import { AuthCard, Button, ErrorBanner, Field, Input, PageSpinner } from '~/components/ui'

const KINDS: LegalKind[] = ['TOS', 'PRIVACY']

export function Register() {
	const navigate = useNavigate()
	// Set by `/r/:code`: a signup, affiliate or org-invite code, redeemed at activation.
	const ref = useSearchParams()[0].get('ref') ?? undefined
	const [email, setEmail] = React.useState('')
	const [name, setName] = React.useState('')
	const [accepted, setAccepted] = React.useState(false)
	const [error, setError] = React.useState<string | null>(null)
	const [attempts, setAttempts] = React.useState<number | null>(null)
	const [busy, setBusy] = React.useState(false)
	const [reading, setReading] = React.useState<LegalDoc | null>(null)
	// The PoW search is unbounded, so an abandoned form has to stop it: without this,
	// leaving mid-solve spins a core for the life of the tab.
	const solving = React.useRef<AbortController | null>(null)
	React.useEffect(() => () => solving.current?.abort(), [])

	const docs = useQuery({
		queryKey: ['legal'],
		queryFn: () => Promise.all(KINDS.map((k) => api.get<LegalDoc>(`/api/legal/${k}`)))
	})

	async function submit(ev: React.FormEvent) {
		ev.preventDefault()
		if (!docs.data) return
		setError(null)
		setBusy(true)
		try {
			solving.current = new AbortController()
			const pow = await solvePow('register', setAttempts, solving.current.signal)
			// The consent record is about the exact version presented above, which is
			// why the accepted pairs come from the fetched documents and not a constant.
			await register({
				email,
				name: name || undefined,
				locale: 'en',
				consents: docs.data.map((d) => ({ kind: d.kind, version: d.version })),
				pow,
				refCode: ref
			})
			navigate('/register/check-email', { replace: true })
		} catch (e) {
			setError(errMsg(e))
		} finally {
			setAttempts(null)
			setBusy(false)
		}
	}

	if (docs.isLoading) return <PageSpinner />
	if (docs.error) {
		return (
			<AuthCard title="Create an account">
				<ErrorBanner message={errMsg(docs.error)} />
			</AuthCard>
		)
	}

	return (
		<AuthCard
			title="Create an account"
			subtitle={
				ref
					? `Invitation code ${ref}. You will set a password from the activation link.`
					: 'You will set a password from the activation link.'
			}
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
				<label className="flex items-start gap-2 text-sm text-slate-700">
					<input
						type="checkbox"
						checked={accepted}
						onChange={(e) => setAccepted(e.target.checked)}
						className="mt-1 h-4 w-4 accent-brand-600"
						required
					/>
					<span>
						I accept the{' '}
						{docs.data?.map((d, i) => (
							<React.Fragment key={d.kind}>
								{i > 0 && ' and the '}
								{/* The document itself, not `/api/legal/{kind}` — that route
								    answers JSON, so accepting used to mean reading a browser's
								    JSON viewer. The consent record is about this exact text. */}
								<button
									type="button"
									// `preventDefault`, or the click also toggles the checkbox it
									// sits inside.
									onClick={(e) => {
										e.preventDefault()
										setReading(d)
									}}
									className="text-brand-700 hover:underline"
								>
									{d.title}
								</button>{' '}
								<span className="text-slate-500">(v{d.version})</span>
							</React.Fragment>
						))}
						.
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
				{/* Markdown from the server rendered as text: the requirement is the verbatim
				    wording, which neither needs a Markdown dependency nor raw HTML. */}
				<div className="max-h-[60vh] overflow-y-auto whitespace-pre-wrap text-sm text-slate-700">
					{reading?.body}
				</div>
				<div className="mt-6 flex justify-end">
					<Button variant="secondary" onClick={() => setReading(null)}>
						Close
					</Button>
				</div>
			</Modal>

			<p className="mt-6 text-sm text-slate-500">
				<Link to="/login" className="text-brand-700 hover:underline">
					Already have an account?
				</Link>
			</p>
		</AuthCard>
	)
}

// vim: ts=4
