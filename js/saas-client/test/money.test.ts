import { describe, expect, it } from 'vitest'

import {
	formatMoney,
	formatQty,
	localDate,
	parseAmount,
	sumByCurrency,
	summaryTotals,
	toQtyE6,
	vatRate
} from '../src/money'
import type { InvoiceSummary } from '../src/types'

/** ICU separates groups with NBSP or a narrow NBSP depending on locale data; normalise so the
 *  assertions test our arithmetic rather than which space this build of Node shipped. */
const norm = (s: string) => s.replace(/[  ]/g, ' ')

describe('formatMoney', () => {
	it('takes fraction digits from the wire string, not from Intl’s per-currency default', () => {
		// The bug this pins: Intl's HUF default is 0 digits, so `15 000 Ft` would silently drop
		// what the server sent.
		expect(norm(formatMoney({ amount: '15000.00', currency: 'HUF' }, 'hu-HU'))).toContain(
			'15 000,00'
		)
		expect(norm(formatMoney({ amount: '15000', currency: 'HUF' }, 'hu-HU'))).not.toContain(',')
	})

	it('formats the same wire amount per locale', () => {
		// Five integer digits, not four: hu-HU's CLDR minimumGroupingDigits is 2, so `1234,56`
		// carries no group separator at all and would pin the wrong thing.
		expect(norm(formatMoney({ amount: '12345.67', currency: 'EUR' }, 'hu-HU'))).toContain(
			'12 345,67'
		)
		expect(formatMoney({ amount: '12345.67', currency: 'EUR' }, 'en-US')).toContain('12,345.67')
	})

	it('keeps full precision on an amount a float would round', () => {
		expect(formatMoney({ amount: '9007199254740993.99', currency: 'EUR' }, 'en-US')).toContain(
			'9,007,199,254,740,993.99'
		)
	})

	it('renders a negative amount and a missing one', () => {
		expect(norm(formatMoney({ amount: '-500.00', currency: 'EUR' }, 'hu-HU'))).toContain(
			'500,00'
		)
		expect(formatMoney(null, 'hu-HU')).toBe('—')
		expect(formatMoney(undefined, 'en-US')).toBe('—')
	})
})

describe('formatQty', () => {
	it('drops the trailing zeros the wire pads to six places', () => {
		expect(formatQty('2.500000', 'hu-HU')).toBe('2,5')
		expect(formatQty('2.500000', 'en-US')).toBe('2.5')
		expect(formatQty('2.000000', 'en-US')).toBe('2')
		expect(formatQty('3', 'en-US')).toBe('3')
	})
})

describe('parseAmount', () => {
	it('accepts either decimal separator and answers the canonical wire string', () => {
		expect(parseAmount('1234,56', 'EUR')).toBe('1234.56')
		expect(parseAmount('1234.56', 'EUR')).toBe('1234.56')
		expect(parseAmount(' 12 ', 'EUR')).toBe('12.00')
		expect(parseAmount('12', 'HUF')).toBe('12')
	})

	it('rejects anything that is not one plain decimal', () => {
		for (const bad of ['1.2.3', '1,234.56', '1e3', '٣', '', '-', 'abc', '1 234']) {
			expect(parseAmount(bad, 'EUR')).toBeNull()
		}
	})

	it('rounds half-up to the currency’s scale, without a float', () => {
		expect(parseAmount('1.005', 'EUR')).toBe('1.01')
		expect(parseAmount('1.004', 'EUR')).toBe('1.00')
		expect(parseAmount('1.5', 'HUF')).toBe('2')
		expect(parseAmount('1.4', 'HUF')).toBe('1')
		expect(parseAmount('-1.5', 'HUF')).toBe('-2')
	})

	it('keeps the sign between -1 and 0', () => {
		expect(parseAmount('-0.006', 'EUR')).toBe('-0.01')
		expect(parseAmount('-0.004', 'EUR')).toBe('0.00')
		expect(parseAmount('-0.5', 'HUF')).toBe('-1')
		expect(parseAmount('-0.4', 'HUF')).toBe('0')
	})
})

describe('toQtyE6', () => {
	it('holds the two-integer-digit bound', () => {
		expect(toQtyE6('2.5')).toBe(2_500_000)
		expect(toQtyE6('99.999999')).toBe(99_999_999)
		// Three integer digits would pass MAX_SAFE_INTEGER at the 1e6 scaling.
		expect(toQtyE6('100')).toBeNull()
		expect(toQtyE6('0')).toBeNull()
		expect(toQtyE6('2.5000001')).toBeNull()
	})
})

