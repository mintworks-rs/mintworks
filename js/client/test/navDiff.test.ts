// SPDX-License-Identifier: MPL-2.0
import { describe, expect, it } from 'vitest'

import { NAV_DIFF_FIELDS, type NavDiffFmt, navDiffCount, navDiffRows } from '../src/navDiff'
import type { NavFieldDiff } from '../src/types'

/** A stand-in dictionary: just the words these cases read. */
const dict: Record<string, string> = {
	'navDiff.yes': 'Yes',
	'navDiff.no': 'No',
	'navDiff.section.BUYER': 'Buyer',
	'navDiff.section.TOTALS': 'Totals',
	'navDiff.section.LINE': 'Line {n}',
	'navDiff.section.VAT_RATE': 'Rate {rate}',
	'navDiff.field.other': 'Other',
	'navDiff.code.paymentMethod.CASH': 'Cash',
	'navDiff.code.customerVatStatus.DOMESTIC': 'Domestic'
}

const fmt: NavDiffFmt = {
	t: (key, vars) =>
		dict[key] === undefined
			? key
			: dict[key].replace(/\{(\w+)\}/g, (m, k) => (vars && k in vars ? String(vars[k]) : m)),
	money: (m) => `${m.amount} ${m.currency}`,
	qty: (d) => d.replace(/\.?0+$/, ''),
	date: (iso) => `date:${iso}`
}

const item = (over: Partial<NavFieldDiff>): NavFieldDiff => ({
	section: 'DETAILS',
	index: null,
	rate: null,
	field: 'invoiceNumber',
	ours: 'A',
	nav: 'B',
	...over
})
const diff = (...items: NavFieldDiff[]) => ({ total: items.length, items })
const row = (over: Partial<NavFieldDiff>) => navDiffRows(diff(item(over)), 'EUR', fmt)[0].rows[0]

describe('navDiffRows', () => {
	it('labels every known field by its key', () => {
		for (const field of Object.keys(NAV_DIFF_FIELDS)) {
			expect(row({ field }).label).toBe(`navDiff.field.${field}`)
		}
	})

	it('groups in reading order, whatever order the items came in', () => {
		const grouped = navDiffRows(
			diff(
				item({ section: 'TOTALS', field: 'invoiceNetAmount' }),
				item({ section: 'LINE', index: 2, field: 'quantity' }),
				item({ section: 'VAT_RATE', index: 1, rate: '0.27', field: 'vatRateNetAmount' }),
				item({ section: 'LINE', index: 1, field: 'quantity' }),
				item({ section: 'LINE', index: 1, field: 'unitPrice' }),
				item({ section: 'BUYER', field: 'customerName' })
			),
			'HUF',
			fmt
		)
		expect(grouped.map((g) => g.heading)).toEqual([
			'Buyer',
			'Line 1',
			'Line 2',
			'Rate 27%',
			'Totals'
		])
		expect(grouped[1].rows).toHaveLength(2)
		const rate = (over: Partial<NavFieldDiff>) =>
			navDiffRows(diff(item({ section: 'VAT_RATE', index: 2, ...over })), 'HUF', fmt)[0].heading
		expect(rate({ rate: 'AAM' })).toBe('Rate AAM')
		expect(rate({})).toBe('Rate 2.')
	})

	it('shows a HUF twin once unless it says something else', () => {
		const amounts = { section: 'TOTALS', ours: '100.00', nav: '200.00' } as const
		const huf = diff(
			item({ ...amounts, field: 'invoiceNetAmount' }),
			item({ ...amounts, field: 'invoiceNetAmountHUF' })
		)
		expect(navDiffRows(huf, 'HUF', fmt)[0].rows).toHaveLength(1)
		expect(navDiffCount(huf)).toBe(1)
		const eur = diff(
			item({ ...amounts, field: 'invoiceNetAmount' }),
			item({ ...amounts, field: 'invoiceNetAmountHUF', ours: '39000.00', nav: '78000.00' })
		)
		expect(navDiffRows(eur, 'EUR', fmt)[0].rows).toHaveLength(2)
		const lines = diff(
			item({ ...amounts, section: 'LINE', index: 1, field: 'lineNetAmount' }),
			item({ ...amounts, section: 'LINE', index: 2, field: 'lineNetAmountHUF' })
		)
		expect(navDiffCount(lines)).toBe(2)
		expect(navDiffCount({ ...huf, total: 12 })).toBe(11)
	})

	it('shows an unknown field by value, never by element name', () => {
		const unknown = row({ field: 'someNewElement', ours: 'x', nav: null })
		expect(unknown.label).toBe('Other')
		expect([unknown.ours, unknown.nav]).toEqual(['x', null])
	})

	it.each([
		['invoiceNumber', 'A/1', 'A/1'],
		['paymentDate', '2026-11-07', 'date:2026-11-07'],
		['paymentDate', 'soon', 'soon'],
		['unitPrice', '10.50', '10.50 EUR'],
		['unitPrice', 'n/a', 'n/a'],
		['lineNetAmountHUF', '10.50', '10.50 HUF'],
		['vatPercentage', '0.27', '27%'],
		['vatPercentage', '0.07', '7%'],
		['quantity', '2.500000', '2.5'],
		['individualExemption', 'true', 'Yes'],
		['individualExemption', 'false', 'No'],
		['paymentMethod', 'CASH', 'Cash'],
		['paymentMethod', 'VOUCHER', 'VOUCHER'],
		['customerVatStatus', 'DOMESTIC', 'Domestic'],
		['lineNatureIndicator', 'SERVICE', 'SERVICE']
	])('formats %s %s', (field, ours, want) => expect(row({ field, ours }).ours).toBe(want))
})

// vim: ts=4
