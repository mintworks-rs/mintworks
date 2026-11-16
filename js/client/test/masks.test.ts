// SPDX-License-Identifier: MPL-2.0
import { describe, expect, it } from 'vitest'

import { maskBankAccount, maskTaxNumber, taxNumberError } from '../src/masks'

describe('masks', () => {
	it('formats as typed', () => {
		expect(maskTaxNumber('12345676241')).toBe('12345676-2-41')
		expect(maskTaxNumber('1234567')).toBe('1234567')
		expect(maskBankAccount('117730161111111800000000')).toBe('11773016-11111118-00000000')
	})

	it('checks a tax number', () => {
		expect(taxNumberError('')).toBe('required')
		// 9,7,3,1,9,7,3 over 1234567 sums to 144: check digit 6.
		expect(taxNumberError('12345676-2-41')).toBeNull()
		expect(taxNumberError('12345675-2-41')).toBe('invalid')
		expect(taxNumberError('12345676-6-41')).toBe('invalid')
		expect(taxNumberError('1234567')).toBe('invalid')
	})
})

// vim: ts=4