describe('sumByCurrency', () => {
	it('sums strictly per currency and converts nothing', () => {
		const out = sumByCurrency([
			{ currency: 'HUF', amount: '1000' },
			{ currency: 'EUR', amount: '10.50' },
			{ currency: 'HUF', amount: '250' },
			{ currency: 'EUR', amount: '0.50' }
		])
		expect(out.get('HUF')).toBe('1250')
		expect(out.get('EUR')).toBe('11.00')
		expect(out.size).toBe(2)
	})

	it('answers an empty map for no rows, and keeps a zero bucket', () => {
		expect(sumByCurrency([]).size).toBe(0)
		expect(sumByCurrency([{ currency: 'EUR', amount: '0.00' }]).get('EUR')).toBe('0.00')
	})

	it('aligns scales instead of adding 1.00 to 1 and reporting 101', () => {
		const out = sumByCurrency([
			{ currency: 'EUR', amount: '1' },
			{ currency: 'EUR', amount: '1.00' }
		])
		expect(out.get('EUR')).toBe('2.00')
	})

	it('survives amounts past MAX_SAFE_INTEGER', () => {
		const out = sumByCurrency([
			{ currency: 'HUF', amount: '9007199254740993' },
			{ currency: 'HUF', amount: '1' }
		])
		expect(out.get('HUF')).toBe('9007199254740994')
	})
})

describe('summaryTotals', () => {
	const m = (currency: string, amount: string) => ({ currency, amount })
	const summary: InvoiceSummary = {
		statuses: [
			{
				status: 'ISSUED',
				currency: 'HUF',
				count: 2,
				net: m('HUF', '2000'),
				vat: m('HUF', '540'),
				gross: m('HUF', '2540'),
				paid: m('HUF', '1270')
			},
			{
				status: 'DRAFT',
				currency: 'HUF',
				count: 3,
				net: m('HUF', '500'),
				vat: m('HUF', '135'),
				gross: m('HUF', '635'),
				paid: m('HUF', '0')
			},
			{
				status: 'ISSUED',
				currency: 'EUR',
				count: 1,
				net: m('EUR', '100.00'),
				vat: m('EUR', '27.00'),
				gross: m('EUR', '127.00'),
				paid: m('EUR', '0.00')
			}
		],
		months: [
			{
				month: '2026-09',
				currency: 'HUF',
				count: 2,
				gross: m('HUF', '2540'),
				paid: m('HUF', '1270')
			},
			{
				month: '2026-08',
				currency: 'HUF',
				count: 1,
				gross: m('HUF', '999'),
				paid: m('HUF', '999')
			}
		],
		overdue: [{ currency: 'HUF', count: 1, outstanding: m('HUF', '1270') }],
		paidThisMonth: [m('HUF', '999')]
	}

	it('nets paid off gross and never sums two currencies together', () => {
		const rows = summaryTotals(summary)
		expect(rows.map((r) => r.currency)).toEqual(['EUR', 'HUF'])
		const huf = rows[1]
		expect(huf.outstanding).toBe('1270')
		expect(huf.drafts).toBe('635')
		expect(huf.draftCount).toBe(3)
		expect(huf.overdue).toBe('1270')
		expect(huf.overdueCount).toBe(1)
		// By payment date: not September's fulfilment-month `paid`.
		expect(huf.paidThisMonth).toBe('999')
		expect(rows[0].outstanding).toBe('127.00')
	})

	it('answers a currency the month has nothing paid in with zero, not undefined', () => {
		const rows = summaryTotals(summary)
		expect(rows[0].paidThisMonth).toBe('0')
		expect(rows[0].overdueCount).toBe(0)
	})
})

describe('localDate', () => {
	it('is the Budapest day, not the UTC one', () => {
		expect(localDate(new Date('2026-03-31T22:30:00Z'))).toBe('2026-04-01')
		expect(localDate(new Date('2026-01-15T12:00:00Z'))).toBe('2026-01-15')
	})
})

describe('vatRate', () => {
	it('formats basis points without a float', () => {
		expect(vatRate(2700)).toBe('27%')
		expect(vatRate(550)).toBe('5.50%')
		expect(vatRate(0)).toBe('0%')
		expect(vatRate(505)).toBe('5.05%')
	})
})

// vim: ts=4
