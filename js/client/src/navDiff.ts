// SPDX-License-Identifier: MPL-2.0
import type { MoneyWire, NavDiff, NavFieldDiff } from './types'

type Format = 'text' | 'date' | 'money' | 'moneyHuf' | 'percent' | 'quantity' | 'bool' | 'code'

/** What `crates/nav/src/xml.rs` emits, by element name; the label is `navDiff.field.<name>`. A
 *  `HUF` amount is a twin of the field without the suffix. */
export const NAV_DIFF_FIELDS: Record<string, Format> = {
	invoiceNumber: 'text',
	invoiceIssueDate: 'date',
	completenessIndicator: 'bool',
	originalInvoiceNumber: 'text',
	modifyWithoutMaster: 'bool',
	modificationIndex: 'text',
	taxpayerId: 'text',
	vatCode: 'text',
	countyCode: 'text',
	communityVatNumber: 'text',
	thirdStateTaxId: 'text',
	supplierName: 'text',
	customerName: 'text',
	countryCode: 'text',
	postalCode: 'text',
	city: 'text',
	additionalAddressDetail: 'text',
	supplierBankAccountNumber: 'text',
	individualExemption: 'bool',
	customerVatStatus: 'code',
	invoiceCategory: 'code',
	invoiceDeliveryDate: 'date',
	invoiceDeliveryPeriodStart: 'date',
	invoiceDeliveryPeriodEnd: 'date',
	periodicalSettlement: 'bool',
	smallBusinessIndicator: 'bool',
	currencyCode: 'text',
	exchangeRate: 'quantity',
	paymentMethod: 'code',
	paymentDate: 'date',
	cashAccountingIndicator: 'bool',
	invoiceAppearance: 'code',
	mergedItemIndicator: 'bool',
	lineNumber: 'text',
	lineNumberReference: 'text',
	lineOperation: 'code',
	lineExpressionIndicator: 'bool',
	lineNatureIndicator: 'code',
	lineDescription: 'text',
	quantity: 'quantity',
	unitOfMeasure: 'code',
	unitOfMeasureOwn: 'text',
	unitPrice: 'money',
	discountDescription: 'text',
	discountValue: 'money',
	discountRate: 'percent',
	lineNetAmount: 'money',
	lineNetAmountHUF: 'moneyHuf',
	vatPercentage: 'percent',
	case: 'code',
	reason: 'text',
	vatRateNetAmount: 'money',
	vatRateNetAmountHUF: 'moneyHuf',
	vatRateVatAmount: 'money',
	vatRateVatAmountHUF: 'moneyHuf',
	vatRateGrossAmount: 'money',
	vatRateGrossAmountHUF: 'moneyHuf',
	invoiceNetAmount: 'money',
	invoiceNetAmountHUF: 'moneyHuf',
	invoiceVatAmount: 'money',
	invoiceVatAmountHUF: 'moneyHuf',
	invoiceGrossAmount: 'money',
	invoiceGrossAmountHUF: 'moneyHuf'
}

/** The `code` fields that mean something to a reader; their words are `navDiff.code.<field>.<value>`. */
const CODES = new Set(['paymentMethod', 'invoiceAppearance', 'customerVatStatus', 'case'])

const SECTIONS: NavFieldDiff['section'][] = ['SELLER', 'BUYER', 'DETAILS', 'LINE', 'VAT_RATE', 'TOTALS']

const NUMBER = /^[+-]?\d+(\.\d+)?$/
const DATE = /^\d{4}-\d{2}-\d{2}/

/** The caller's formatters. `t` looks up a dictionary key and answers the key itself when it has
 *  no entry; the keys are `navDiff.yes|no`, `navDiff.section.<SECTION>` (`LINE` gets `{n}`,
 *  `VAT_RATE` gets `{rate}`), `navDiff.field.<name>|other` and `navDiff.code.<field>.<value>`. */
export interface NavDiffFmt {
	t: (key: string, vars?: Record<string, string | number>) => string
	money: (m: MoneyWire) => string
	qty: (decimal: string) => string
	date: (iso: string) => string
}

