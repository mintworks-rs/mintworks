// SPDX-License-Identifier: MIT-0
// The booking example's own wire types. Everything the framework serves is in
// `@mintworks/client`; amounts stay strings there for the same reason.

import type { PayMethod } from '@mintworks/client'

export interface CheckoutRequest {
	method: PayMethod
	provider?: string
}

// --- bookings (the example's own extension) ---
export interface Booking {
	uid: string
	serviceCode: string
	/** YYYY-MM-DD. */
	occurredOn: string
	qtyE6: number
	note: string | null
	invoiceUid: string | null
}

export interface BookRequest {
	serviceCode: string
	occurredOn: string
	qtyE6: number
	note?: string | null
}
// vim: ts=4
