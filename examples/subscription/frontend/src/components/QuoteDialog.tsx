// SPDX-License-Identifier: MIT-0
import { useQuery } from '@tanstack/react-query'
import * as React from 'react'

import type { Checkout, PayMethod, QuoteReq } from '@mintworks/client'
import { ServerError, api, errMsg, formatMoney, quote, useCheckout } from '@mintworks/client'
import { Modal } from '~/components/Modal'
import { useToast } from '~/components/Toast'
import { Button, ErrorBanner, PageSpinner, Select } from '~/components/ui'
import { LOCALE, date } from '~/lib/format'

/** The card gateway `mintworks` registers in a deployment; the Rune suite pays with `fake`. */
const CARD_PROVIDER = 'barion'

/**
 * Quote → confirm → checkout, for a purchase and for a tier or seat change alike: the price the
 * user confirms is the server's quote, and checkout commits exactly that token.
 */
export function QuoteDialog({
	req,
	title,
	onClose
}: {
	req: QuoteReq
	title: string
	onClose: () => void
}) {
	const toast = useToast()
	const commit = useCheckout()
	const [payMethod, setPayMethod] = React.useState<PayMethod>('CARD')
	const [error, setError] = React.useState<string | null>(null)
	const q = useQuery({
		queryKey: ['quote', req],
		queryFn: () => quote(req),
		gcTime: 0,
		retry: false
	})

	async function confirm() {
		if (!q.data) return
		setError(null)
		try {
			done(await commit.mutateAsync({ quoteToken: q.data.quoteToken, payMethod }))
		} catch (e) {
			// The subscription moved or the token aged out: show the new price, never commit blind.
			if (
				e instanceof ServerError &&
				(e.errCode === 'E-PLAN-QUOTE-STALE' || e.errCode === 'E-PLAN-QUOTE-EXPIRED')
			) {
				void q.refetch()
				setError('The price changed; here is the new quote.')
			} else setError(errMsg(e))
		}
	}

	async function done(c: Checkout) {
		if (c.next === 'pay' && c.invoiceUid) {
			const r = await api.post<{ redirectUrl: string | null }>(
				`/api/invoices/${encodeURIComponent(c.invoiceUid)}/pay`,
				{ provider: CARD_PROVIDER, returnUrl: `${location.origin}/account` }
			)
			if (r.redirectUrl) {
				window.location.assign(r.redirectUrl)
				return
			}
		}
		toast.success(
			{
				pay: 'Payment started.',
				issued: 'Invoice issued; it activates once the transfer arrives.',
				trialing: 'Your trial has started.',
				scheduled: 'The change takes effect at the end of the period.'
			}[c.next]
		)
		onClose()
	}

	const data = q.data
	return (
		<Modal open onClose={onClose} title={title}>
			{q.isPending && <PageSpinner />}
			<ErrorBanner message={q.error ? errMsg(q.error) : error} />
			{data && (
				<div className="flex flex-col gap-4 text-sm text-slate-700">
					<ul className="divide-y divide-slate-200">
						{data.lines.map((l) => (
							<li key={l.description} className="flex justify-between gap-4 py-2">
								<span>{l.description}</span>
								<span>{formatMoney(l.gross, LOCALE)}</span>
							</li>
						))}
					</ul>
					<p className="flex justify-between font-semibold">
						<span>Total</span>
						<span>{formatMoney(data.gross, LOCALE)}</span>
					</p>
					<p className="text-slate-500">
						{data.effective === 'period_end'
							? `Nothing to pay now; the change applies on ${date(data.periodEnd)}.`
							: data.periodEnd
								? `Covers the period until ${date(data.periodEnd)}.`
								: null}
					</p>
					{data.effective === 'now' && (
						<Select
							aria-label="Payment method"
							value={payMethod}
							onChange={(e) => setPayMethod(e.target.value as PayMethod)}
						>
							<option value="CARD">Card</option>
							<option value="TRANSFER">Bank transfer</option>
						</Select>
					)}
					<div className="flex justify-end gap-2">
						<Button variant="secondary" onClick={onClose}>
							Cancel
						</Button>
						<Button onClick={() => void confirm()} loading={commit.isPending}>
							Confirm
						</Button>
					</div>
				</div>
			)}
		</Modal>
	)
}

// vim: ts=4
