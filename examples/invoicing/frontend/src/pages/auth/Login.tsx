import * as React from 'react'
import QRCode from 'react-qr-code'
import { Link, Navigate, useNavigate } from 'react-router-dom'

import type { LoginBody, QrInit, QrPending } from '@mintworks/client'
import {
	PasskeyCancelled,
	initQr,
	loginWithPasskey,
	pollQr,
	useAuth,
	useConditionalPasskey
} from '@mintworks/client'
import { AuthCard, Button, ErrorBanner, Field, Input } from '~/components/ui'
import { useT } from '~/i18n'

/** `qr::TTL_SECONDS`. The countdown is the client's copy of a server-side clock, so a drift of
 *  a second or two only ever means the loop stops one window early; the server answers `404`,
 *  which `pollQr` maps to `expired`, about a second later. */
const QR_TTL_SECONDS = 120

export function Login() {
	const { t, err } = useT()
	const { me, login, adopt } = useAuth()
	const navigate = useNavigate()
	const [email, setEmail] = React.useState('')
	const [password, setPassword] = React.useState('')
	const [error, setError] = React.useState<string | null>(null)
	const [busy, setBusy] = React.useState(false)
	const [qr, setQr] = React.useState(false)
	const { available: passkey, abort } = useConditionalPasskey({
		skip: !!me || qr,
		onLogin: adopt
	})

	// `me` is set by `adopt` for both passkey and QR, so the redirect is the same one the
	// session read gets.
	if (me) return <Navigate to="/" replace />

	async function submit(ev: React.FormEvent) {
		ev.preventDefault()
		abort()
		setError(null)
		setBusy(true)
		try {
			await login(email, password)
			navigate('/', { replace: true })
		} catch (e) {
			setError(err(e))
		} finally {
			setBusy(false)
		}
	}

	async function withPasskey() {
		setError(null)
		setBusy(true)
		// Only one `get()` may be in flight; the conditional one is still pending from mount.
		abort()
		try {
			adopt(await loginWithPasskey())
		} catch (e) {
			if (!(e instanceof PasskeyCancelled)) setError(err(e))
		} finally {
			setBusy(false)
		}
	}

	return (
		<AuthCard title={t('auth.signIn')}>
			<ErrorBanner message={error} />
			{qr ? (
				<QrPanel onUsePassword={() => setQr(false)} />
			) : (
				<form onSubmit={submit} className="mt-3 flex flex-col gap-4">
					<Field label={t('common.email')} htmlFor="email" required>
						<Input
							type="email"
							value={email}
							// `webauthn` is the whole conditional-UI contract: the browser only
							// offers a passkey in a field carrying it.
							autoComplete="username webauthn"
							onChange={(e) => setEmail(e.target.value)}
							required
						/>
					</Field>
					<Field label={t('common.password')} htmlFor="password" required>
						<Input
							type="password"
							value={password}
							autoComplete="current-password"
							onChange={(e) => setPassword(e.target.value)}
							required
						/>
					</Field>
					<Button type="submit" loading={busy}>
						{t('auth.signIn')}
					</Button>
					{passkey && (
						<Button
							type="button"
							variant="secondary"
							disabled={busy}
							onClick={() => void withPasskey()}
						>
							{t('auth.signInPasskey')}
						</Button>
					)}
					<Button
						type="button"
						variant="ghost"
						disabled={busy}
						onClick={() => setQr(true)}
					>
						{t('auth.signInQr')}
					</Button>
				</form>
			)}
			<p className="mt-6 text-sm text-fg-muted">
				<Link to="/register" className="text-accent hover:underline">
					{t('auth.createAccount')}
				</Link>
				{' · '}
				<Link to="/password/reset-request" className="text-accent hover:underline">
					{t('auth.forgot')}
				</Link>
			</p>
		</AuthCard>
	)
}

/**
 * The desktop's end of QR login. The phone scans the code and approves, and this browser
 * collects the session — this page is never signed in by anything it sends.
 *
 * "Use password instead" is visible throughout, because a 2-minute window on a screen someone
 * is standing in front of is short enough that waiting it out is not an answer.
 */
function QrPanel({ onUsePassword }: { onUsePassword: () => void }) {
	const { t, err } = useT()
	const { adopt } = useAuth()
	const [session, setSession] = React.useState<QrInit | null>(null)
	const [status, setStatus] = React.useState<QrPending | 'waiting'>('waiting')
	const [left, setLeft] = React.useState(QR_TTL_SECONDS)
	const [error, setError] = React.useState<string | null>(null)
	// Bumped by "Show a new code": a fresh run of the effect is the whole restart path.
	const [nonce, setNonce] = React.useState(0)

	React.useEffect(() => {
		const ac = new AbortController()
		let live = true
		let timer: number | undefined
		setSession(null)
		setStatus('waiting')
		setLeft(QR_TTL_SECONDS)
		setError(null)
		void (async () => {
			try {
				const started = await initQr()
				if (!live) return
				setSession(started)
				timer = window.setInterval(() => setLeft((s) => (s > 0 ? s - 1 : 0)), 1000)
				// The server's own TTL, measured from the same instant as the countdown.
				const deadline = Date.now() + QR_TTL_SECONDS * 1000
				for (;;) {
					const outcome = await pollQr(started.sessionId, started.secret, ac.signal)
					if (!live) return
					if (typeof outcome !== 'string') {
						adopt(outcome as LoginBody)
						return
					}
					// `pending` is the long poll timing out, not an answer: poll again until the
					// session's TTL has passed, then fall to the "expired" branch.
					if (outcome !== 'pending' || Date.now() >= deadline) {
						setStatus(outcome)
						return
					}
				}
			} catch (e) {
				if (live) setError(err(e))
			}
		})()
		return () => {
			live = false
			ac.abort()
			window.clearInterval(timer)
		}
	}, [nonce, adopt, err])

	if (session === null)
		return (
			<div className="flex flex-col items-center gap-3" aria-live="polite">
				<ErrorBanner message={error} />
				<p className="text-sm text-fg-muted">{t('qr.preparing')}</p>
				<button
					type="button"
					onClick={onUsePassword}
					className="min-h-[44px] text-sm text-accent hover:underline"
				>
					{t('auth.usePassword')}
				</button>
			</div>
		)

	const live = status === 'waiting' && left > 0
	return (
		<div className="flex flex-col items-center gap-3" aria-live="polite">
			{/* Above the panel body, not instead of it: an init failure must still leave
			    "Use password instead" reachable, and the not-live branch "Show a new code". */}
			<ErrorBanner message={error} />
			{live ? (
				<>
					{/* White plate under the code: a dark surface behind the modules inverts the
					    contrast a scanner looks for. */}
					<div className="rounded-lg bg-white p-3">
						<QRCode value={`${location.origin}/qr/${session.sessionId}`} size={168} />
					</div>
					<p className="font-mono text-2xl tracking-[0.3em] text-fg">
						{session.matchCode}
					</p>
					<p className="text-xs text-fg-muted">
						{t('qr.hint')} {t('qr.expiresIn', { n: left })}
					</p>
				</>
			) : (
				<>
					<p className="text-sm text-fg">
						{status === 'denied' ? t('qr.denied') : t('qr.expired')}
					</p>
					<Button onClick={() => setNonce((n) => n + 1)}>{t('qr.new')}</Button>
				</>
			)}
			<button
				type="button"
				onClick={onUsePassword}
				className="min-h-[44px] text-sm text-accent hover:underline"
			>
				{t('auth.usePassword')}
			</button>
		</div>
	)
}

// vim: ts=4
