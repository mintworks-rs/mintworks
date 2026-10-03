import * as React from 'react'

import type { OfferView, QuoteReq } from '@saas-framework/client'
import { errMsg, formatMoney, useOffers, useSubscriptions } from '@saas-framework/client'
import { QuoteDialog } from '~/components/QuoteDialog'
import { Badge, Button, ErrorBanner, Input, PageSpinner } from '~/components/ui'
import { LOCALE } from '~/lib/format'
import { liveIn, offerOf, perSeat } from '~/lib/offers'

export function Pricing() {
	const offers = useOffers()
	const subs = useSubscriptions()
	const [open, setOpen] = React.useState<{ req: QuoteReq; title: string } | null>(null)

	if (offers.isPending || subs.isPending) return <PageSpinner />
	if (offers.error || subs.error) return <ErrorBanner message={errMsg(offers.error ?? subs.error)} />

	const items = subs.data.items
	// An offer with no price is never sold: it is only granted as a reward.
	const sold = offers.data.items.filter((o) => o.prices.length > 0)

	return (
		<div className="flex flex-col gap-6">
			<h1 className="text-xl font-semibold text-slate-900">Pricing</h1>
			<div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
				{sold.map((o) => (
					<OfferCard
						key={o.uid}
						offer={o}
						current={(() => {
							const s = liveIn(items, o.family)
							return s && offerOf(s, offers.data.items)?.code === o.code
						})()}
						onChoose={(qty) => {
							const sub = liveIn(items, o.family)
							setOpen({
								req: { offer: o.code, qty, subscription: sub?.uid },
								title: sub ? `Switch to ${o.name}` : o.name
							})
						}}
					/>
				))}
			</div>
			{open && <QuoteDialog {...open} onClose={() => setOpen(null)} />}
		</div>
	)
}

function OfferCard({
	offer,
	current,
	onChoose
}: {
	offer: OfferView
	current: boolean | undefined
	onChoose: (qty: number) => void
}) {
	const [qty, setQty] = React.useState(1)
	return (
		<div className="flex flex-col gap-3 rounded-xl border border-slate-200 bg-white p-5">
			<div className="flex items-center justify-between">
				<h2 className="font-semibold text-slate-900">{offer.name}</h2>
				{current && <Badge tone="info">Current</Badge>}
			</div>
			<p className="text-lg text-slate-900">
				{formatMoney(offer.prices[0], LOCALE)}
				<span className="text-sm text-slate-500">
					{offer.kind === 'RECURRING'
						? ` / ${offer.interval === 'YEAR' ? 'year' : 'month'}${perSeat(offer) ? ' / seat' : ''}`
						: ''}
				</span>
			</p>
			{offer.trialDays > 0 && (
				<p className="text-sm text-slate-500">{offer.trialDays}-day free trial</p>
			)}
			<ul className="text-sm text-slate-600">
				{offer.entitlements.map((e) => (
					<li key={e.key}>
						{e.key}: {e.amount}
						{e.perSeat ? ' per seat' : ''}
					</li>
				))}
			</ul>
			{perSeat(offer) && (
				<Input
					type="number"
					min={1}
					aria-label="Seats"
					value={qty}
					onChange={(e) => setQty(Math.max(1, Math.trunc(Number(e.target.value)) || 1))}
				/>
			)}
			<Button className="mt-auto" disabled={current} onClick={() => onChoose(qty)}>
				{offer.kind === 'RECURRING' ? 'Choose' : 'Buy'}
			</Button>
		</div>
	)
}

// vim: ts=4
