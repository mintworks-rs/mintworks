// SPDX-License-Identifier: MIT-0
import { useEffect, useState } from 'react'
import { useNavigate, useSearchParams } from 'react-router-dom'

import type { InvoiceStatus, InvoiceView } from '@mintworks/client'
import { useInvoices } from '@mintworks/client'
import type { Column } from '~/components/DataTable'
import { DataTable } from '~/components/DataTable'
import type { Tone } from '~/components/ui'
import { Button, ErrorBanner, Input, MoneyText, SkeletonTable, StatusChip } from '~/components/ui'
import { useT } from '~/i18n'

/** A chip is never the only signal — the label is always the status word. Shared with the
 *  detail and composer screens. */
const TONES: Record<InvoiceStatus, Tone> = {
	DRAFT: 'neutral',
	PENDING: 'warning',
	ISSUED: 'accent',
	PAID: 'positive',
	STORNO: 'neutral',
	STORNOED: 'danger'
}

export function InvoiceStatusChip({ status }: { status: InvoiceStatus }) {
	const { t } = useT()
	return <StatusChip tone={TONES[status]}>{t(`invoices.status.${status}`)}</StatusChip>
}

/** The filter chips, in the order they are shown. `''` is no `status` key at all; the storno
 *  chip asks for both halves of the pair, the credit note and what it cancelled. */
const FILTERS = [
	{ key: 'all', status: '' },
	{ key: 'draft', status: 'DRAFT' },
	{ key: 'issued', status: 'ISSUED' },
	{ key: 'paid', status: 'PAID' },
	{ key: 'storno', status: 'STORNO,STORNOED' }
] as const

export function Invoices() {
	const { t, err, date } = useT()
	const navigate = useNavigate()
	const [params, setParams] = useSearchParams()
	const status = params.get('status') ?? ''
	const q = params.get('q') ?? ''
	const [text, setText] = useState(q)

	// The URL is the state, not the box: a filtered view is linkable and survives a reload.
	// The box runs ahead of it only for the length of the debounce.
	useEffect(() => {
		if (text.trim() === q) return
		const id = setTimeout(() => {
			const next: Record<string, string> = {}
			if (status) next.status = status
			if (text.trim()) next.q = text.trim()
			setParams(next, { replace: true })
		}, 300)
		return () => clearTimeout(id)
	}, [text, q, status, setParams])

	const list = useInvoices({ status, q, limit: 50 })
	const rows = list.data?.pages.flatMap((p) => p.items) ?? []
	const filtered = status !== '' || q !== ''

	function pick(next: string) {
		const p: Record<string, string> = {}
		if (next) p.status = next
		if (q) p.q = q
		setParams(p, { replace: true })
	}

	// No sorting: the server orders newest-first and nothing sorts server-side, so a client
	// sort over one loaded page would claim an order the rest of the list does not have.
	const columns: Column<InvoiceView>[] = [
		{
			key: 'number',
			header: t('invoices.number'),
			cell: (i) => (
				<span className="font-medium">{i.number ?? t('invoices.status.DRAFT')}</span>
			)
		},
		{
			key: 'date',
			header: t('invoices.date'),
			cell: (i) => date(i.fulfilmentDate ?? i.issuedAt ?? i.createdAt)
		},
		{
			key: 'buyer',
			header: t('invoices.buyer'),
			cell: (i) => i.buyer?.name ?? t('common.none')
		},
		{
			key: 'status',
			header: t('invoices.statusCol'),
			cell: (i) => <InvoiceStatusChip status={i.status} />
		},
		{
			key: 'net',
			header: t('invoices.net'),
			cell: (i) => <MoneyText value={i.net} />,
			numeric: true
		},
		{
			key: 'vat',
			header: t('invoices.vat'),
			cell: (i) => <MoneyText value={i.vat} />,
			numeric: true
		},
		{
			key: 'gross',
			header: t('invoices.gross'),
			cell: (i) => <MoneyText value={i.gross} className="font-medium" />,
			numeric: true
		}
	]

	return (
		<div className="space-y-6">
			<div className="flex flex-wrap items-start justify-between gap-4">
				<div>
					<h1 className="text-lg font-semibold text-fg">{t('invoices.title')}</h1>
					<p className="mt-1 max-w-2xl text-sm text-fg-muted">{t('invoices.intro')}</p>
				</div>
				<Button onClick={() => navigate('/invoices/new')}>{t('invoices.new')}</Button>
			</div>

			<div className="flex flex-wrap items-center gap-3">
				<fieldset className="flex flex-wrap gap-1" aria-label={t('invoices.statusCol')}>
					{FILTERS.map((f) => (
						<Button
							key={f.key}
							variant="secondary"
							className={f.status === status ? 'border-accent text-accent' : ''}
							aria-pressed={f.status === status}
							onClick={() => pick(f.status)}
						>
							{t(`invoices.filter.${f.key}`)}
						</Button>
					))}
				</fieldset>
				<Input
					type="search"
					className="sm:w-64"
					aria-label={t('invoices.search')}
					placeholder={t('invoices.searchHint')}
					value={text}
					onChange={(e) => setText(e.target.value)}
				/>
			</div>

			{list.isPending ? (
				<SkeletonTable />
			) : list.error ? (
				<ErrorBanner message={err(list.error)} />
			) : (
				<DataTable
					columns={columns}
					rows={rows}
					rowKey={(i) => i.uid}
					caption={t('invoices.title')}
					empty={
						filtered
							? {
									title: t('invoices.noMatch'),
									description: t('invoices.noMatch.body'),
									action: (
										<Button
											onClick={() => {
												setText('')
												setParams({}, { replace: true })
											}}
										>
											{t('invoices.clearFilter')}
										</Button>
									)
								}
							: {
									title: t('invoices.empty'),
									description: t('invoices.empty.body'),
									action: (
										<Button onClick={() => navigate('/invoices/new')}>
											{t('invoices.new')}
										</Button>
									)
								}
					}
					onRowClick={(i) =>
						navigate(
							i.status === 'DRAFT' ? `/invoices/${i.uid}/edit` : `/invoices/${i.uid}`
						)
					}
				/>
			)}

			{/* A button, never infinite scroll: the footer stays reachable and nothing loads
			    itself while the reader is looking at row three. */}
			{list.hasNextPage && (
				<div className="flex justify-center">
					<Button
						variant="secondary"
						loading={list.isFetchingNextPage}
						onClick={() => void list.fetchNextPage()}
					>
						{t('invoices.loadMore')}
					</Button>
				</div>
			)}
		</div>
	)
}

// vim: ts=4
