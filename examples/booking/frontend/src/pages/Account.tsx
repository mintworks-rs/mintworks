import { useState } from 'react'
import { Link } from 'react-router-dom'

import type { Consent, LegalKind, PasskeyView } from '@saas-framework/client'
import {
	PasskeyCancelled,
	ServerError,
	api,
	errMsg,
	registerPasskey,
	useAuth,
	useConsents,
	useDeleteAccount,
	usePasskeys,
	useRemovePasskey,
	useRenamePasskey,
	useWithdrawConsent
} from '@saas-framework/client'
import { ConfirmDialog, StepUpDialog } from '~/components/ConfirmDialog'
import { DataTable } from '~/components/DataTable'
import { useToast } from '~/components/Toast'
import { Badge, Button, ErrorBanner, Input, PageSpinner } from '~/components/ui'
import { date } from '~/lib/money'

/** `consent::UNWITHDRAWABLE` — withdrawing either would mean the account cannot be served. */
const PERMANENT: LegalKind[] = ['TOS', 'PRIVACY']

export function Account() {
	const { me, logout } = useAuth()
	const toast = useToast()
	const consents = useConsents()
	const withdraw = useWithdrawConsent()
	const del = useDeleteAccount()

	const [deleteOpen, setDeleteOpen] = useState(false)
	// Which action the step-up prompt is standing in front of; both are step-up gated.
	const [stepUp, setStepUp] = useState<'delete' | 'export' | null>(null)

	if (!me || consents.isPending) return <PageSpinner />
	if (consents.error) return <ErrorBanner message={errMsg(consents.error)} />

	const email = me.account.email

	async function runDelete() {
		try {
			await del.mutateAsync(email)
			setStepUp(null)
			toast.success('Account anonymised. Signing out.')
			await logout()
		} catch (err) {
			if (err instanceof ServerError && err.errCode === 'E-AUTH-STEPUP') {
				setStepUp('delete')
				return
			}
			setStepUp(null)
			toast.error(errMsg(err))
		}
	}

	// Fetched, not navigated to: the route is step-up gated, so a bare <a> took the user out
	// of the SPA and rendered `{"error":{"errCode":"E-AUTH-STEPUP"…}}` as a page with no way
	// back. Through the client the code is catchable and drives the same prompt Delete uses.
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
			toast.success('Export downloaded.')
		} catch (err) {
			if (err instanceof ServerError && err.errCode === 'E-AUTH-STEPUP') {
				setStepUp('export')
				return
			}
			setStepUp(null)
			toast.error(errMsg(err))
		}
	}

	return (
		<div className="space-y-8">
			<section>
				<h1 className="text-lg font-semibold text-slate-900">Account</h1>
				<dl className="mt-4 rounded-xl border border-slate-200 bg-white p-5 text-sm">
					<Row label="Email" value={email} />
					<Row label="Name" value={me.account.name ?? '—'} />
					<Row label="Org" value={me.org?.name ?? '—'} />
					<Row label="Role" value={me.org?.role ?? '—'} />
				</dl>
			</section>

			<section>
				<h2 className="text-base font-semibold text-slate-900">Consents</h2>
				<div className="mt-4">
					<DataTable<Consent>
						rows={consents.data?.items ?? []}
						rowKey={(c) => `${c.kind}-${c.docVersion}`}
						caption="Consents"
						empty={{ title: 'No consent records' }}
						columns={[
							{ key: 'kind', header: 'Document', cell: (c) => c.kind },
							{ key: 'version', header: 'Version', cell: (c) => c.docVersion },
							{ key: 'at', header: 'Granted', cell: (c) => date(c.at) },
							{
								key: 'state',
								header: 'State',
								cell: (c) =>
									c.granted ? (
										<Badge tone="success">Active</Badge>
									) : (
										<Badge tone="neutral">
											Withdrawn {date(c.withdrawnAt)}
										</Badge>
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
													.then(() => toast.success('Consent withdrawn.'))
													.catch((e) => toast.error(errMsg(e)))
											}
										>
											Withdraw
										</Button>
									) : null
							}
						]}
					/>
				</div>
				<p className="mt-2 text-xs text-slate-500">
					The terms and the privacy notice cannot be withdrawn while the account exists —
					deleting the account is how you withdraw those.
				</p>
			</section>

			<section>
				<h2 className="text-base font-semibold text-slate-900">Security</h2>
				<div className="mt-4">
					<Passkeys />
				</div>
				<p className="mt-2 text-xs text-slate-500">
					A passkey signs you in without a password and confirms destructive actions. API
					keys are{' '}
					<Link to="/account/api-keys" className="text-brand-700 hover:underline">
						on their own page
					</Link>
					.
				</p>
			</section>

			<section>
				<h2 className="text-base font-semibold text-slate-900">Your data</h2>
				<div className="mt-4 flex flex-wrap gap-2">
					<Button variant="secondary" onClick={() => void runExport()}>
						Export my data
					</Button>
					<Button variant="danger" onClick={() => setDeleteOpen(true)}>
						Delete my account
					</Button>
				</div>
				<p className="mt-2 text-xs text-slate-500">
					Deleting anonymises the account. Issued invoices are kept: they are legal
					documents with a statutory retention period.
				</p>
			</section>

			<ConfirmDialog
				open={deleteOpen}
				title="Delete your account"
				description="This anonymises your account and signs you out. Issued invoices are retained as the law requires."
				confirmLabel="Delete account"
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
					// Cleared before the retry runs: leaving the modal open let a second
					// Continue click re-run the deletion. Both handlers re-open it themselves
					// if the retry is refused again.
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
			<dt className="text-slate-500">{label}</dt>
			<dd className="text-slate-800">{value}</dd>
		</div>
	)
}

