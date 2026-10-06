import * as React from 'react'
import QRCode from 'react-qr-code'
import { Link, Navigate, useNavigate } from 'react-router-dom'

import type { LoginBody, QrInit, QrPending } from '@mintworks/client'
import {
	PasskeyCancelled,
	errMsg,
	initQr,
	loginWithPasskey,
	pollQr,
	useAuth,
	useConditionalPasskey
} from '@mintworks/client'
import { AuthCard, Button, ErrorBanner, Field, Input } from '~/components/ui'

/** `qr::TTL_SECONDS`. The countdown is the client's copy of a server-side clock, so a drift of
 *  a second or two only ever means the loop stops one window early; the server answers `404`,
 *  which `pollQr` maps to `expired`, about a second later. */
const QR_TTL_SECONDS = 120

/** No TOTP branch: the framework's second-factor routes stay mounted, the demo
 *  never enrols one, so `POST /api/auth/login/totp` is unreachable from here. */
export function Login() {
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
			setError(errMsg(e))
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
			if (!(e instanceof PasskeyCancelled)) setError(errMsg(e))
		} finally {
			setBusy(false)
		}
	}

	return (
		<AuthCard title="Sign in">
			<ErrorBanner message={error} />
			{qr ? (
				<QrPanel onUsePassword={() => setQr(false)} />
			) : (
				<form onSubmit={submit} className="flex flex-col gap-4">
					<Field label="Email" htmlFor="email" required>
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
					<Field label="Password" htmlFor="password" required>
						<Input
							type="password"
							value={password}
							autoComplete="current-password"
							onChange={(e) => setPassword(e.target.value)}
							required
						/>
					</Field>
					<Button type="submit" loading={busy}>
						Sign in
					</Button>
					{passkey && (
						<Button
							type="button"
							variant="secondary"
							disabled={busy}
							onClick={() => void withPasskey()}
						>
							Sign in with a passkey
						</Button>
					)}
					<Button
						type="button"
						variant="ghost"
						disabled={busy}
						onClick={() => setQr(true)}
					>
						Sign in with a QR code
					</Button>
				</form>
			)}
			<p className="mt-6 text-sm text-slate-500">
				<Link to="/register" className="text-brand-700 hover:underline">
					Create an account
				</Link>
				{' · '}
				<Link to="/password/reset-request" className="text-brand-700 hover:underline">
					Forgot your password?
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
				if (live) setError(errMsg(e))
			}
		})()
		return () => {
			live = false
			ac.abort()
			window.clearInterval(timer)
		}
	}, [nonce, adopt])

	if (session === null)
		return (
			<div className="flex flex-col items-center gap-3" aria-live="polite">
				<ErrorBanner message={error} />
				<p className="text-sm text-slate-500">Preparing a code…</p>
				<button
					type="button"
					onClick={onUsePassword}
					className="min-h-[44px] text-sm text-brand-700 hover:underline"
				>
					Use password instead
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
					<QRCode value={`${location.origin}/qr/${session.sessionId}`} size={168} />
					<p className="font-mono text-2xl tracking-[0.3em] text-slate-900">
						{session.matchCode}
					</p>
					<p className="text-xs text-slate-500">
						Scan with a signed-in device and type the code shown here into it. Expires
						in {left}s.
					</p>
				</>
			) : (
				<>
					<p className="text-sm text-slate-700">
						{status === 'denied' ? 'That sign-in was denied.' : 'That code expired.'}
					</p>
					<Button onClick={() => setNonce((n) => n + 1)}>Show a new code</Button>
				</>
			)}
			<button
				type="button"
				onClick={onUsePassword}
				className="min-h-[44px] text-sm text-brand-700 hover:underline"
			>
				Use password instead
			</button>
		</div>
	)
}

// vim: ts=4
