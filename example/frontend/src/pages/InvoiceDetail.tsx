import type * as React from 'react'
import { useState } from 'react'
import { Link, useParams } from 'react-router-dom'

import { api, errMsg, ServerError } from '~/api/client'
import type { InvoiceAction } from '~/api/hooks'
import { useInvoice, useInvoiceAction, useNavSubmission, useParties } from '~/api/hooks'
import type { BuyerView, LineView, MoneyWire, VatGroupView } from '~/api/types'
import { ConfirmDialog, StepUpDialog } from '~/components/ConfirmDialog'
import { useToast } from '~/components/Toast'
import { Badge, Button, ErrorBanner, PageSpinner } from '~/components/ui'
import { date, money, qtyDec, vatRate } from '~/lib/money'
import { NavBadge, StatusBadge } from '~/pages/Invoices'

interface Pending {
	action: InvoiceAction
	reason?: string
	/** The gross the screen showed, replayed unchanged after a step-up: the server refuses it
	 *  if the invoice is no longer that amount. */
	amount?: MoneyWire
}

export function InvoiceDetail() {
	const { uid = '' } = useParams()
	const toast = useToast()
	const invoice = useInvoice(uid)
	const act = useInvoiceAction(uid)
	const nav = useNavSubmission(uid, invoice.data ? invoice.data.status !== 'DRAFT' : false)
	// A draft has no buyer snapshot — it is written inside the issue transaction — so the
	// preview shows the party it *would* freeze, which is still editable on /billing.
	const parties = useParties()

	const [stornoOpen, setStornoOpen] = useState(false)
	const [paymentOpen, setPaymentOpen] = useState(false)
	const [stepUp, setStepUp] = useState<Pending | null>(null)

	if (invoice.isPending) return <PageSpinner />
	if (invoice.isError || !invoice.data) return <ErrorBanner message={errMsg(invoice.error)} />

	const inv = invoice.data
	const issued = inv.status === 'ISSUED' || inv.status === 'PAID'
	const buyer =
		inv.buyer ?? parties.data?.items.find((p) => p.uid === inv.billingPartyUid) ?? null

	async function run(pending: Pending) {
		try {
			await act.mutateAsync(pending)
			setStepUp(null)
			toast.success('Done.')
		} catch (err) {
			// Step-up is time-boxed (`auth.stepup_window`, 300 s), so a session that has been
			// open a while hits this on the first destructive action and retries after re-auth.
			if (err instanceof ServerError && err.errCode === 'E-AUTH-STEPUP') {
				setStepUp(pending)
				return
			}
			setStepUp(null)
			toast.error(errMsg(err))
		}
	}

	// Fetched, not navigated to: `/pdf` is behind `require_auth`, so a bare <a> bypassed the
	// client's refresh-and-retry and rendered the error envelope as a page with no way back.
	// The server's `Content-Disposition` name does not survive a blob, hence `a.download`.
	async function downloadPdf() {
		try {
			const url = URL.createObjectURL(await api.blob(`/api/invoices/${inv.uid}/pdf`))
			const a = document.createElement('a')
			a.href = url
			a.download = `${inv.number ?? inv.uid}.pdf`
			// Appended, and revoked a tick later: Firefox drops the download if the object URL
			// is pulled before it starts reading, and a detached anchor is not reliably clickable.
			document.body.append(a)
			a.click()
			a.remove()
			setTimeout(() => URL.revokeObjectURL(url), 0)
		} catch (err) {
			toast.error(errMsg(err))
		}
	}

	return (
		<div className="space-y-6">
			<div className="flex flex-wrap items-center gap-3">
				<Link to="/invoices" className="text-sm text-brand-700 underline">
					Invoices
				</Link>
				<h1 className="text-lg font-semibold text-slate-900">{inv.number ?? 'Draft'}</h1>
				<StatusBadge status={inv.status} />
				<NavBadge uid={inv.uid} status={inv.status} />
			</div>

			<div className="flex flex-wrap gap-2">
				{inv.status === 'DRAFT' && (
					<Button loading={act.isPending} onClick={() => void run({ action: 'confirm' })}>
						Confirm and issue
					</Button>
				)}
				{inv.status === 'ISSUED' && (
					<Button
						variant="secondary"
						loading={act.isPending}
						onClick={() => setPaymentOpen(true)}
					>
						Record payment
					</Button>
				)}
				{issued && (
					<Button
						variant="danger"
						loading={act.isPending}
						onClick={() => setStornoOpen(true)}
					>
						Storno
					</Button>
				)}
				{issued && (
					<Button variant="secondary" onClick={() => void downloadPdf()}>
						Download PDF
					</Button>
				)}
			</div>

			{inv.status === 'ISSUED' && (
				<p className="text-sm text-slate-500">
					Recording a payment is simulated: it writes the paid state directly. A real
					gateway arrives with <code>saas-billing</code> and its payment adapter. It is
					one-way — a paid invoice can no longer be stornoed.
				</p>
			)}

			<section className="grid gap-4 sm:grid-cols-2">
				<Card title="Buyer">
					<Buyer buyer={buyer} pending={!inv.buyer} />
				</Card>
				<Card title="Dates">
					<Row label="Issued" value={date(inv.issuedAt)} />
					<Row label="Fulfilment" value={date(inv.fulfilmentDate)} />
					<Row label="Due" value={date(inv.dueDate)} />
					<Row label="Paid" value={date(inv.paidAt)} />
					{!issued && (
						<p className="mt-2 text-xs text-slate-500">
							Issued is stamped when you confirm; fulfilment defaults to that day and
							due to it plus the payment term.
						</p>
					)}
				</Card>
			</section>

			<section>
				<h2 className="mb-2 text-base font-semibold text-slate-900">Lines</h2>
				<div className="overflow-x-auto rounded-xl border border-slate-200 bg-white">
					<table className="w-full text-sm">
						<thead className="border-b border-slate-200 bg-slate-50 text-left text-slate-600">
							<tr>
								<th scope="col" className="px-4 py-3 font-medium">
									Description
								</th>
								<th scope="col" className="px-4 py-3 text-right font-medium">
									Qty
								</th>
								<th scope="col" className="px-4 py-3 text-right font-medium">
									Unit price
								</th>
								<th scope="col" className="px-4 py-3 text-right font-medium">
									Net
								</th>
								<th scope="col" className="px-4 py-3 text-right font-medium">
									VAT
								</th>
								<th scope="col" className="px-4 py-3 text-right font-medium">
									Gross
								</th>
							</tr>
						</thead>
						<tbody className="divide-y divide-slate-100">
							{(inv.lines ?? []).map((l: LineView) => (
								<tr key={l.lineNo}>
									<td className="px-4 py-3 text-slate-800">
										{l.description}
										{l.note && (
											<div className="text-xs text-slate-500">{l.note}</div>
										)}
									</td>
									<td className="px-4 py-3 text-right tabular-nums">
										{qtyDec(l.qty)} {l.unit}
									</td>
									<td className="px-4 py-3 text-right tabular-nums">
										{money(l.unitPrice)}
									</td>
									<td className="px-4 py-3 text-right tabular-nums">
										{money(l.net)}
									</td>
									<td className="px-4 py-3 text-right tabular-nums">
										{money(l.vat)} ({vatRate(l.vatRateBp)})
									</td>
									<td className="px-4 py-3 text-right tabular-nums">
										{money(l.gross)}
									</td>
								</tr>
							))}
						</tbody>
					</table>
				</div>
			</section>

			<section className="grid gap-4 sm:grid-cols-2">
				<Card title="VAT summary">
					{(inv.vatSummary ?? []).map((g: VatGroupView) => (
						<Row
							key={g.vatCode}
							label={`${g.vatCode} (${vatRate(g.vatRateBp)})`}
							value={`${money(g.net)} + ${money(g.vat)} = ${money(g.gross)}`}
						/>
					))}
					<div className="mt-2 border-t border-slate-200 pt-2">
						<Row label="Total" value={money(inv.gross)} />
					</div>
					{inv.vatNotes.length > 0 && (
						<p className="mt-2 text-xs text-slate-500">{inv.vatNotes.join(' · ')}</p>
					)}
				</Card>

				<Card title="NAV Online Számla">
					{inv.status === 'DRAFT' ? (
						<p className="text-sm text-slate-600">
							A draft is not a legal document and is never reported.
						</p>
					) : nav.data ? (
						<>
							<Row label="Operation" value={nav.data.op} />
							<Row label="Verdict" value={nav.data.verdict ?? 'awaiting NAV'} />
							<Row label="Answered" value={date(nav.data.submittedAt)} />
							{nav.data.message && (
								<p className="mt-2 text-xs text-slate-500">{nav.data.message}</p>
							)}
						</>
					) : nav.isError ? (
						/* A dropped request is not a filing verdict, same as `NavBadge`. */
						<p className="text-sm text-slate-600">
							<Badge tone="neutral">Status unavailable</Badge> — the filing state
							could not be read.
						</p>
					) : (
						/* A `nav_submissions` row is opened only after a successful tokenExchange,
						   so "no row" covers a queued job, a failed one and no credentials alike —
						   the page cannot tell them apart and must not guess. */
						<p className="text-sm text-slate-600">
							<Badge tone="neutral">Not reported</Badge> — the invoice is legally
							issued either way. The report job has not filed it yet; the backend log
							says whether it is queued, failing or unconfigured.
						</p>
					)}
				</Card>
			</section>

			<ConfirmDialog
				open={paymentOpen}
				title="Record payment"
				description="This marks the invoice PAID, and PAID is final: the invoice can no longer be stornoed. There is no gateway here — it writes the paid state directly."
				confirmLabel="Mark as paid"
				loading={act.isPending}
				onClose={() => setPaymentOpen(false)}
				onConfirm={() => {
					setPaymentOpen(false)
					void run({ action: 'payment', amount: inv.gross })
				}}
			/>

			<ConfirmDialog
				open={stornoOpen}
				title="Storno this invoice"
				description="A storno is a new, numbered document that cancels this one. It cannot be undone, and the bookings stay attached to the cancelled invoice."
				confirmLabel="Issue storno"
				reasonLabel="Reason (appears on the storno)"
				reasonRequired
				loading={act.isPending}
				onClose={() => setStornoOpen(false)}
				onConfirm={(reason) => {
					setStornoOpen(false)
					void run({ action: 'cancel', reason })
				}}
			/>

			<StepUpDialog
				open={stepUp !== null}
				onClose={() => setStepUp(null)}
				onAuthenticated={() => {
					// Cleared before the retry runs: leaving the modal open let a second
					// Continue click re-run the same confirm, storno or payment mutation.
					// `run` re-opens it itself if the retry is refused again.
					const pending = stepUp
					setStepUp(null)
					if (pending) void run(pending)
				}}
			/>
		</div>
	)
}

