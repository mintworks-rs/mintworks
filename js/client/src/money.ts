// Amounts stay strings the whole way. `Money` is i64 minor units on the server and a
// JavaScript number would lose the cent, so nothing here calls Number() on an amount.

import type { InvoiceSummary, MoneyWire } from './types'

/** The wire amount, always `-?\d+(\.\d+)?`, in the shape `Intl.NumberFormat` accepts. */
function numeric(amount: string): Intl.StringNumericLiteral {
	return amount as Intl.StringNumericLiteral
}

function fractionDigits(amount: string): number {
	const dot = amount.indexOf('.')
	return dot < 0 ? 0 : amount.length - dot - 1
}

/**
 * `{"amount":"15000.00","currency":"HUF"}` → `15 000,00 Ft` at `hu-HU`.
 *
 * `format()` takes a string, so the amount never becomes a float. Fraction digits come from
 * the wire string, not from Intl's per-currency default: HUF defaults to 0 and would print
 * `15 000 Ft` for an amount the server sent with two decimals.
 */
export function formatMoney(m: MoneyWire | null | undefined, locale: string): string {
	if (!m) return '—'
	const digits = fractionDigits(m.amount)
	return new Intl.NumberFormat(locale, {
		style: 'currency',
		currency: m.currency,
		minimumFractionDigits: digits,
		maximumFractionDigits: digits
	}).format(numeric(m.amount))
}

/** `LineView.qty` arrives as a decimal string: `'2.500000'` → `2,5` at `hu-HU`. */
export function formatQty(qtyDecimal: string, locale: string): string {
	const trimmed = qtyDecimal.includes('.')
		? qtyDecimal.replace(/0+$/, '').replace(/\.$/, '')
		: qtyDecimal
	return new Intl.NumberFormat(locale, {
		maximumFractionDigits: fractionDigits(trimmed)
	}).format(numeric(trimmed))
}

/**
 * A typed amount → the canonical wire string, or null when it is not one.
 *
 * `,` and `.` are both accepted as **the decimal separator** — hu types the comma — but only
 * one of them: a second separator means the typist meant grouping, and `1,234.5` reads as
 * 1.234 to one half of the audience and 1234.5 to the other. Refuse rather than guess.
 */
export function parseAmount(input: string, currency: string): string | null {
	const raw = input.trim()
	if ((raw.match(/[.,]/g) ?? []).length > 1) return null
	const t = raw.replace(',', '.')
	if (!/^-?\d+(\.\d+)?$/.test(t)) return null
	// The scale is the currency's, not the typist's: `1.5` in HUF is 2 forints at issue time,
	// and rounding here rather than at the server keeps what is shown and what is stored equal.
	const scale = currency === 'HUF' ? 0 : 2
	const [int, frac = ''] = t.split('.')
	if (frac.length <= scale) {
		const exact = scale === 0 ? int : `${int}.${frac.padEnd(scale, '0')}`
		// `-0` / `-0.00` is not a wire amount, and the server's `Money(i64)` has no negative zero.
		return /^-0(\.0*)?$/.test(exact) ? exact.slice(1) : exact
	}
	// Half-up on the digit string, no float: carry through the integer part with BigInt. The sign
	// comes from the input, not from `keep`: `BigInt('-0' + '00')` is `0n`.
	const keep = BigInt(`${int.replace('-', '')}${frac.slice(0, scale)}`)
	const rounded = Number(frac[scale]) >= 5 ? keep + 1n : keep
	const digits = rounded.toString().padStart(scale + 1, '0')
	const sign = t.startsWith('-') && rounded !== 0n ? '-' : ''
	return scale === 0
		? `${sign}${digits}`
		: `${sign}${digits.slice(0, -scale)}.${digits.slice(-scale)}`
}

/** VAT rates are integer basis points: 2700 → `27%`. */
export function vatRate(bp: number): string {
	const whole = Math.trunc(bp / 100)
	const frac = Math.abs(bp % 100)
	return frac === 0 ? `${whole}%` : `${whole}.${String(frac).padStart(2, '0')}%`
}

/** The booking form's inverse: `'2.5'` → 2500000, null when it is not a positive quantity.
 *
 *  Two integer digits, not unbounded: `Number(int) * 1_000_000` passes MAX_SAFE_INTEGER above
 *  ~9e9 and posts a qtyE6 that is not what was typed, into an i64 column, with no error. Two
 *  is what `MAX_QTY_E6` in `examples/booking/app-rust/src/bookings.rs` allows for (24 units). */
