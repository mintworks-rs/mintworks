import { useEffect, useState } from 'react'
import { Link, useNavigate, useParams } from 'react-router-dom'

import { ServerError, api, localDate, urls, useInvoice } from '@mintworks/client'
import {
	useInvoiceProject,
	useMarkPaid,
	useProjects,
	useSetInvoiceProject,
	useStornoInvoice
} from '~/api/hooks'
import { ConfirmDialog, StepUpDialog } from '~/components/ConfirmDialog'
import { Modal } from '~/components/Modal'
import { useToast } from '~/components/Toast'
import { Button, ErrorBanner, Field, Input, PageSpinner, Select } from '~/components/ui'
import { type Key, useT } from '~/i18n'
import { Review } from '~/pages/InvoiceComposer'
import { InvoiceStatusChip } from '~/pages/Invoices'

/** The PDF is rendered by a background job, so `document` is null for a moment after issuing.
 *  Poll for it, but not forever: a job that has not finished in half a minute has failed, and
 *  a page that polls all day is a page that never idles. */
const POLL_MS = 2000
const POLL_FOR_MS = 30_000

export function InvoiceDetail() {
	const { uid = '' } = useParams()
	const { t, err, date, dateTime } = useT()
	const navigate = useNavigate()
	const toast = useToast()
	const storno = useStornoInvoice()
	const [stornoOpen, setStornoOpen] = useState(false)
	const [stornoReason, setStornoReason] = useState('')
	const [stepUp, setStepUp] = useState(false)
	const [error, setError] = useState<string | null>(null)
	const [poll, setPoll] = useState<number | false>(false)

	const q = useInvoice(uid, poll)
	const inv = q.data
	const pending = inv !== undefined && inv.number !== null && inv.document === undefined

	useEffect(() => {
		setPoll(pending ? POLL_MS : false)
		if (!pending) return
		const id = setTimeout(() => setPoll(false), POLL_FOR_MS)
		return () => clearTimeout(id)
	}, [pending])

	if (q.isPending) return <PageSpinner />
	if (q.error) return <ErrorBanner message={err(q.error)} />
	if (!inv) return null

	const issued = inv.number !== null

	async function doStorno(reason: string) {
		if (!inv) return
		try {
			const made = await storno.mutateAsync({ uid: inv.uid, reason })
			toast.success(t('detail.stornoed'))
			navigate(`/invoices/${made.uid}`)
		} catch (e) {
			if (e instanceof ServerError && e.errCode === 'E-AUTH-STEPUP') {
				setStornoReason(reason)
				setStepUp(true)
				return
			}
			setError(err(e))
		}
	}

	// Fetched, not navigated to: `/pdf` is behind `require_auth`, so a bare <a> bypasses the
	// client's refresh-and-retry and renders the error envelope as a page with no way back.
	async function downloadPdf() {
		if (!inv) return
		try {
			const url = URL.createObjectURL(await api.blob(urls.invoicePdf(inv.uid)))
			const a = document.createElement('a')
			a.href = url
			a.download = `${inv.number ?? inv.uid}.pdf`
			// Appended, and revoked a tick later: Firefox drops the download if the object URL
			// is pulled before it starts reading, and a detached anchor is not reliably clickable.
			document.body.append(a)
			a.click()
			a.remove()
			setTimeout(() => URL.revokeObjectURL(url), 0)
		} catch (e) {
			toast.error(err(e))
		}
	}

	return (
		<div className="space-y-6">
			<div className="flex flex-wrap items-center gap-3">
				<Link to="/invoices" className="text-sm text-accent underline">
					{t('invoices.title')}
				</Link>
				<h1 className="text-lg font-semibold text-fg">
					{inv.number ?? t('invoices.status.DRAFT')}
				</h1>
				<InvoiceStatusChip status={inv.status} />
			</div>

			<ErrorBanner message={error} />

			<div className="flex flex-wrap gap-2">
				{inv.status === 'DRAFT' && (
					<Button
						variant="secondary"
						onClick={() => navigate(`/invoices/${inv.uid}/edit`)}
					>
						{t('common.edit')}
					</Button>
				)}
				{issued && (
					<Button
						variant="secondary"
						// Said, not just greyed out: a disabled control with no explanation is
						// the anti-pattern.
						disabled={inv.document === undefined}
						onClick={() => void downloadPdf()}
					>
						{inv.document === undefined ? t('detail.pdfPending') : t('detail.pdf')}
					</Button>
				)}
				{inv.status === 'ISSUED' && inv.paymentMethod === 'TRANSFER' && (
					<MarkPaid uid={inv.uid} />
				)}
				{inv.status === 'ISSUED' && (
					<Button variant="danger" onClick={() => setStornoOpen(true)}>
						{t('detail.storno')}
					</Button>
				)}
			</div>

			<dl className="grid gap-x-6 gap-y-2 rounded-xl border border-line bg-surface-raised p-4 text-sm sm:grid-cols-3">
				<div>
					<dt className="text-fg-muted">{t('detail.issuedAt')}</dt>
					<dd className="text-fg">{dateTime(inv.issuedAt)}</dd>
				</div>
				<div>
					<dt className="text-fg-muted">{t('invoices.paymentMethod')}</dt>
					<dd className="text-fg">{t(`invoices.pay.${inv.paymentMethod}` as Key)}</dd>
				</div>
				<div>
					<dt className="text-fg-muted">{t('detail.paid')}</dt>
					<dd className="text-fg">{date(inv.paidAt)}</dd>
				</div>
				{inv.stornoInvoiceUid !== null && (
					<div>
						<dt className="text-fg-muted">{t('detail.stornoedBy')}</dt>
						<dd>
							<Link
								to={`/invoices/${inv.stornoInvoiceUid}`}
								className="text-accent underline"
							>
								{t('detail.open')}
							</Link>
						</dd>
					</div>
				)}
				{inv.originalInvoiceUid !== null && (
					<div>
						<dt className="text-fg-muted">{t('detail.stornoOf')}</dt>
						<dd>
							<Link
								to={`/invoices/${inv.originalInvoiceUid}`}
								className="text-accent underline"
							>
								{t('detail.open')}
							</Link>
						</dd>
					</div>
				)}
			</dl>

			<ProjectPicker invoiceUid={inv.uid} />

			<Review invoice={inv} />

			{inv.vatNotes.length > 0 && (
				<ul className="space-y-1 text-sm text-fg-muted">
					{inv.vatNotes.map((n) => (
						<li key={n}>{n}</li>
					))}
				</ul>
			)}

			<ConfirmDialog
				open={stornoOpen}
				title={t('detail.storno.title')}
				description={t('detail.storno.body')}
				confirmLabel={t('detail.storno')}
				reasonLabel={t('detail.storno.reason')}
				reasonRequired
				loading={storno.isPending}
				onClose={() => setStornoOpen(false)}
				onConfirm={(reason) => {
					setStornoOpen(false)
					void doStorno(reason)
				}}
			/>
			<StepUpDialog
				open={stepUp}
				onClose={() => setStepUp(false)}
				onAuthenticated={() => {
					setStepUp(false)
					void doStorno(stornoReason)
				}}
			/>
		</div>
	)
}

