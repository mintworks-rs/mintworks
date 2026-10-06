// SPDX-License-Identifier: MPL-2.0
import { afterEach, describe, expect, it, vi } from 'vitest'

import { checkout, quote, subscriptionAction } from '../src/commerce'
import { ERRORS_EN, ERRORS_HU, errText } from '../src/errors'
import { ServerError } from '../src/http'

const NEW_CODES = [
	'E-CORE-REF-INVALID',
	'E-CORE-REF-TYPE',
	'E-CORE-SLUG-TAKEN',
	'E-CORE-SLUG-INVALID',
	'E-AUTH-INVITE-EMAIL',
	'E-AUTH-INVITE-EXPIRED',
	'E-AUTH-INVITE-REQUIRED',
	'E-ENT-UNKNOWN',
	'E-ENT-DENIED',
	'E-ENT-EXHAUSTED',
	'E-PLAN-NO-PRICE',
	'E-PLAN-QUOTE-EXPIRED',
	'E-PLAN-QUOTE-STALE',
	'E-PLAN-COUPON-INVALID',
	'E-PLAN-FAMILY-MISMATCH',
	'E-PLAN-CHANGE-INVALID',
	'E-PLAN-FAMILY-LIVE',
	'E-PLAN-TRIAL-USED'
]

function reply(status: number, body?: unknown) {
	const fetch = vi.fn(
		async () =>
			new Response(body === undefined ? null : JSON.stringify(body), {
				status,
				headers: { 'Content-Type': 'application/json' }
			})
	)
	vi.stubGlobal('fetch', fetch)
	return fetch
}

afterEach(() => vi.unstubAllGlobals())

describe('the commerce codes', () => {
	it('are in both dictionaries', () => {
		for (const code of NEW_CODES) {
			expect(ERRORS_EN[code], code).toBeTruthy()
			expect(ERRORS_HU[code], code).toBeTruthy()
		}
	})
})

describe('a tier change', () => {
	it('quotes with the subscription in the body', async () => {
		const fetch = reply(200, { effective: 'period_end', quoteToken: 't' })
		const q = await quote({ offer: 'basic', subscription: 'sub_1' })
		expect(q.effective).toBe('period_end')
		const [url, init] = fetch.mock.calls[0] as unknown as [string, RequestInit]
		expect(url).toBe('/api/plans/quote')
		expect(JSON.parse(init.body as string)).toEqual({ offer: 'basic', subscription: 'sub_1' })
	})

	it('surfaces a stale quote as a branchable ServerError', async () => {
		reply(409, { error: { errCode: 'E-PLAN-QUOTE-STALE', errStr: 'stale' } })
		const e = await checkout('t', 'CARD').catch((x: unknown) => x)
		expect(e).toBeInstanceOf(ServerError)
		expect((e as ServerError).httpStatus).toBe(409)
		expect(errText(e, ERRORS_EN)).toBe(ERRORS_EN['E-PLAN-QUOTE-STALE'])
	})

	it('drops a queued downgrade through cancel-change', async () => {
		const fetch = reply(200, { uid: 'sub_1' })
		await subscriptionAction('sub_1', 'cancel-change')
		const [url] = fetch.mock.calls[0] as unknown as [string]
		expect(url).toBe('/api/plans/subscriptions/sub_1/cancel-change')
	})
})

// vim: ts=4
