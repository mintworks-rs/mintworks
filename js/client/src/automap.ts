// SPDX-License-Identifier: MPL-2.0
// Header → field auto-mapping for a CSV import, before the rows go to a `/bulk` route.

export interface MapField {
	key: string
	label: string
	required: boolean
}

/** Field key → index into `headers`, or null when the field is not imported. */
export type ColumnMap = Record<string, number | null>

/** Lowercase, accents and everything but letters and digits removed: "Adószám" → "adoszam". */
const norm = (s: string) =>
	s
		.normalize('NFD')
		.replace(/\p{M}/gu, '')
		.toLowerCase()
		.replace(/[^a-z0-9]/g, '')

/** Maps each field to the first unclaimed header equal (after `norm`) to its key, its label or
 *  one of its `synonyms[key]`. */
export function autoMap(
	headers: string[],
	fields: MapField[],
	synonyms: Record<string, string[]>
): ColumnMap {
	const hs = headers.map(norm)
	const taken = new Set<number>()
	const out: ColumnMap = {}
	for (const f of fields) {
		const names = new Set([f.key, f.label, ...(synonyms[f.key] ?? [])].map(norm))
		const i = hs.findIndex((h, j) => h !== '' && !taken.has(j) && names.has(h))
		out[f.key] = i < 0 ? null : i
		if (i >= 0) taken.add(i)
	}
	return out
}

// vim: ts=4
