import type * as React from 'react'
import { useState } from 'react'
import { Link, useNavigate } from 'react-router-dom'

import { errMsg } from '~/api/client'
import { useBook, useBookings, useCheckout, useParties, useServices } from '~/api/hooks'
import type { Booking, ServiceView } from '~/api/types'
import { DataTable } from '~/components/DataTable'
import { useToast } from '~/components/Toast'
import { Button, ErrorBanner, Field, Input, PageSpinner } from '~/components/ui'
import { localDate, money, qty, toQtyE6 } from '~/lib/money'

const today = () => localDate()

export function Book() {
	const navigate = useNavigate()
	const toast = useToast()
	const services = useServices()
	const bookings = useBookings()
	const parties = useParties()
	const book = useBook()
	const checkout = useCheckout()

	const [serviceCode, setServiceCode] = useState('CONSULT')
	const [occurredOn, setOccurredOn] = useState(today)
	const [amount, setAmount] = useState('1')
	const [note, setNote] = useState('')
	const [error, setError] = useState<string | null>(null)

	if (services.isPending || bookings.isPending || parties.isPending) return <PageSpinner />
	const loadError = services.error ?? bookings.error ?? parties.error
	if (loadError) return <ErrorBanner message={errMsg(loadError)} />

	const catalogue = services.data?.items ?? []
	const rows = bookings.data?.items ?? []
	// Display only: `claim_unbilled` claims *every* unbilled row server-side, so a truncated
	// page still checks out the whole set.
	const unbilled = rows.filter((b) => b.invoiceUid === null)
	const billed = rows.filter((b) => b.invoiceUid !== null)
	const selected = catalogue.find((s) => s.code === serviceCode)
	const hasParty = (parties.data?.items.length ?? 0) > 0

	async function submit(e: React.FormEvent) {
		e.preventDefault()
		setError(null)
		const qtyE6 = toQtyE6(amount)
		if (qtyE6 === null) {
			setError('Quantity must be a positive number, at most six decimals.')
			return
		}
		try {
			await book.mutateAsync({ serviceCode, occurredOn, qtyE6, note: note.trim() || null })
			setNote('')
			toast.success('Booking added.')
		} catch (err) {
			setError(errMsg(err))
		}
	}

	async function runCheckout() {
		try {
			const draft = await checkout.mutateAsync()
			// 204 — there was nothing unbilled to claim.
			if (!draft) {
				toast.info('Nothing to check out.')
				return
			}
			navigate(`/invoices/${draft.uid}`)
		} catch (err) {
			toast.error(errMsg(err))
		}
	}

	return (
		<div className="space-y-8">
			<section>
				<h1 className="text-lg font-semibold text-slate-900">Book a service</h1>
				<div className="mt-4 grid gap-4 sm:grid-cols-2">
					{catalogue.map((s) => (
						<ServiceCard
							key={s.uid}
							service={s}
							selected={s.code === serviceCode}
							onSelect={() => setServiceCode(s.code ?? '')}
						/>
					))}
				</div>
			</section>

			<section className="rounded-xl border border-slate-200 bg-white p-5">
				<form onSubmit={submit} className="space-y-4">
					<ErrorBanner message={error} />

					<div className="grid gap-4 sm:grid-cols-2">
						<Field label="Date" htmlFor="occurredOn" required>
							<Input
								id="occurredOn"
								type="date"
								value={occurredOn}
								onChange={(e) => setOccurredOn(e.target.value)}
							/>
						</Field>

						<Field
							label={selected ? `Quantity (${selected.unit}s)` : 'Quantity'}
							htmlFor="amount"
							required
							hint={selected ? `Billed per ${selected.unit}.` : undefined}
						>
							<Input
								id="amount"
								inputMode="decimal"
								value={amount}
								onChange={(e) => setAmount(e.target.value)}
							/>
						</Field>
					</div>

					<Field label="Note" htmlFor="note" hint="Appears on the invoice line.">
						<Input id="note" value={note} onChange={(e) => setNote(e.target.value)} />
					</Field>

					<Button type="submit" loading={book.isPending}>
						Add booking
					</Button>
				</form>
			</section>

			<section>
				<div className="flex items-center justify-between">
					<h2 className="text-base font-semibold text-slate-900">Not yet billed</h2>
					<Button
						onClick={() => void runCheckout()}
						loading={checkout.isPending}
						disabled={!hasParty}
					>
						Check out
					</Button>
				</div>

				{!hasParty && (
					<p className="mt-2 text-sm text-slate-600">
						Add your{' '}
						<Link to="/billing" className="font-medium text-brand-700 underline">
							billing details
						</Link>{' '}
						before checking out — an invoice needs a buyer.
					</p>
				)}

				{bookings.data?.nextCursor && (
					<p className="mt-2 text-sm text-slate-600">
						Showing the most recent {rows.length} bookings; checking out still bills
						every unbilled one.
					</p>
				)}

				<div className="mt-4">
					<BookingTable
						rows={unbilled}
						caption="Bookings not yet billed"
						empty={{
							title: 'Nothing unbilled',
							description: 'Bookings you add appear here until you check out.'
						}}
					/>
				</div>
			</section>

			{billed.length > 0 && (
				<section>
					<h2 className="text-base font-semibold text-slate-900">Billed</h2>
					<div className="mt-4">
						<BookingTable
							rows={billed}
							caption="Billed bookings"
							empty={{ title: 'Nothing billed yet' }}
						/>
					</div>
				</section>
			)}
		</div>
	)
}

function ServiceCard({
	service,
	selected,
	onSelect
}: {
	service: ServiceView
	selected: boolean
	onSelect: () => void
}) {
	return (
		<button
			type="button"
			onClick={onSelect}
			aria-pressed={selected}
			className={`rounded-xl border p-4 text-left ${
				selected ? 'border-brand-500 bg-brand-50' : 'border-slate-200 bg-white'
			}`}
		>
			<div className="font-medium text-slate-900">{service.name}</div>
			<div className="mt-1 text-sm text-slate-600">
				{money(service.unitPrice)} / {service.unit}
			</div>
		</button>
	)
}

function BookingTable({
	rows,
	caption,
	empty
}: {
	rows: Booking[]
	caption: string
	empty: { title: string; description?: string }
}) {
	return (
		<DataTable
			rows={rows}
			rowKey={(b) => b.uid}
			caption={caption}
			empty={empty}
			columns={[
				{ key: 'date', header: 'Date', cell: (b) => b.occurredOn },
				{ key: 'service', header: 'Service', cell: (b) => b.serviceCode },
				{ key: 'qty', header: 'Quantity', numeric: true, cell: (b) => qty(b.qtyE6) },
				{ key: 'note', header: 'Note', cell: (b) => b.note ?? '—' },
				{
					key: 'invoice',
					header: 'Invoice',
					cell: (b) =>
						b.invoiceUid ? (
							<Link
								to={`/invoices/${b.invoiceUid}`}
								className="font-medium text-brand-700 underline"
							>
								View
							</Link>
						) : (
							'—'
						)
				}
			]}
		/>
	)
}

// vim: ts=4