function Card({ title, children }: { title: string; children: React.ReactNode }) {
	return (
		<div className="rounded-xl border border-slate-200 bg-white p-5">
			<h2 className="mb-3 text-sm font-semibold text-slate-900">{title}</h2>
			{children}
		</div>
	)
}

function Row({ label, value }: { label: string; value: string }) {
	return (
		<div className="flex justify-between gap-4 py-1 text-sm">
			<span className="text-slate-500">{label}</span>
			<span className="text-slate-800">{value}</span>
		</div>
	)
}

// `pending` is a draft's preview of the billing party, not the frozen snapshot: editing it on
// /billing still changes what gets issued, which the snapshot can never do.
function Buyer({ buyer, pending }: { buyer: BuyerView | null; pending: boolean }) {
	if (!buyer) {
		return (
			<p className="text-sm text-slate-600">
				No billing details yet —{' '}
				<Link to="/billing" className="text-brand-700 underline">
					add them
				</Link>{' '}
				before issuing.
			</p>
		)
	}
	return (
		<div className="text-sm text-slate-800">
			<div>{buyer.name}</div>
			<div className="text-slate-600">
				{[buyer.postcode, buyer.city, buyer.street].filter(Boolean).join(' ')}
			</div>
			<div className="text-slate-600">{buyer.country}</div>
			{buyer.taxNumber && <div className="text-slate-600">{buyer.taxNumber}</div>}
			{pending && (
				<p className="mt-2 text-xs text-slate-500">
					Copied onto the invoice when you confirm it. Edit on{' '}
					<Link to="/billing" className="text-brand-700 underline">
						Billing details
					</Link>
					.
				</p>
			)}
		</div>
	)
}

// vim: ts=4
