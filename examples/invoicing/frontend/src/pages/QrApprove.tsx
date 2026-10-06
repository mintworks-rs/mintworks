import { useEffect, useState } from 'react'
import { Link, useParams } from 'react-router-dom'

import type { QrDetails } from '@mintworks/client'
import { qrDetails, respondQr, useAuth } from '@mintworks/client'
import { AuthCard, Button, ErrorBanner, Input, PageSpinner } from '~/components/ui'
import { useT } from '~/i18n'

/**
 * The phone's end of QR login: it approves a session that belongs to *another* browser, so the
 * tokens go to that browser's long poll and this device's own session is untouched.
 *
 * The link is opened from a camera app, so this route is outside the shell and the signed-out
 * case is a real state with its own screen — a redirect to /login would lose the code.
 */
export function QrApprove() {
	const { t, err } = useT()
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
			.catch((e) => live && setError(err(e)))
		return () => {
			live = false
		}
	}, [me, sessionId, err])

	if (loading) return <PageSpinner />
	if (!me)
		return (
			<AuthCard title={t('qr.approve')}>
				<p className="text-sm text-fg-muted">{t('qr.signInFirst')}</p>
				<Link to="/login" className="mt-4 inline-block text-sm text-accent hover:underline">
					{t('auth.signIn')}
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
			setError(err(e))
		} finally {
			setBusy(false)
		}
	}

	return (
		<AuthCard title={t('qr.approve')}>
			<ErrorBanner message={error} />
			<div aria-live="polite">
				{answered !== null ? (
					<p className="text-sm text-fg">
						{answered === 'approved' ? t('qr.approved') : t('qr.deniedDone')}
					</p>
				) : details === null ? (
					<p className="text-sm text-fg-muted">{t('qr.reading')}</p>
				) : (
					<>
						<p className="text-sm text-fg-muted">
							{t('qr.asks', { email: me.account.email })}
						</p>
						<dl className="mt-4 space-y-1 text-sm">
							<div className="flex justify-between gap-4">
								<dt className="text-fg-muted">{t('qr.browser')}</dt>
								<dd className="text-fg">{details.browser}</dd>
							</div>
							<div className="flex justify-between gap-4">
								<dt className="text-fg-muted">{t('qr.address')}</dt>
								<dd className="text-fg">{details.ip ?? t('common.none')}</dd>
							</div>
						</dl>
						<p className="mt-4 text-sm text-fg-muted">{t('qr.typeCode')}</p>
						<Input
							aria-label={t('qr.matchCode')}
							autoCapitalize="characters"
							autoComplete="off"
							maxLength={6}
							value={code}
							onChange={(e) => setCode(e.target.value)}
							className="mt-2 font-mono text-lg uppercase tracking-[0.3em]"
						/>
						{/* Equal weight, neither autofocused, no confirmshaming: Deny is not the
						    smaller or the greyer of the two. */}
						<div className="mt-6 flex gap-2">
							<Button
								loading={busy}
								disabled={code.trim().length !== 6}
								onClick={() => void answer(true)}
							>
								{t('qr.approve')}
							</Button>
							<Button loading={busy} onClick={() => void answer(false)}>
								{t('qr.deny')}
							</Button>
						</div>
					</>
				)}
			</div>
		</AuthCard>
	)
}

// vim: ts=4
