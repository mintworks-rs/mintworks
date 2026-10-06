import { useState } from 'react'

import type { Consent, LegalKind, PasskeyView } from '@mintworks/client'
import {
	PasskeyCancelled,
	ServerError,
	api,
	registerPasskey,
	useAuth,
	useConsents,
	useDeleteAccount,
	usePasskeys,
	useRemovePasskey,
	useRenamePasskey,
	useWithdrawConsent
} from '@mintworks/client'
import { ConfirmDialog, StepUpDialog } from '~/components/ConfirmDialog'
import { DataTable } from '~/components/DataTable'
import { useToast } from '~/components/Toast'
import { Button, ErrorBanner, Field, Input, PageSpinner, StatusChip } from '~/components/ui'
import { useT } from '~/i18n'

/** `consent::UNWITHDRAWABLE` — withdrawing either would mean the account cannot be served. */
const PERMANENT: LegalKind[] = ['TOS', 'PRIVACY']

export function Account() {
	const { t, err, date } = useT()
	const { me, logout } = useAuth()
	const toast = useToast()
	const consents = useConsents()
	const withdraw = useWithdrawConsent()
	const del = useDeleteAccount()

	const [deleteOpen, setDeleteOpen] = useState(false)
	// Which action the step-up prompt is standing in front of; both are step-up gated.
	const [stepUp, setStepUp] = useState<'delete' | 'export' | null>(null)

	if (!me || consents.isPending) return <PageSpinner />
	if (consents.error) return <ErrorBanner message={err(consents.error)} />

	const email = me.account.email

	async function runDelete() {
		try {
			await del.mutateAsync(email)
			setStepUp(null)
			toast.success(t('account.delete.done'))
			await logout()
		} catch (e) {
			if (e instanceof ServerError && e.errCode === 'E-AUTH-STEPUP') {
				setStepUp('delete')
				return
			}
			setStepUp(null)
			toast.error(err(e))
		}
	}

	// Fetched, not navigated to: the route is step-up gated, and a bare <a> renders the error
	// envelope as a page; through the client `E-AUTH-STEPUP` drives the prompt Delete uses.
	async function runExport() {
		try {
			const data = await api.get<unknown>('/api/account/export')
			setStepUp(null)
			const url = URL.createObjectURL(
				new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' })
			)
			const a = document.createElement('a')
			a.href = url
			a.download = 'account-export.json'
			// Appended, and revoked a tick later: Firefox drops the download if the object URL
			// is pulled before it starts reading, and a detached anchor is not reliably
			// clickable — a subject-access request that looks fulfilled and produced no file.
			document.body.append(a)
			a.click()
			a.remove()
			setTimeout(() => URL.revokeObjectURL(url), 0)
			toast.success(t('account.data.exported'))
		} catch (e) {
			if (e instanceof ServerError && e.errCode === 'E-AUTH-STEPUP') {
				setStepUp('export')
				return
			}
			setStepUp(null)
			toast.error(err(e))
		}
	}

	return (
		<div className="space-y-8">
			<section>
				<h1 className="text-lg font-semibold text-fg">{t('account.title')}</h1>
				<dl className="mt-4 rounded-xl border border-line bg-surface-raised p-5 text-sm">
					<Row label={t('common.email')} value={email} />
					<Row label={t('common.name')} value={me.account.name ?? t('common.none')} />
					<Row label={t('account.org')} value={me.org?.name ?? t('common.none')} />
					<Row label={t('account.role')} value={me.org?.role ?? t('common.none')} />
				</dl>
			</section>

			<section>
				<h2 className="text-base font-semibold text-fg">{t('account.consents')}</h2>
				<div className="mt-4">
					<DataTable<Consent>
						rows={consents.data?.items ?? []}
						rowKey={(c) => `${c.kind}-${c.docVersion}`}
						caption={t('account.consents')}
						empty={{ title: t('account.consents.empty') }}
						columns={[
							{
								key: 'kind',
								header: t('account.consents.document'),
								cell: (c) => c.kind
							},
							{
								key: 'version',
								header: t('account.consents.version'),
								cell: (c) => c.docVersion
							},
							{
								key: 'at',
								header: t('account.consents.granted'),
								cell: (c) => date(c.at)
							},
							{
								key: 'state',
								header: t('account.consents.state'),
								cell: (c) =>
									c.granted ? (
										<StatusChip tone="positive">
											{t('account.consents.active')}
										</StatusChip>
									) : (
										<StatusChip>
											{t('account.consents.withdrawn', {
												date: date(c.withdrawnAt)
											})}
										</StatusChip>
									)
							},
							{
								key: 'action',
								header: '',
								cell: (c) =>
									c.granted && !PERMANENT.includes(c.kind) ? (
										<Button
											variant="ghost"
											onClick={() =>
												withdraw
													.mutateAsync(c.kind)
													.then(() =>
														toast.success(t('account.consents.done'))
													)
													.catch((e) => toast.error(err(e)))
											}
										>
											{t('account.consents.withdraw')}
										</Button>
									) : null
							}
						]}
					/>
				</div>
				<p className="mt-2 text-xs text-fg-muted">{t('account.consents.permanent')}</p>
			</section>

			<section>
				<h2 className="text-base font-semibold text-fg">{t('account.security')}</h2>
				<div className="mt-4">
					<Passkeys />
				</div>
				<p className="mt-2 text-xs text-fg-muted">{t('account.security.hint')}</p>
				<div className="mt-6">
					<Totp />
				</div>
			</section>

			<section>
				<h2 className="text-base font-semibold text-fg">{t('account.data')}</h2>
				<div className="mt-4 flex flex-wrap gap-2">
					<Button variant="secondary" onClick={() => void runExport()}>
						{t('account.data.export')}
					</Button>
					<Button variant="danger" onClick={() => setDeleteOpen(true)}>
						{t('account.data.delete')}
					</Button>
				</div>
				<p className="mt-2 text-xs text-fg-muted">{t('account.data.hint')}</p>
			</section>

			<ConfirmDialog
				open={deleteOpen}
				title={t('account.delete.title')}
				description={t('account.delete.body')}
				confirmLabel={t('account.data.delete')}
				confirmPhrase={email}
				loading={del.isPending}
				onClose={() => setDeleteOpen(false)}
				onConfirm={() => {
					setDeleteOpen(false)
					void runDelete()
				}}
			/>

			<StepUpDialog
				open={stepUp !== null}
				onClose={() => setStepUp(null)}
				onAuthenticated={() => {
					// Cleared before the retry, so a second Continue cannot re-run it. Both
					// handlers re-open it themselves if the retry is refused again.
					const pending = stepUp
					setStepUp(null)
					if (pending) void (pending === 'export' ? runExport() : runDelete())
				}}
			/>
		</div>
	)
}

