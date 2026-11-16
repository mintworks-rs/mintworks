// SPDX-License-Identifier: MPL-2.0
// RFC 4180 CSV, as Excel and Hungarian invoicing tools export it. Tests: `test/csv.test.ts`.

const DELIMITERS = [';', ',', '\t'] as const

/** An opening quote that never closes; `line` is the 1-based source line its row starts on. */
export class CsvQuoteError extends Error {
	line: number
	constructor(line: number) {
		super(`unclosed quote starting on line ${line}`)
		this.line = line
	}
}

/** `utf-8` when the bytes are valid UTF-8, else `windows-1250`, what Hungarian Excel saves. */
export function detectEncoding(buffer: ArrayBuffer): 'utf-8' | 'windows-1250' {
	try {
		new TextDecoder('utf-8', { fatal: true }).decode(buffer)
		return 'utf-8'
	} catch {
		return 'windows-1250'
	}
}

/** The delimiter that occurs most often outside quotes on the first line; `,` on a tie or none.
 *  A quote opens only at a cell's start, as in `parseCsv`. */
function detectDelimiter(text: string): string {
	const counts: Record<string, number> = { ';': 0, ',': 0, '\t': 0 }
	let quoted = false
	for (let i = 0; i < text.length; i++) {
		const c = text[i]
		if (quoted) {
			if (c !== '"') continue
			if (text[i + 1] === '"') i++
			else quoted = false
		} else if (c === '"' && (i === 0 || text[i - 1] in counts)) quoted = true
		else if (c === '\n' || c === '\r') break
		else if (c in counts) counts[c]++
	}
	return DELIMITERS.reduce((best, d) => (counts[d] > counts[best] ? d : best), ',')
}

/** Rows of cells. Strips a UTF-8 BOM, accepts CRLF/LF/CR, drops blank lines. Cells are not trimmed.
 *  A `"` opens a quote only at a cell's start; elsewhere it is literal (`Monitor 27"`). Throws
 *  `CsvQuoteError` on a quote left open. `lines`, when given, gets each kept row's 1-based
 *  starting source line. */
export function parseCsv(text: string, lines?: number[]): string[][] {
	if (text.charCodeAt(0) === 0xfeff) text = text.slice(1)
	const delim = detectDelimiter(text)
	const rows: string[][] = []
	let row: string[] = []
	let cell = ''
	let quoted = false
	let line = 1
	let start = 1
	const endRow = () => {
		row.push(cell)
		if (row.some((c) => c.trim() !== '')) {
			rows.push(row)
			lines?.push(start)
		}
		row = []
		cell = ''
	}
	for (let i = 0; i < text.length; i++) {
		const c = text[i]
		if (quoted) {
			if (c === '\n' || (c === '\r' && text[i + 1] !== '\n')) line++
			if (c !== '"') cell += c
			else if (text[i + 1] === '"') {
				cell += '"'
				i++
			} else quoted = false
		} else if (c === '"' && cell === '') quoted = true
		else if (c === delim) {
			row.push(cell)
			cell = ''
		} else if (c === '\r' || c === '\n') {
			if (c === '\r' && text[i + 1] === '\n') i++
			endRow()
			line++
			start = line
		} else cell += c
	}
	if (quoted) throw new CsvQuoteError(start)
	endRow()
	return rows
}

// vim: ts=4
