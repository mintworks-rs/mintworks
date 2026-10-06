// SPDX-License-Identifier: MPL-2.0
import { describe, expect, it } from 'vitest'

import { ERRORS_EN, ERRORS_HU, errText, fieldErrors } from '../src/errors'
import { ServerError } from '../src/http'

describe('errText', () => {
	it('answers the dictionary entry for a known code', () => {
		const e = new ServerError('E-AUTH-CREDENTIALS', 'invalid credentials', 401)
		expect(errText(e, ERRORS_EN)).toBe(ERRORS_EN['E-AUTH-CREDENTIALS'])
		expect(errText(e, ERRORS_HU)).toBe(ERRORS_HU['E-AUTH-CREDENTIALS'])
		expect(errText(e, ERRORS_EN)).not.toBe(errText(e, ERRORS_HU))
	})

	it('falls back to the server’s errStr on an unknown code, never to a blank string', () => {
		const e = new ServerError('E-INV-FROM-THE-FUTURE', 'the server explains itself', 400)
		expect(errText(e, ERRORS_EN)).toBe('the server explains itself')
		expect(errText(new ServerError('E-WHO-KNOWS', '', 400), ERRORS_EN)).toBe('E-WHO-KNOWS')
	})

	it('handles anything else that can be thrown', () => {
		expect(errText(new Error('offline'), ERRORS_EN)).toBe('offline')
		expect(errText('a string', ERRORS_EN)).toBe(ERRORS_EN['E-CORE-INTERNAL'])
	})
})

describe('fieldErrors', () => {
	it('maps each field’s error code through the dictionary', () => {
		// `fields` is valued by a code, not by prose — `GET /api/invoices?status=nonsense`
		// answers exactly this.
		const e = new ServerError('E-CORE-VALIDATION', 'validation failed', 400, {
			status: 'E-CORE-FORMAT'
		})
		expect(fieldErrors(e, ERRORS_EN)).toEqual({ status: ERRORS_EN['E-CORE-FORMAT'] })
		expect(fieldErrors(e, ERRORS_HU)).toEqual({ status: ERRORS_HU['E-CORE-FORMAT'] })
	})

	it('falls back per field, and answers nothing when there are no fields', () => {
		const e = new ServerError('E-CORE-VALIDATION', 'validation failed', 400, {
			qty: 'E-SOMETHING-NEW'
		})
		expect(fieldErrors(e, ERRORS_EN)).toEqual({ qty: 'validation failed' })
		expect(fieldErrors(new ServerError('E-CORE-NOTFOUND', 'nope', 404), ERRORS_EN)).toEqual({})
		expect(fieldErrors(new Error('offline'), ERRORS_EN)).toEqual({})
	})
})

describe('the dictionaries', () => {
	it('carry the same keys in both languages', () => {
		expect(Object.keys(ERRORS_HU).sort()).toEqual(Object.keys(ERRORS_EN).sort())
	})

	it('leave no entry blank', () => {
		for (const [code, text] of Object.entries({ ...ERRORS_EN, ...ERRORS_HU })) {
			expect(text, code).not.toBe('')
		}
	})
})

// vim: ts=4