/** Passkeys are account-scoped, so this list is the same on every org. */
function Passkeys() {
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
			toast.success('Passkey added.')
			await list.refetch()
		} catch (e) {
			// Adding a passkey is step-up gated, so the challenge fetch refuses before any
			// authenticator prompt; a full retry after re-auth is the correct response.
			if (e instanceof ServerError && e.errCode === 'E-AUTH-STEPUP') {
				setStepUp(true)
				return
			}
			setStepUp(false)
			// A dismissed prompt is a cancel, not a failure. Showing it is the commonest passkey
			// UX bug there is.
			if (!(e instanceof PasskeyCancelled)) toast.error(errMsg(e))
		} finally {
			setBusy(false)
		}
	}

	return (
		<div className="rounded-xl border border-slate-200 bg-white p-5">
			{list.isPending ? (
				<PageSpinner />
			) : items.length === 0 ? (
				<p className="text-sm text-slate-500">
					No passkeys yet. Adding one lets you sign in without a password.
				</p>
			) : (
				<ul className="divide-y divide-slate-100">
					{items.map((p) => (
						<PasskeyRow key={p.credentialId} passkey={p} />
					))}
				</ul>
			)}
			<Button className="mt-3" loading={busy} onClick={() => void add()}>
				Add a passkey
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
			toast.success('Passkey removed.')
		} catch (e) {
			if (e instanceof ServerError && e.errCode === 'E-AUTH-STEPUP') {
				setStepUp(true)
				return
			}
			setStepUp(false)
			toast.error(errMsg(e))
		}
	}

	return (
		<li className="flex flex-wrap items-center gap-2 py-3">
			<Input
				aria-label="Passkey name"
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
						.then(() => toast.success('Renamed.'))
						.catch((e) => toast.error(errMsg(e)))
				}
			>
				Save
			</Button>
			<span className="text-xs text-slate-500">
				{passkey.lastUsedAt === null
					? 'Never used'
					: `Last used ${date(passkey.lastUsedAt)}`}
			</span>
			<Button
				variant="ghost"
				loading={remove.isPending}
				onClick={() => setConfirmRemove(true)}
			>
				Remove
			</Button>
			<ConfirmDialog
				open={confirmRemove}
				title="Remove this passkey"
				description={`“${passkey.name}” will no longer sign you in or confirm actions.`}
				confirmLabel="Remove passkey"
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

// vim: ts=4