export interface NavDiffRow {
	key: string
	label: string
	/** Formatted; null when that side has no such value. */
	ours: string | null
	nav: string | null
}

export interface NavDiffGroup {
	key: string
	heading: string
	rows: NavDiffRow[]
}

/** `0.27` → `27%`. Rounded first: `0.07 * 100` is not 7 in floating point. */
function percent(v: string, fmt: NavDiffFmt): string {
	return `${fmt.qty((Number(v) * 100).toFixed(2))}%`
}

/** A value NAV's copy holds may be anything, so what does not fit its format shows as it is. */
function format(field: string, v: string, currency: string, fmt: NavDiffFmt): string {
	const number = NUMBER.test(v)
	switch (NAV_DIFF_FIELDS[field]) {
		case 'date':
			return DATE.test(v) ? fmt.date(v) : v
		case 'money':
			return number ? fmt.money({ amount: v, currency }) : v
		case 'moneyHuf':
			return number ? fmt.money({ amount: v, currency: 'HUF' }) : v
		case 'percent':
			return number ? percent(v, fmt) : v
		case 'quantity':
			return number ? fmt.qty(v) : v
		case 'bool':
			if (v !== 'true' && v !== 'false') return v
			return fmt.t(v === 'true' ? 'navDiff.yes' : 'navDiff.no')
		case 'code': {
			if (!CODES.has(field)) return v
			const key = `navDiff.code.${field}.${v}`
			const word = fmt.t(key)
			return word === key ? v : word
		}
		default:
			return v
	}
}

/** The items worth a row: a `…HUF` twin saying what its base field says is the same fact twice,
 *  which on a HUF invoice is every amount. */
function visible(items: NavFieldDiff[]): NavFieldDiff[] {
	return items.filter((i) => {
		if (!i.field.endsWith('HUF')) return true
		const base = items.find(
			(b) =>
				b.field === i.field.slice(0, -3) && b.section === i.section && b.index === i.index
		)
		return !(base && base.ours === i.ours && base.nav === i.nav)
	})
}

/** How many differences a reader sees, the ones past the stored cap included. */
export function navDiffCount(diff: NavDiff): number {
	return visible(diff.items).length + navDiffHidden(diff)
}

/** How many differences were not stored. */
export function navDiffHidden(diff: NavDiff): number {
	return diff.total - diff.items.length
}

function heading(i: NavFieldDiff, fmt: NavDiffFmt): string {
	if (i.section === 'LINE') return fmt.t('navDiff.section.LINE', { n: i.index ?? '' })
	if (i.section === 'VAT_RATE') {
		const rate = i.rate ?? `${i.index}.`
		return fmt.t('navDiff.section.VAT_RATE', {
			rate: NUMBER.test(rate) ? percent(rate, fmt) : rate
		})
	}
	return fmt.t(`navDiff.section.${i.section}`)
}

/** The differences, grouped under a heading per section in the invoice's own reading order. */
export function navDiffRows(diff: NavDiff, currency: string, fmt: NavDiffFmt): NavDiffGroup[] {
	const groups = new Map<string, NavDiffGroup & { order: number; index: number }>()
	for (const i of visible(diff.items)) {
		const key = `${i.section}-${i.index ?? 0}`
		let group = groups.get(key)
		if (!group) {
			const order = SECTIONS.indexOf(i.section)
			group = { key, heading: heading(i, fmt), rows: [], order, index: i.index ?? 0 }
			groups.set(key, group)
		}
		const known = i.field in NAV_DIFF_FIELDS
		const side = (v: string | null) => (v === null ? null : format(i.field, v, currency, fmt))
		group.rows.push({
			key: `${i.field}-${group.rows.length}`,
			label: fmt.t(known ? `navDiff.field.${i.field}` : 'navDiff.field.other'),
			ours: side(i.ours),
			nav: side(i.nav)
		})
	}
	return [...groups.values()].sort((a, b) => a.order - b.order || a.index - b.index)
}

// vim: ts=4
