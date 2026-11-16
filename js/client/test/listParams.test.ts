// SPDX-License-Identifier: MPL-2.0
import { describe, expect, it } from 'vitest'

import { filterKey, footer, pageOf, pageSince } from '../src/listParams'

const q = (s: string) => new URLSearchParams(s)

describe('listParams', () => {
	it('keys the filters only', () => {
		expect(filterKey(q('sort=-a&kind=STORNO&page=3&q=x'))).toBe('kind=STORNO&q=x')
		expect(filterKey(q('sort=a&page=2'))).toBe('')
	})

	it('reads the page', () => {
		for (const [v, want] of [['0', 1], ['-3', 1], ['x', 1], ['2.7', 2], ['', 1]] as const) {
			expect(pageOf(q(`page=${v}`))).toBe(want)
		}
		expect(pageOf(q(''))).toBe(1)
	})

	it('reads page 1 until a filter change has reset the URL', () => {
		expect(pageSince(q('kind=STORNO&page=3'), 'kind=STORNO')).toBe(3)
		expect(pageSince(q('kind=NORMAL&page=3'), 'kind=STORNO')).toBe(1)
		expect(pageSince(q('sort=-a&page=2'), '')).toBe(2)
	})

	it('builds the footer', () => {
		let went = 0
		const goTo = (n: number) => {
			went = n
		}
		const first = footer(1, 50, 50, 120, goTo)
		expect([first.from, first.to, first.onPrev]).toEqual([1, 50, undefined])
		first.onNext?.()
		expect(went).toBe(2)

		const mid = footer(2, 50, 50, 120, goTo)
		expect([mid.from, mid.to]).toEqual([51, 100])
		mid.onPrev?.()
		expect(went).toBe(1)
		mid.onNext?.()
		expect(went).toBe(3)

		const last = footer(3, 50, 20, 120, goTo)
		expect([last.from, last.to, last.onNext]).toEqual([101, 120, undefined])

		const unknown = footer(2, 50, 50, undefined, goTo)
		expect(unknown.onNext).toBeUndefined()
		expect(unknown.total).toBeUndefined()
		expect(footer(1, 50, 0, 0, goTo).from).toBe(0)
	})
})

// vim: ts=4