function Row({ label, value }: { label: string; value: string }) {
	return (
		<div className="flex justify-between gap-4 py-1">
			<dt className="text-fg-muted">{label}</dt>
			<dd className="text-fg">{value}</dd>
		</div>
	)
}

/** Passkeys are account-scoped, so this list is the same on every org. */
function Passkeys() {
	const { t, err } = useT()
	const toast = useToast()
	const list = usePasskeys()
	const [busy, setBusy] = useState(false)
	const [stepUp, setStepUp] = useState(false)
	const items = list.data?.items ?? []

	async function add() {
		setBusy(true)
		try {
			await registerPasskey()
			setStepUp(false)
			toast.success(t('account.passkeys.added'))
			await list.refetch()
		} catch (e) {
			// Adding a passkey is step-up gated, so the challenge fetch refuses before any
			// authenticator prompt; a full retry after re-auth is the correct response.
			if (e instanceof ServerError && e.errCode === 'E-AUTH-STEPUP') {
				setStepUp(true)
				return
			}
			setStepUp(false)
			// A dismissed prompt is a cancel, not a failure. Showing it is the commonest
			// passkey UX bug there is.
			if (!(e instanceof PasskeyCancelled)) toast.error(err(e))
		} finally {
			setBusy(false)
		}
	}

	return (
		<div className="rounded-xl border border-line bg-surface-raised p-5">
			{list.isPending ? (
				<PageSpinner />
			) : items.length === 0 ? (
				<p className="text-sm text-fg-muted">{t('account.passkeys.empty')}</p>
			) : (
				<ul className="divide-y divide-line">
					{items.map((p) => (
						<PasskeyRow key={p.credentialId} passkey={p} />
					))}
				</ul>
			)}
			<Button className="mt-3" loading={busy} onClick={() => void add()}>
				{t('account.passkeys.add')}
			</Button>
			<StepUpDialog
				open={stepUp}
				onClose={() => setStepUp(false)}
				onAuthenticated={() => {
					setStepUp(false)
					void add()
				}}
			/>
		</div>
	)
}

function PasskeyRow({ passkey }: { passkey: PasskeyView }) {
	const { t, err, date } = useT()
	const toast = useToast()
	const [name, setName] = useState(passkey.name)
	const [stepUp, setStepUp] = useState(false)
	const [confirmRemove, setConfirmRemove] = useState(false)
	const rename = useRenamePasskey()
	const remove = useRemovePasskey()

	async function removeKey() {
		try {
			await remove.mutateAsync(passkey.credentialId)
			setStepUp(false)
			toast.success(t('account.passkeys.removed'))
		} catch (e) {
			if (e instanceof ServerError && e.errCode === 'E-AUTH-STEPUP') {
				setStepUp(true)
				return
			}
			setStepUp(false)
			toast.error(err(e))
		}
	}

	return (
		<li className="flex flex-wrap items-center gap-2 py-3">
			<Input
				aria-label={t('account.passkeys.name')}
				value={name}
				className="max-w-xs"
				onChange={(e) => setName(e.target.value)}
			/>
			<Button
				variant="secondary"
				disabled={name.trim() === '' || name === passkey.name}
				loading={rename.isPending}
				onClick={() =>
					rename
						.mutateAsync({ credentialId: passkey.credentialId, name })
						.then(() => toast.success(t('keys.renamed')))
						.catch((e) => toast.error(err(e)))
				}
			>
				{t('common.save')}
			</Button>
			<span className="text-xs text-fg-muted">
				{passkey.lastUsedAt === null
					? t('account.passkeys.never')
					: t('account.passkeys.lastUsed', { date: date(passkey.lastUsedAt) })}
			</span>
			<Button
				variant="ghost"
				loading={remove.isPending}
				onClick={() => setConfirmRemove(true)}
			>
				{t('common.remove')}
			</Button>
			<ConfirmDialog
				open={confirmRemove}
				title={t('account.passkeys.removeTitle')}
				description={t('account.passkeys.removeBody', { name: passkey.name })}
				confirmLabel={t('account.passkeys.removeConfirm')}
				loading={remove.isPending}
				onClose={() => setConfirmRemove(false)}
				onConfirm={() => {
					setConfirmRemove(false)
					void removeKey()
				}}
			/>
			<StepUpDialog
				open={stepUp}
				onClose={() => setStepUp(false)}
				onAuthenticated={() => {
					setStepUp(false)
					void removeKey()
				}}
			/>
		</li>
	)
}