/** A transfer received in full. The date is what the KATA/átalány cash basis counts by. */
function MarkPaid({ uid }: { uid: string }) {
	const { t, err } = useT()
	const toast = useToast()
	const paid = useMarkPaid()
	const [open, setOpen] = useState(false)
	const [on, setOn] = useState(() => localDate())
	const [error, setError] = useState<string | null>(null)

	async function submit() {
		setError(null)
		try {
			await paid.mutateAsync({ uid, paidOn: on })
			setOpen(false)
			toast.success(t('detail.markPaid.done'))
		} catch (e) {
			setError(err(e))
		}
	}

	return (
		<>
			<Button variant="secondary" onClick={() => setOpen(true)}>
				{t('detail.markPaid')}
			</Button>
			<Modal open={open} onClose={() => setOpen(false)} title={t('detail.markPaid.title')}>
				<form
					noValidate
					className="space-y-4"
					onSubmit={(e) => {
						e.preventDefault()
						void submit()
					}}
				>
					<ErrorBanner message={error} />
					<Field label={t('detail.markPaid.on')} htmlFor="paid-on" required>
						<Input
							type="date"
							max={localDate()}
							value={on}
							onChange={(e) => setOn(e.target.value)}
						/>
					</Field>
					<div className="flex justify-end gap-2">
						<Button type="button" variant="secondary" onClick={() => setOpen(false)}>
							{t('common.cancel')}
						</Button>
						<Button type="submit" loading={paid.isPending} disabled={on === ''}>
							{t('detail.markPaid')}
						</Button>
					</div>
				</form>
			</Modal>
		</>
	)
}

/** Deliberately not gated on `issued`: the link is an `invoice.ext` row keyed by the invoice,
 *  not a column on it, so `ISSUED` immutability never reaches it. The hint says so — a control
 *  that stays live on a frozen invoice otherwise reads as a bug.
 *
 *  `PUT /api/app/invoices/{uid}/project` requires a `projectUid` and there is no DELETE
 *  (`projects.rn::inv_set_project`), so an assignment can be moved but not cleared. */
function ProjectPicker({ invoiceUid }: { invoiceUid: string }) {
	const { t, err } = useT()
	const toast = useToast()
	const projects = useProjects()
	const current = useInvoiceProject(invoiceUid)
	const set = useSetInvoiceProject(invoiceUid)

	if (current.isPending || projects.isPending) return null

	return (
		<div className="rounded-xl border border-line bg-surface-raised p-4 sm:max-w-md">
			<Field
				label={t('projects.assign')}
				htmlFor="invoice-project"
				hint={t('projects.assign.hint')}
			>
				<Select
					value={current.data?.projectUid ?? ''}
					disabled={set.isPending}
					onChange={(e) =>
						void set
							.mutateAsync(e.target.value)
							.then(() => toast.success(t('projects.assigned')))
							.catch((e2) => toast.error(err(e2)))
					}
				>
					<option value="" disabled>
						{t('common.none')}
					</option>
					{(projects.data?.items ?? []).map((p) => (
						<option key={p.uid} value={p.uid}>
							{p.body.name}
						</option>
					))}
				</Select>
			</Field>
		</div>
	)
}

// vim: ts=4
