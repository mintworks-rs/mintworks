// Amounts stay strings the whole way. `Money` is i64 minor units on the server and a
// JavaScript number would lose the cent, so nothing here calls Number() on an amount.

import type { MoneyWire } from '~/api/types'

/** `{"amount":"15000.00","currency":"HUF"}` → `15 000.00 HUF`. String work only. */
export function money(m: MoneyWire | null | undefined): string {
	if (!m) return '—'
	const neg = m.amount.startsWith('-')
	const [int, frac] = (neg ? m.amount.slice(1) : m.amount).split('.')
	const grouped = int.replace(/\B(?=(\d{3})+(?!\d))/g, ' ')
	return `${neg ? '-' : ''}${grouped}${frac ? `.${frac}` : ''} ${m.currency}`
}

/** `LineView.qty` arrives as a decimal string: `'2.500000'` → `2.5`. String work only, so it
 *  cannot be confused with `qty()` below, which takes the raw 1e6 integer bookings carry. */
export function qtyDec(qtyDecimal: string): string {
	return qtyDecimal.includes('.') ? qtyDecimal.replace(/\.?0+$/, '') : qtyDecimal
}

/** `Booking.qtyE6` is scaled 1e6: 2500000 → `2.5`. Integer maths, no rounding step. */
export function qty(qtyE6: number): string {
	const neg = qtyE6 < 0
	const v = Math.abs(qtyE6)
	const frac = String(v % 1_000_000)
		.padStart(6, '0')
		.replace(/0+$/, '')
	return `${neg ? '-' : ''}${Math.trunc(v / 1_000_000)}${frac ? `.${frac}` : ''}`
}

/** The booking form's inverse: `'2.5'` → 2500000, null when it is not a positive quantity.
 *
 *  Two integer digits, not unbounded: `Number(int) * 1_000_000` passes MAX_SAFE_INTEGER above
 *  ~9e9 and posts a qtyE6 that is not what was typed, into an i64 column, with no error. Two
 *  is what `MAX_QTY_E6` in `example/backend/src/bookings.rs` allows for (24 units). */
export function toQtyE6(input: string): number | null {
	if (!/^\d{1,2}(\.\d{1,6})?$/.test(input.trim())) return null
	const [int, frac = ''] = input.trim().split('.')
	const e6 = Number(int) * 1_000_000 + Number(frac.padEnd(6, '0'))
	return e6 > 0 ? e6 : null
}

/** VAT rates are integer basis points: 2700 → `27%`. */
export function vatRate(bp: number): string {
	return bp % 100 === 0 ? `${bp / 100}%` : `${(bp / 100).toFixed(2)}%`
}

/**
 * Europe/Budapest, not UTC: `toISOString()` rolls the date back an hour or two before midnight
 * local, and the value is the fulfilment date on a legal invoice. `'sv-SE'` is the locale whose
 * short date format is already YYYY-MM-DD.
 */
export function localDate(d: Date = new Date()): string {
	return d.toLocaleDateString('sv-SE', { timeZone: 'Europe/Budapest' })
}

/** ISO-8601 from the wire → the date alone, which is all any screen here shows. */
export function date(iso: string | null | undefined): string {
	return iso ? localDate(new Date(iso)) : '—'
}

// vim: ts=4
