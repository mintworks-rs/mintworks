import { localDate } from '@mintworks/client'

/** No locale switcher — one constant, and the whole app formats through it. */
export const LOCALE = 'hu-HU'

/** ISO-8601 from the wire → the date alone. */
export function date(iso: string | null | undefined): string {
	return iso ? localDate(new Date(iso)) : '—'
}

// vim: ts=4
