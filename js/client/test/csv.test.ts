// SPDX-License-Identifier: MPL-2.0
import { describe, expect, it } from 'vitest'

import { CsvQuoteError, detectEncoding, parseCsv } from '../src/csv'

describe('parseCsv', () => {
	it.each([
		['comma, trailing newline', 'a,b\n1,2\n', [['a', 'b'], ['1', '2']]],
		['semicolon, CRLF', 'a;b\r\n1;2', [['a', 'b'], ['1', '2']]],
		['tab', 'a\tb\n1\t2', [['a', 'b'], ['1', '2']]],
		['semicolon wins over a later comma', 'név;cím\nKft, Bt;x', [['név', 'cím'], ['Kft, Bt', 'x']]],
		['a quoted delimiter does not count', '"a;b",c\n1,2', [['a;b', 'c'], ['1', '2']]],
		['BOM stripped', '﻿name,x\n1,2', [['name', 'x'], ['1', '2']]],
		[
			'quotes, escaped quotes, newline in quotes',
			'a,b\n"x ""y"", z","line\nbreak"',
			[['a', 'b'], ['x "y", z', 'line\nbreak']]
		],
		['blank line dropped, empty trailing cell kept', 'a,b\n\n1,\n', [['a', 'b'], ['1', '']]],
		['rows of bare delimiters dropped', 'név;cím\n;;;\n ; ;\n1;2', [['név', 'cím'], ['1', '2']]],
		['no delimiter at all', 'single\nrow', [['single'], ['row']]],
		[
			'a mid-cell quote is literal and keeps the following rows',
			'név;méret\nMonitor 27";x\nEgér;y\n',
			[['név', 'méret'], ['Monitor 27"', 'x'], ['Egér', 'y']]
		],
		['a mid-cell quote does not hide the delimiter', 'Monitor 27";x\n1;2', [['Monitor 27"', 'x'], ['1', '2']]],
		['an escaped quote keeps detection inside the quotes', '"a""b;c",d\n1,2', [['a"b;c', 'd'], ['1', '2']]]
	])('%s', (_what, text, want) => expect(parseCsv(text)).toEqual(want))

	it('has no rows for empty text', () => expect(parseCsv('')).toEqual([]))

	it('records each row\'s starting source line', () => {
		const lines: number[] = []
		parseCsv('a,b\n\n1,"x\ny"\r\n2,z', lines)
		expect(lines).toEqual([1, 3, 5])
	})

	it('throws on an unterminated quote with its row\'s line', () => {
		let open = 0
		try {
			parseCsv('a,b\n1,2\n"x,3\n4,5')
		} catch (e) {
			if (e instanceof CsvQuoteError) open = e.line
		}
		expect(open).toBe(3)
	})
})

describe('detectEncoding', () => {
	it('tells UTF-8 from windows-1250', () => {
		expect(detectEncoding(new TextEncoder().encode('tűrő').buffer as ArrayBuffer)).toBe('utf-8')
		expect(detectEncoding(new Uint8Array([0x74, 0xf5, 0x6b]).buffer as ArrayBuffer)).toBe(
			'windows-1250'
		)
	})
})

// vim: ts=4
