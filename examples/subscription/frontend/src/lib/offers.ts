// SPDX-License-Identifier: MIT-0
import type { OfferView, Subscription } from '@mintworks/client'

/** The org's live subscription in `family`, if any: the one a tier change would move. */
export function liveIn(subs: Subscription[], family: string | null): Subscription | undefined {
	return family ? subs.find((s) => s.family === family && s.status !== 'CANCELED') : undefined
}

// ponytail: the wire `Subscription` carries no offer code, so the offer is guessed by family and
// price; a repriced or grandfathered sub matches nothing. Expose the code on the wire to fix.
export function offerOf(sub: Subscription, offers: OfferView[]): OfferView | undefined {
	return offers.find(
		(o) =>
			o.family === sub.family &&
			o.prices.some((p) => p.currency === sub.currency && p.amount === sub.price)
	)
}

/** Whether the offer is priced per seat, so buying it asks for a quantity. */
export const perSeat = (o: OfferView) => o.entitlements.some((e) => e.perSeat)

// vim: ts=4