interface Enrolment {
	secret: string
	otpauthUri: string
	digits: number
	period: number
}

/**
 * `POST /api/auth/totp` → `DELETE`, with `POST /api/auth/totp/verify` in between. Written
 * against `api.*` because the SDK ships no TOTP hook.
 *
 * The framework serves no *read* of the second factor — neither `/api/auth/me` nor any other
 * route says whether one is enrolled — so both actions are always offered rather than one
 * being shown as the current state.
 */
function Totp() {
	const { t, err } = useT()
	const toast = useToast()
	const [enrolment, setEnrolment] = useState<Enrolment | null>(null)
	const [codes, setCodes] = useState<string[] | null>(null)
	const [code, setCode] = useState('')
	const [busy, setBusy] = useState(false)
	const [stepUp, setStepUp] = useState<'enrol' | 'disable' | null>(null)

	async function run(what: 'enrol' | 'disable') {
		setBusy(true)
		try {
			if (what === 'enrol') setEnrolment(await api.post<Enrolment>('/api/auth/totp'))
			else {
				await api.delete('/api/auth/totp')
				setEnrolment(null)
				setCodes(null)
				toast.success(t('account.totp.disabled'))
			}
			setStepUp(null)
		} catch (e) {
			if (e instanceof ServerError && e.errCode === 'E-AUTH-STEPUP') {
				setStepUp(what)
				return
			}
			setStepUp(null)
			toast.error(err(e))
		} finally {
			setBusy(false)
		}
	}

	async function confirm() {
		setBusy(true)
		try {
			const out = await api.post<{ recoveryCodes: string[] }>('/api/auth/totp/verify', {
				code
			})
			setCodes(out.recoveryCodes)
			setEnrolment(null)
			setCode('')
		} catch (e) {
			toast.error(err(e))
		} finally {
			setBusy(false)
		}
	}

	return (
		<div className="rounded-xl border border-line bg-surface-raised p-5">
			<h3 className="text-sm font-semibold text-fg">{t('account.totp')}</h3>
			<p className="mt-1 text-xs text-fg-muted">{t('account.totp.hint')}</p>

			{enrolment && (
				<div className="mt-4 space-y-3">
					<p className="text-sm text-fg-muted">{t('account.totp.scan')}</p>
					<dl className="text-sm">
						<Row label={t('account.totp.secret')} value={enrolment.secret} />
						<Row label={t('account.totp.uri')} value={enrolment.otpauthUri} />
					</dl>
					<div className="max-w-[12rem]">
						<Field label={t('account.totp.code')} htmlFor="totp-code" required>
							<Input
								inputMode="numeric"
								autoComplete="one-time-code"
								maxLength={enrolment.digits}
								value={code}
								onChange={(e) => setCode(e.target.value)}
							/>
						</Field>
					</div>
					<Button
						loading={busy}
						disabled={code.trim() === ''}
						onClick={() => void confirm()}
					>
						{t('account.totp.confirm')}
					</Button>
				</div>
			)}

			{codes && (
				<div className="mt-4">
					<p className="text-sm text-fg">{t('account.totp.enabled')}</p>
					<h4 className="mt-3 text-xs font-medium text-fg-muted">
						{t('account.totp.recovery')}
					</h4>
					<ul className="mt-1 grid grid-cols-2 gap-1 font-mono text-sm text-fg">
						{codes.map((c) => (
							<li key={c}>{c}</li>
						))}
					</ul>
				</div>
			)}

			<div className="mt-4 flex flex-wrap gap-2">
				<Button loading={busy} onClick={() => void run('enrol')}>
					{t('account.totp.enrol')}
				</Button>
				<Button variant="ghost" loading={busy} onClick={() => void run('disable')}>
					{t('account.totp.disable')}
				</Button>
			</div>

			<StepUpDialog
				open={stepUp !== null}
				onClose={() => setStepUp(null)}
				onAuthenticated={() => {
					const pending = stepUp
					setStepUp(null)
					if (pending) void run(pending)
				}}
			/>
		</div>
	)
}

// vim: ts=4
