// Formatting this app needs and `@mintworks/client` deliberately does not ship: the
// package's money surface is the table in its `README.md`, and these are booking's.

import type { MoneyWire } from '@mintworks/client'
import { localDate } from '@mintworks/client'

export { localDate }

/** Booking has no locale switcher — one constant, and the whole app formats through it. */
export const LOCALE = 'hu-HU'

/** `Booking.qtyE6` is scaled 1e6: 2500000 → `2.5`. Integer maths, no rounding step. */
export function qty(qtyE6: number): string {
	const neg = qtyE6 < 0
	const v = Math.abs(qtyE6)
	const frac = String(v % 1_000_000)
		.padStart(6, '0')
		.replace(/0+$/, '')
	return `${neg ? '-' : ''}${Math.trunc(v / 1_000_000)}${frac ? `.${frac}` : ''}`
}

/** `gross - paid`, as a wire amount. BigInt over the minor units, never `Number()`: the
 *  no-floats rule reaches the frontend too. Same-currency wires share a scale, and that is the
 *  only pair this is ever called with. */
export function due(gross: MoneyWire, paid: MoneyWire): MoneyWire {
	const minor = (m: MoneyWire) => BigInt(m.amount.replace('.', ''))
	const scale = gross.amount.includes('.') ? gross.amount.split('.')[1].length : 0
	const left = minor(gross) - minor(paid)
	const neg = left < 0n
	const digits = (neg ? -left : left).toString().padStart(scale + 1, '0')
	const whole = scale === 0 ? digits : `${digits.slice(0, -scale)}.${digits.slice(-scale)}`
	return { amount: `${neg ? '-' : ''}${whole}`, currency: gross.currency }
}

/** ISO-8601 from the wire → the date alone, which is all any screen here shows. */
export function date(iso: string | null | undefined): string {
	return iso ? localDate(new Date(iso)) : '—'
}

// vim: ts=4
