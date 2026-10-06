// SPDX-License-Identifier: MIT-0
import { useEffect, useState } from 'react'
import { Link, useParams } from 'react-router-dom'

import type { QrDetails } from '@mintworks/client'
import { errMsg, qrDetails, respondQr, useAuth } from '@mintworks/client'
import { AuthCard, Button, ErrorBanner, Input, PageSpinner } from '~/components/ui'

/**
 * The phone's end of QR login: it approves a session that belongs to *another* browser, so the
 * tokens go to that browser's long poll and this device's own session is untouched.
 *
 * The link is opened from a camera app, so this route is outside the shell and the signed-out
 * case is a real state with its own screen — a redirect to /login would lose the code.
 */
export function QrApprove() {
	const { sessionId = '' } = useParams()
	const { me, loading } = useAuth()
	const [details, setDetails] = useState<QrDetails | null>(null)
	const [code, setCode] = useState('')
	const [error, setError] = useState<string | null>(null)
	const [busy, setBusy] = useState(false)
	const [answered, setAnswered] = useState<'approved' | 'denied' | null>(null)

	useEffect(() => {
		if (!me) return
		let live = true
		qrDetails(sessionId)
			.then((d) => live && setDetails(d))
			.catch((e) => live && setError(errMsg(e)))
		return () => {
			live = false
		}
	}, [me, sessionId])

	if (loading) return <PageSpinner />
	if (!me)
		return (
			<AuthCard title="Approve a sign-in">
				<p className="text-sm text-slate-600">
					Sign in on this device first, then open the link again.
				</p>
				<Link
					to="/login"
					className="mt-4 inline-block text-sm text-brand-700 hover:underline"
				>
					Sign in
				</Link>
			</AuthCard>
		)

	async function answer(approved: boolean) {
		setBusy(true)
		setError(null)
		try {
			await respondQr(sessionId, approved, code)
			setAnswered(approved ? 'approved' : 'denied')
		} catch (e) {
			setError(errMsg(e))
		} finally {
			setBusy(false)
		}
	}

	return (
		<AuthCard title="Approve a sign-in">
			<ErrorBanner message={error} />
			<div aria-live="polite">
				{answered !== null ? (
					<p className="text-sm text-slate-700">
						{answered === 'approved'
							? 'Approved. The other device is signed in.'
							: 'Denied. Nothing was signed in.'}
					</p>
				) : details === null ? (
					<p className="text-sm text-slate-500">Reading the request…</p>
				) : (
					<>
						<p className="text-sm text-slate-600">
							Another browser is asking to sign in as{' '}
							<strong className="text-slate-900">{me.account.email}</strong>.
						</p>
						<dl className="mt-4 space-y-1 text-sm">
							<div className="flex justify-between gap-4">
								<dt className="text-slate-500">Browser</dt>
								<dd className="text-slate-800">{details.browser}</dd>
							</div>
							<div className="flex justify-between gap-4">
								<dt className="text-slate-500">Address</dt>
								<dd className="text-slate-800">{details.ip ?? '—'}</dd>
							</div>
						</dl>
						<p className="mt-4 text-sm text-slate-600">
							Type the code shown on the other screen.
						</p>
						<Input
							aria-label="Match code"
							autoCapitalize="characters"
							autoComplete="off"
							maxLength={6}
							value={code}
							onChange={(e) => setCode(e.target.value)}
							className="mt-2 font-mono text-lg tracking-[0.3em] uppercase"
						/>
						{/* Equal weight, neither autofocused, no confirmshaming: Deny is not the
						    smaller or the greyer of the two. */}
						<div className="mt-6 flex gap-2">
							<Button
								loading={busy}
								disabled={code.trim().length !== 6}
								onClick={() => void answer(true)}
							>
								Approve
							</Button>
							<Button loading={busy} onClick={() => void answer(false)}>
								Deny
							</Button>
						</div>
					</>
				)}
			</div>
		</AuthCard>
	)
}

// vim: ts=4
