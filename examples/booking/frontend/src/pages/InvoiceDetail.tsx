import type * as React from 'react'
import { useEffect, useState } from 'react'
import { Link, useNavigate, useParams } from 'react-router-dom'

import type { BuyerView, LineView, PaymentView, VatGroupView } from '@saas-framework/client'
import {
	api,
	errMsg,
	formatMoney,
	formatQty,
	useInvoice,
	useParties,
	vatRate
} from '@saas-framework/client'
import {
	useDiscardDraft,
	useInvoicePayments,
	useNavSubmission,
	usePayByTransfer,
	useProviders,
	useStartPayment
} from '~/api/hooks'
import { ConfirmDialog } from '~/components/ConfirmDialog'
import { useToast } from '~/components/Toast'
import { Badge, Button, ErrorBanner, PageSpinner } from '~/components/ui'
import { LOCALE, date, due } from '~/lib/money'
import { NavBadge, StatusBadge, unissued } from '~/pages/Invoices'

/** The four live states, as `crates/saas-billing/src/provider.rs` ranks them. */
const LIVE = ['PENDING', 'AWAITING_USER', 'RESERVED', 'AUTHORIZED']

/** Money that actually landed, whether or not all of it did. */
const ARRIVED = ['SUCCEEDED', 'PARTIALLY_SUCCEEDED']

/** Seconds left until `iso`, ticked locally. `null` when there is no deadline: the backend's
 *  fresh gateway read is the real gate, so a missing one leaves the button enabled. */
function useSecondsLeft(iso: string | null): number | null {
	const [left, setLeft] = useState<number | null>(null)
	useEffect(() => {
		if (!iso) {
			setLeft(null)
			return
		}
		const deadline = Date.parse(iso)
		const tick = () => setLeft(Math.max(0, Math.ceil((deadline - Date.now()) / 1000)))
		tick()
		const timer = window.setInterval(tick, 1000)
		return () => window.clearInterval(timer)
	}, [iso])
	return left
}

/** `m:ss`, for a countdown measured in minutes. */
function clock(secs: number): string {
	const m = Math.floor(secs / 60)
	return `${m}:${String(secs % 60).padStart(2, '0')}`
}

const METHOD_LABEL: Record<string, string> = {
	TRANSFER: 'Bank transfer',
	CARD: 'Card',
	CASH: 'Cash',
	OTHER: 'Other'
}

