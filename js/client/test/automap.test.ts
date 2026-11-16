// SPDX-License-Identifier: MPL-2.0
import { describe, expect, it } from 'vitest'

import { autoMap } from '../src/automap'

const fields = [
	{ key: 'name', label: 'Név', required: true },
	{ key: 'adoszam', label: 'Tax number', required: false },
	{ key: 'email', label: 'E-mail', required: false }
]
const syn = { email: ['mail', 'emailcim'] }

describe('autoMap', () => {
	it('folds accents and case', () => expect(autoMap(['ADÓSZÁM'], fields, syn).adoszam).toBe(0))
	it('matches a synonym', () => expect(autoMap(['x', 'Mail'], fields, syn).email).toBe(1))
	it('matches the folded label', () => expect(autoMap(['nev'], fields, syn).name).toBe(0))
	it('never reuses a taken header', () => {
		const two = [fields[0], { key: 'nev', label: 'nev', required: false }]
		expect(autoMap(['Név'], two, {}).nev).toBeNull()
	})
	it('maps no match to null', () => expect(autoMap(['foo'], fields, syn).name).toBeNull())
	it('ignores an empty header', () => {
		expect(autoMap(['', ' '], [{ key: '', label: '', required: false }], {})['']).toBeNull()
	})
})

// vim: ts=4
