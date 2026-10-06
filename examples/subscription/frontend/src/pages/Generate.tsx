// SPDX-License-Identifier: MIT-0
import { useQueryClient } from '@tanstack/react-query'
import * as React from 'react'
import { Link } from 'react-router-dom'

import { ServerError, api, errMsg, keys, useEntitlements } from '@mintworks/client'
import { Button, ErrorBanner } from '~/components/ui'

/** The gated feature: each run spends 10 `ai_credits`, and an empty meter answers 402. */
export function Generate() {
	const qc = useQueryClient()
	const ent = useEntitlements()
	const [busy, setBusy] = React.useState(false)
	const [error, setError] = React.useState<string | null>(null)
	const [upsell, setUpsell] = React.useState(false)

	async function run() {
		setBusy(true)
		setError(null)
		try {
			await api.post('/api/demo/generate', { idem: crypto.randomUUID() })
			setUpsell(false)
		} catch (e) {
			if (e instanceof ServerError && e.errCode === 'E-ENT-EXHAUSTED') setUpsell(true)
			else setError(errMsg(e))
		} finally {
			setBusy(false)
			void qc.invalidateQueries({ queryKey: keys.entitlements })
		}
	}

	return (
		<div className="flex max-w-lg flex-col gap-4">
			<h1 className="text-xl font-semibold text-slate-900">Generate</h1>
			<p className="text-sm text-slate-600">
				Each run costs 10 AI credits. Balance:{' '}
				{ent.data?.meters.ai_credits?.balance ?? 0}
			</p>
			<ErrorBanner message={error} />
			{upsell && (
				<div role="alert" className="rounded-xl border border-amber-300 bg-amber-50 p-4 text-sm">
					<p className="font-medium text-amber-900">You are out of AI credits.</p>
					<p className="mt-1 text-amber-800">
						Upgrade your plan or buy a credit pack on{' '}
						<Link to="/" className="font-medium underline">
							Pricing
						</Link>
						.
					</p>
				</div>
			)}
			<Button onClick={() => void run()} loading={busy}>
				Generate
			</Button>
		</div>
	)
}

// vim: ts=4
