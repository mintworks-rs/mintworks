// SPDX-License-Identifier: MIT-0
import { Link } from 'react-router-dom'

import type { InvoiceStatus, InvoiceView } from '@mintworks/client'
import { errMsg, formatMoney, useInvoices } from '@mintworks/client'
import { useNavSubmission } from '~/api/hooks'
import { DataTable } from '~/components/DataTable'
import { Badge, Button, ErrorBanner, PageSpinner } from '~/components/ui'
import { LOCALE, date, due } from '~/lib/money'

const STATUS_TONE: Record<InvoiceStatus, 'neutral' | 'success' | 'warning' | 'danger' | 'info'> = {
	DRAFT: 'neutral',
	PENDING: 'info',
	ISSUED: 'info',
	PAID: 'success',
	STORNO: 'warning',
	STORNOED: 'danger'
}

/** The raw enum is a database value, not a word: colour is never the only signal, so the
 *  label carries the meaning too. */
const STATUS_LABEL: Record<InvoiceStatus, string> = {
	DRAFT: 'Draft',
	PENDING: 'Paying',
	ISSUED: 'Issued',
	PAID: 'Paid',
	STORNO: 'Storno',
	STORNOED: 'Cancelled'
}

/** No number, so nothing NAV has ever seen and nothing to render a PDF from. `PENDING` is a
 *  draft with a payment open on it. */
export function unissued(status: InvoiceStatus) {
	return status === 'DRAFT' || status === 'PENDING'
}

export function StatusBadge({ status }: { status: InvoiceStatus }) {
	return <Badge tone={STATUS_TONE[status]}>{STATUS_LABEL[status]}</Badge>
}

/**
 * ponytail: one request per non-draft invoice. The framework's `InvoiceView` carries no NAV
 * field and `mintworks-nav` mounts no routes, so the example serves the filing record on its own
 * `/api/invoices/{uid}/nav`; fold it into the invoice body if a list ever gets long.
 */
export function NavBadge({ uid, status }: { uid: string; status: InvoiceStatus }) {
	const nav = useNavSubmission(uid, !unissued(status))

	if (unissued(status)) return <span className="text-slate-500">—</span>
	if (nav.isPending) return <span className="text-slate-500">…</span>
	// A failed query is not a filing verdict: rendering "Not reported" here told the customer
	// their invoice was never filed with the tax authority because a request dropped.
	if (nav.isError) return <Badge tone="neutral">Status unavailable</Badge>

	// No row at all: either the report job has not run yet, or — the demo's usual case —
	// no NAV credentials are configured, so it never will.
	if (!nav.data) return <Badge tone="neutral">Not reported</Badge>

	switch (nav.data.verdict) {
		case 'DONE':
			return <Badge tone="success">Filed</Badge>
		case 'WARN':
			return <Badge tone="warning">Filed with warnings</Badge>
		case 'REJECTED':
			return <Badge tone="danger">Rejected</Badge>
		case 'FAILED':
			return <Badge tone="danger">Failed</Badge>
		default:
			return <Badge tone="info">In flight</Badge>
	}
}

export function Invoices() {
	const invoices = useInvoices()

	if (invoices.isPending) return <PageSpinner />
	if (invoices.isError) return <ErrorBanner message={errMsg(invoices.error)} />

	const rows = invoices.data.pages.flatMap((p) => p.items)

	return (
		<div className="space-y-4">
			<h1 className="text-lg font-semibold text-slate-900">Invoices</h1>

			<DataTable<InvoiceView>
				rows={rows}
				rowKey={(i) => i.uid}
				caption="Invoices"
				empty={{
					title: 'No invoices yet',
					description: 'Check out your bookings and a draft appears here.'
				}}
				columns={[
					{
						key: 'number',
						header: 'Number',
						cell: (i) => (
							<Link
								to={`/invoices/${i.uid}`}
								className="font-medium text-brand-700 underline"
							>
								{i.number ?? 'Draft'}
							</Link>
						)
					},
					{ key: 'issued', header: 'Issued', cell: (i) => date(i.issuedAt) },
					{
						key: 'status',
						header: 'Status',
						cell: (i) => <StatusBadge status={i.status} />
					},
					{
						key: 'nav',
						header: 'NAV',
						cell: (i) => <NavBadge uid={i.uid} status={i.status} />
					},
					{
						key: 'gross',
						header: 'Total',
						numeric: true,
						cell: (i) => formatMoney(i.gross, LOCALE)
					},
					{
						key: 'due',
						header: 'Amount due',
						numeric: true,
						cell: (i) =>
							// 'STORNO' too: a counter-invoice negates every figure and is paid by
							// nobody, so `due` rendered its full negative gross as a payment link.
							i.status === 'PAID' ||
							i.status === 'STORNOED' ||
							i.status === 'STORNO' ? (
								'—'
							) : (
								<Link
									to={`/invoices/${i.uid}`}
									className="font-medium text-brand-700 underline"
								>
									{formatMoney(due(i.gross, i.paidAmount), LOCALE)}
								</Link>
							)
					}
				]}
			/>

			{invoices.hasNextPage && (
				<Button
					variant="secondary"
					loading={invoices.isFetchingNextPage}
					onClick={() => void invoices.fetchNextPage()}
				>
					Load more
				</Button>
			)}
		</div>
	)
}

// vim: ts=4