export function toQtyE6(input: string): number | null {
	if (!/^\d{1,2}(\.\d{1,6})?$/.test(input.trim())) return null
	const [int, frac = ''] = input.trim().split('.')
	const e6 = Number(int) * 1_000_000 + Number(frac.padEnd(6, '0'))
	return e6 > 0 ? e6 : null
}

/**
 * Totals per currency, for the dashboard tiles. BigInt over the minor units — the no-floats
 * rule reaches the frontend too — and **nothing is converted**: rows of different currencies
 * never meet, because no rate applies to a mixed sum.
 */
export function sumByCurrency(rows: { currency: string; amount: string }[]): Map<string, string> {
	const acc = new Map<string, { minor: bigint; scale: number }>()
	for (const r of rows) {
		const scale = fractionDigits(r.amount)
		const minor = BigInt(r.amount.replace('.', ''))
		const seen = acc.get(r.currency)
		if (!seen) {
			acc.set(r.currency, { minor, scale })
			continue
		}
		// A currency's rows share a scale in practice; align defensively rather than add
		// `1.00` to `1` and report 101.
		const to = Math.max(seen.scale, scale)
		const lift = (v: bigint, from: number) => v * 10n ** BigInt(to - from)
		acc.set(r.currency, { minor: lift(seen.minor, seen.scale) + lift(minor, scale), scale: to })
	}
	const out = new Map<string, string>()
	for (const [currency, { minor, scale }] of acc) {
		const neg = minor < 0n
		const digits = (neg ? -minor : minor).toString().padStart(scale + 1, '0')
		const whole = scale === 0 ? digits : `${digits.slice(0, -scale)}.${digits.slice(-scale)}`
		out.set(currency, `${neg ? '-' : ''}${whole}`)
	}
	return out
}

/** One dashboard row. Amounts are wire strings in that row's own currency. */
export interface CurrencyTotals {
	currency: string
	/** `gross - paid` over the `ISSUED` buckets: a half-paid invoice is half outstanding. */
	outstanding: string
	overdue: string
	overdueCount: number
	drafts: string
	draftCount: number
	paidThisMonth: string
}

/**
 * The summary reshaped into one tile row per currency — the one place client-side money
 * arithmetic happens, and it works on minor units through `sumByCurrency`, never on formatted
 * strings.
 */
export function summaryTotals(s: InvoiceSummary): CurrencyTotals[] {
	const issued = s.statuses.filter((b) => b.status === 'ISSUED')
	const drafts = s.statuses.filter((b) => b.status === 'DRAFT')
	// `sumByCurrency` only adds, and a negated wire amount is still a wire amount.
	const negate = (a: string) => (a.startsWith('-') ? a.slice(1) : `-${a}`)
	const outstanding = sumByCurrency([
		...issued.map((b) => b.gross),
		...issued.map((b) => ({ currency: b.paid.currency, amount: negate(b.paid.amount) }))
	])
	const overdue = sumByCurrency(s.overdue.map((b) => b.outstanding))
	const draft = sumByCurrency(drafts.map((b) => b.gross))
	const paid = sumByCurrency(s.paidThisMonth)

	const counts = (rows: { currency: string; count: number }[]) => {
		const m = new Map<string, number>()
		for (const r of rows) m.set(r.currency, (m.get(r.currency) ?? 0) + r.count)
		return m
	}
	const overdueCounts = counts(s.overdue)
	const draftCounts = counts(drafts)

	const currencies = [
		...new Set([...outstanding.keys(), ...overdue.keys(), ...draft.keys(), ...paid.keys()])
	].sort()
	return currencies.map((currency) => ({
		currency,
		outstanding: outstanding.get(currency) ?? '0',
		overdue: overdue.get(currency) ?? '0',
		overdueCount: overdueCounts.get(currency) ?? 0,
		drafts: draft.get(currency) ?? '0',
		draftCount: draftCounts.get(currency) ?? 0,
		paidThisMonth: paid.get(currency) ?? '0'
	}))
}

/** The deployment's zone: every calendar date the server reasons about is a Budapest one. */
export const ZONE = 'Europe/Budapest'

/**
 * `d`'s calendar day in {@link ZONE} as `YYYY-MM-DD`. Not `toISOString()`, which is UTC and a
 * day behind before 01:00/02:00 local. `'sv-SE'`'s short date format is already ISO.
 */
export function localDate(d: Date = new Date()): string {
	return d.toLocaleDateString('sv-SE', { timeZone: ZONE })
}

// vim: ts=4
