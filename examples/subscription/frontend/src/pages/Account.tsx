import * as React from 'react'

import type { OfferView, QuoteReq, Subscription } from '@mintworks/client'
import {
	errMsg,
	useEntitlements,
	useOffers,
	useSubscriptionAction,
	useSubscriptions
} from '@mintworks/client'
import { QuoteDialog } from '~/components/QuoteDialog'
import { useToast } from '~/components/Toast'
import { Badge, Button, EmptyState, ErrorBanner, Input, PageSpinner } from '~/components/ui'
import { date } from '~/lib/format'
import { offerOf } from '~/lib/offers'

type Open = { req: QuoteReq; title: string } | null

export function Account() {
	const subs = useSubscriptions()
	const offers = useOffers()
	const ent = useEntitlements()
	const [open, setOpen] = React.useState<Open>(null)

	const err = subs.error ?? offers.error ?? ent.error
	if (err) return <ErrorBanner message={errMsg(err)} />
	if (!subs.data || !offers.data || !ent.data) return <PageSpinner />

	return (
		<div className="flex flex-col gap-8">
			<section className="flex flex-col gap-4">
				<h1 className="text-xl font-semibold text-slate-900">Subscriptions</h1>
				{subs.data.items.length === 0 ? (
					<EmptyState title="No subscriptions" description="Pick a plan on Pricing." />
				) : (
					subs.data.items.map((s) => (
						<SubCard key={s.uid} sub={s} offers={offers.data.items} onQuote={setOpen} />
					))
				)}
			</section>

			<section className="flex flex-col gap-3">
				<h2 className="text-lg font-semibold text-slate-900">What you can use</h2>
				<dl className="grid grid-cols-2 gap-x-6 gap-y-1 rounded-xl border border-slate-200 bg-white p-5 text-sm">
					{ent.data.features.map((f) => (
						<React.Fragment key={f}>
							<dt className="text-slate-500">{f}</dt>
							<dd>enabled</dd>
						</React.Fragment>
					))}
					{Object.entries(ent.data.limits).map(([k, v]) => (
						<React.Fragment key={k}>
							<dt className="text-slate-500">{k}</dt>
							<dd>{v}</dd>
						</React.Fragment>
					))}
					{Object.entries(ent.data.meters).map(([k, m]) => (
						<React.Fragment key={k}>
							<dt className="text-slate-500">{k}</dt>
							<dd>
								{m.balance} left
								{m.nextExpiry && ` (next expiry ${date(m.nextExpiry)})`}
							</dd>
						</React.Fragment>
					))}
				</dl>
			</section>

			{open && <QuoteDialog {...open} onClose={() => setOpen(null)} />}
		</div>
	)
}

function SubCard({
	sub,
	offers,
	onQuote
}: {
	sub: Subscription
	offers: OfferView[]
	onQuote: (o: Open) => void
}) {
	const toast = useToast()
	const act = useSubscriptionAction()
	const offer = offerOf(sub, offers)
	const [seats, setSeats] = React.useState(sub.qty)
	const others = offers.filter(
		(o) => o.family && o.family === sub.family && o.code !== offer?.code && o.prices.length > 0
	)
	const live = sub.status === 'ACTIVE'

	function run(action: 'cancel' | 'resume' | 'cancel-change') {
		act.mutate(
			{ uid: sub.uid, action },
			{ onError: (e) => toast.error(errMsg(e)), onSuccess: () => toast.success('Updated.') }
		)
	}

	return (
		<div className="flex flex-col gap-3 rounded-xl border border-slate-200 bg-white p-5 text-sm">
			<div className="flex items-center gap-3">
				<h2 className="font-semibold text-slate-900">{offer?.name ?? sub.family ?? 'Add-on'}</h2>
				<Badge tone={live || sub.status === 'TRIALING' ? 'success' : 'warning'}>
					{sub.status}
				</Badge>
			</div>
			<p className="text-slate-600">
				{sub.qty} × {sub.price} {sub.currency}, current period ends {date(sub.periodEnd)}
				{sub.cancelAtPeriodEnd && ' — cancels then'}
				{sub.nextQty !== null && ` — changes to ${sub.nextQty} seats then`}
			</p>

			{live && others.length > 0 && (
				<div className="flex flex-wrap gap-2">
					{others.map((o) => (
						<Button
							key={o.code}
							variant="secondary"
							onClick={() =>
								onQuote({
									req: { offer: o.code, qty: sub.qty, subscription: sub.uid },
									title: `Switch to ${o.name}`
								})
							}
						>
							Switch to {o.name}
						</Button>
					))}
				</div>
			)}

			{live && offer && (
				<div className="flex items-end gap-2">
					<Input
						type="number"
						min={1}
						aria-label="Seats"
						className="max-w-24"
						value={seats}
						onChange={(e) => setSeats(Math.max(1, Math.trunc(Number(e.target.value)) || 1))}
					/>
					<Button
						variant="secondary"
						disabled={seats === sub.qty}
						onClick={() =>
							onQuote({
								req: { offer: offer.code, qty: seats, subscription: sub.uid },
								title: `Change to ${seats} seats`
							})
						}
					>
						Change seats
					</Button>
				</div>
			)}

			<div className="flex gap-2">
				{sub.status !== 'CANCELED' &&
					(sub.cancelAtPeriodEnd ? (
						<Button variant="secondary" loading={act.isPending} onClick={() => run('resume')}>
							Resume
						</Button>
					) : (
						<Button variant="danger" loading={act.isPending} onClick={() => run('cancel')}>
							Cancel at period end
						</Button>
					))}
				{/* A queued tier downgrade is not on the wire, only a seat change is. */}
				{sub.nextQty !== null && (
					<Button variant="ghost" loading={act.isPending} onClick={() => run('cancel-change')}>
						Keep current plan
					</Button>
				)}
			</div>
		</div>
	)
}

// vim: ts=4