export function InvoiceDetail() {
	const { uid = '' } = useParams()
	const navigate = useNavigate()
	const toast = useToast()
	const invoice = useInvoice(uid)
	const nav = useNavSubmission(uid, invoice.data ? !unissued(invoice.data.status) : false)
	// A draft has no buyer snapshot — it is written inside the issue transaction — so the
	// preview shows the party it *would* freeze, which is still editable on /billing.
	const parties = useParties()
	const payments = useInvoicePayments(uid)
	const providers = useProviders()
	const startPayment = useStartPayment(uid)
	const payByTransfer = usePayByTransfer(uid)
	const discard = useDiscardDraft(uid)

	const [discardOpen, setDiscardOpen] = useState(false)
	// Before the early returns: the payment window is what the closed-tab case counts down.
	const secondsLeft = useSecondsLeft(payments.data?.items[0]?.expiresAt ?? null)

	if (invoice.isPending) return <PageSpinner />
	if (invoice.isError || !invoice.data) return <ErrorBanner message={errMsg(invoice.error)} />

	const inv = invoice.data
	const issued = inv.status === 'ISSUED' || inv.status === 'PAID'
	const buyer =
		inv.buyer ?? parties.data?.items.find((p) => p.uid === inv.billingPartyUid) ?? null
	const gateway = providers.data?.items[0]?.id
	const outstanding = due(inv.gross, inv.paidAmount)
	// BigInt over the minor units, as `due` itself does — `Number()` would lose the fillér.
	const owes = BigInt(outstanding.amount.replace('.', '')) > 0n
	// Newest first from the server, so the first row is the attempt this page is about.
	const latest: PaymentView | undefined = payments.data?.items[0]
	const busy = startPayment.isPending || payByTransfer.isPending
	const phase = paymentPhase(inv.status, latest, owes)
	// The gateway still holds it, so "pay another way" is refused until the window closes.
	const transferBlocked = phase === 'live' && secondsLeft !== null && secondsLeft > 0
	const resume = latest?.redirectUrl ?? null

	async function payByCard() {
		if (!gateway) return
		try {
			const started = await startPayment.mutateAsync(gateway)
			if (started.redirectUrl) window.location.assign(started.redirectUrl)
			else toast.info('The gateway opened a payment but sent nowhere to go.')
		} catch (err) {
			toast.error(errMsg(err))
		}
	}

	async function payByBankTransfer() {
		try {
			await payByTransfer.mutateAsync()
			toast.success('Invoiced. Transfer the amount quoting the invoice number.')
		} catch (err) {
			toast.error(errMsg(err))
		}
	}

	async function runDiscard() {
		setDiscardOpen(false)
		try {
			await discard.mutateAsync()
			toast.success('Draft discarded; the bookings are unbilled again.')
			navigate('/')
		} catch (err) {
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
				{issued && (
					<Button variant="secondary" onClick={() => void downloadPdf()}>
						Download PDF
					</Button>
				)}
				{/* Not while a payment is live: the server refuses it with `E-BOOK-PAYMENT-OPEN`,
				    because deleting the draft would strand money the payer has already sent. */}
				{inv.status === 'DRAFT' && phase !== 'live' && (
					<Button
						variant="danger"
						loading={discard.isPending}
						onClick={() => setDiscardOpen(true)}
					>
						Discard draft
					</Button>
				)}
			</div>

			{/* role="status" so the change from "in progress" to paid is announced, not just
			    recoloured — the webhook can land while the page is open. */}
			<section
				role="status"
				className="rounded-xl border border-slate-200 bg-white p-5"
				aria-busy={payments.isPending}
			>
				<h2 className="mb-3 text-sm font-semibold text-slate-900">Payment</h2>
				<Row label="Method" value={METHOD_LABEL[inv.paymentMethod] ?? inv.paymentMethod} />
				<Row label="Paid" value={formatMoney(inv.paidAmount, LOCALE)} />
				<Row label="Amount due" value={formatMoney(outstanding, LOCALE)} />

				{phase === 'paid' && (
					<p className="mt-3 text-sm text-slate-700">
						<Badge tone="success">Paid</Badge> in full on {date(inv.paidAt)}.
					</p>
				)}

				{phase === 'live' && (
					<p className="mt-3 text-sm text-slate-700">
						<Badge tone="info">Payment in progress</Badge> —{' '}
						{secondsLeft === null
							? 'finish it at the gateway, or choose another way to pay.'
							: `the gateway holds this payment for ${clock(secondsLeft)} more. Finish it there, or pay another way once it expires.`}
					</p>
				)}

				{phase === 'partial' && (
					<p className="mt-3 text-sm text-slate-700">
						<Badge tone="warning">Part-paid</Badge> — {formatMoney(outstanding, LOCALE)}{' '}
						still outstanding. Paying again charges only the remainder.
					</p>
				)}

				{phase === 'failed' && (
					<p className="mt-3 text-sm text-slate-700">
						<Badge tone="danger">Payment failed</Badge> — nothing was charged. Try
						again, or pay another way.
					</p>
				)}

				{phase !== 'paid' && phase !== 'settled' && (
					<div className="mt-3 flex flex-wrap items-center gap-2">
						{phase === 'live' &&
							(resume ? (
								<Button onClick={() => window.location.assign(resume)}>
									Continue payment
								</Button>
							) : (
								// Disabled with the reason beside it, not a bare greyed button.
								<>
									<Button disabled>Continue payment</Button>
									<span className="text-sm text-slate-600">
										This attempt has no gateway link to return to — pay another
										way.
									</span>
								</>
							))}
						{phase !== 'live' && gateway && (
							<Button loading={busy} onClick={() => void payByCard()}>
								{phase === 'failed' ? 'Pay again' : 'Pay by card'}
							</Button>
						)}
						{/* While the gateway still reports the payment live, the backend refuses this
						    with `E-BOOK-PAYMENT-LIVE`: the money is not given up on until the window
						    closes. A missing `expiresAt` leaves it enabled — the server is the gate. */}
						{unissued(inv.status) && (
							<>
								<Button
									variant="secondary"
									loading={busy}
									disabled={transferBlocked}
									onClick={() => void payByBankTransfer()}
								>
									{phase === 'none' ? 'Bank transfer' : 'Pay another way'}
								</Button>
								{transferBlocked && (
									<span className="text-sm text-slate-600">
										A card payment is still open at the gateway
										{secondsLeft !== null &&
											`, expiring in ${clock(secondsLeft)}`}
										.
									</span>
								)}
							</>
						)}
					</div>
				)}

				{inv.status === 'ISSUED' && inv.paymentMethod === 'TRANSFER' && owes && (
					<p className="mt-3 text-sm text-slate-600">
						Transfer {formatMoney(outstanding, LOCALE)} by {date(inv.dueDate)}, quoting{' '}
						<strong>{inv.number}</strong> as the payment reference.
					</p>
				)}
			</section>

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
							Issued is stamped when the invoice is raised — at once for a bank
							transfer, when the money lands for a card sale.
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
										{formatQty(l.qty, LOCALE)} {l.unit}
									</td>
									<td className="px-4 py-3 text-right tabular-nums">
										{formatMoney(l.unitPrice, LOCALE)}
									</td>
									<td className="px-4 py-3 text-right tabular-nums">
										{formatMoney(l.net, LOCALE)}
									</td>
									<td className="px-4 py-3 text-right tabular-nums">
										{formatMoney(l.vat, LOCALE)} ({vatRate(l.vatRateBp)})
									</td>
									<td className="px-4 py-3 text-right tabular-nums">
										{formatMoney(l.gross, LOCALE)}
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
							value={`${formatMoney(g.net, LOCALE)} + ${formatMoney(g.vat, LOCALE)} = ${formatMoney(g.gross, LOCALE)}`}
						/>
					))}
					<div className="mt-2 border-t border-slate-200 pt-2">
						<Row label="Total" value={formatMoney(inv.gross, LOCALE)} />
					</div>
					{inv.vatNotes.length > 0 && (
						<p className="mt-2 text-xs text-slate-500">{inv.vatNotes.join(' · ')}</p>
					)}
				</Card>

				<Card title="NAV Online Számla">
					{unissued(inv.status) ? (
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
				open={discardOpen}
				title="Discard this draft"
				description="The draft is deleted and its bookings go back to the unbilled list, ready to be checked out again. If a payment on this draft has already gone through, the discard is refused."
				confirmLabel="Discard draft"
				loading={discard.isPending}
				onClose={() => setDiscardOpen(false)}
				onConfirm={() => void runDiscard()}
			/>
		</div>
	)
}

/**
 * Which of the six payment states this invoice is in. One function, so the copy, the buttons
 * and the "nothing to offer" case cannot disagree about it.
 *
 * `settled` is the odd one: the money is in but the invoice is not `PAID` — an overpayment, or
 * a partial allocation an operator is still working through. There is nothing for the customer
 * to press either way.
 *
 * `partial` exists because `failed` used to be the catch-all: a `PARTIALLY_SUCCEEDED` payment,
 * or a `SUCCEEDED` one only partly allocated, told a charged customer "nothing was charged".
 */
function paymentPhase(
	status: string,
	latest: PaymentView | undefined,
	owes: boolean
): 'paid' | 'settled' | 'live' | 'partial' | 'failed' | 'none' {
	if (status === 'PAID') return 'paid'
	if (latest && LIVE.includes(latest.status)) return 'live'
	// A locked invoice *is* a payment in flight. Without this arm it fell through to `partial`
	// or `failed`, which told a customer mid-checkout that their money had gone missing — the
	// payments read that settles the invoice had not answered yet.
	if (status === 'PENDING') return 'live'
	if (!owes) return 'settled'
	if (latest && ARRIVED.includes(latest.status)) return 'partial'
	return latest ? 'failed' : 'none'
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
