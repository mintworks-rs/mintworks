// The SDK owns the prose for `E-*` codes, because the codes are the framework's. Everything
// else an app shows is the app's own, and stays out of here.

import { ServerError } from '../http'

export type ErrorDict = Record<string, string>

export { ERRORS_EN } from './en'
export { ERRORS_HU } from './hu'

/**
 * Display text for anything thrown by the transport.
 *
 * An unrecognised code falls back to the server's `errStr`, never to a blank string: a new
 * code shipped by the backend must degrade to the English sentence the server already sent,
 * not to an empty banner that looks like the request succeeded.
 */
export function errText(e: unknown, dict: ErrorDict): string {
	if (e instanceof ServerError) return dict[e.errCode] || e.errStr || e.errCode
	if (e instanceof Error) return e.message
	return dict['E-CORE-INTERNAL'] ?? 'Unexpected error'
}

/**
 * `ServerError.fields` is keyed by field name and valued by an **error code**
 * (`E-CORE-FORMAT`, `E-CORE-RANGE`), not by prose — this is what maps it to text for the form.
 */
export function fieldErrors(e: unknown, dict: ErrorDict): Record<string, string> {
	if (!(e instanceof ServerError) || !e.fields) return {}
	const out: Record<string, string> = {}
	for (const [field, code] of Object.entries(e.fields)) {
		out[field] = dict[code] || e.errStr || code
	}
	return out
}

// vim: ts=4
