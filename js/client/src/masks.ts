// SPDX-License-Identifier: MPL-2.0

/** `12345678-1-12`, typed as digits. */
export function maskTaxNumber(v: string): string {
	const d = v.replace(/\D/g, '').slice(0, 11)
	return [d.slice(0, 8), d.slice(8, 9), d.slice(9)].filter(Boolean).join('-')
}

/** `12345678-12345678[-12345678]`, typed as digits. */
export function maskBankAccount(v: string): string {
	const d = v.replace(/\D/g, '').slice(0, 24)
	return (d.match(/.{1,8}/g) ?? []).join('-')
}

/** A Hungarian tax number's fault: `required` when empty (an optional field checks only input),
 *  `invalid` on a bad shape, VAT digit or check digit; null when valid. */
export function taxNumberError(v: string): 'required' | 'invalid' | null {
	const d = v.replace(/\D/g, '')
	if (d === '') return 'required'
	// The 9th digit is NAV's `vatCode`, restricted to 1-5.
	if (d.length !== 11 || !/[1-5]/.test(d[8] ?? '')) return 'invalid'
	// The 8th digit is the törzsszám check digit, weights 9,7,3,1 over the first seven.
	const sum = [9, 7, 3, 1, 9, 7, 3].reduce((s, w, i) => s + w * Number(d[i]), 0)
	if ((10 - (sum % 10)) % 10 !== Number(d[7])) return 'invalid'
	return null
}

// vim: ts=4
