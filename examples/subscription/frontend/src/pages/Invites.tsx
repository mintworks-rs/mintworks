import { useMutation, useQueryClient } from '@tanstack/react-query'

import type { Ref } from '@mintworks/client'
import {
	createSignupRef,
	errMsg,
	keys,
	useCreateRef,
	useRefs,
	useRevokeRef
} from '@mintworks/client'
import { useToast } from '~/components/Toast'
import { Badge, Button, EmptyState, ErrorBanner, PageSpinner } from '~/components/ui'
import { date } from '~/lib/format'

const link = (r: Ref) => `${location.origin}/r/${encodeURIComponent(r.code)}`

export function Invites() {
	const toast = useToast()
	const qc = useQueryClient()
	const refs = useRefs()
	const revoke = useRevokeRef()
	const affiliate = useCreateRef()
	// `signup` is mintworks-auth's type, minted through its own route rather than `POST /api/refs`.
	const signup = useMutation({
		mutationFn: () => createSignupRef({}),
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.refs })
	})
	const onError = (e: unknown) => toast.error(errMsg(e))

	if (refs.isPending) return <PageSpinner />
	if (refs.error) return <ErrorBanner message={errMsg(refs.error)} />

	return (
		<div className="flex flex-col gap-6">
			<h1 className="text-xl font-semibold text-slate-900">Invites</h1>
			<p className="text-sm text-slate-600">
				A signup link rewards you and the new account with a month of Pro after the first payment.
			</p>
			<div className="flex gap-2">
				<Button loading={signup.isPending} onClick={() => signup.mutate(undefined, { onError })}>
					New signup link
				</Button>
				<Button
					variant="secondary"
					loading={affiliate.isPending}
					onClick={() => affiliate.mutate({ type: 'affiliate' }, { onError })}
				>
					New affiliate link
				</Button>
			</div>

			{refs.data.items.length === 0 ? (
				<EmptyState title="No links yet" />
			) : (
				<ul className="divide-y divide-slate-200 rounded-xl border border-slate-200 bg-white">
					{refs.data.items.map((r) => (
						<li key={r.uid} className="flex flex-wrap items-center gap-3 px-4 py-3 text-sm">
							<Badge>{r.type}</Badge>
							<code className="flex-1 break-all text-slate-700">{link(r)}</code>
							<span className="text-slate-500">
								{r.usesLeft === null ? 'unlimited' : `${r.usesLeft} uses left`}
								{r.expiresAt && `, until ${date(r.expiresAt)}`}
							</span>
							{r.status === 'ACTIVE' ? (
								<Button
									variant="ghost"
									loading={revoke.isPending && revoke.variables === r.uid}
									onClick={() => revoke.mutate(r.uid, { onError })}
								>
									Revoke
								</Button>
							) : (
								<Badge tone="neutral">revoked</Badge>
							)}
						</li>
					))}
				</ul>
			)}
		</div>
	)
}

// vim: ts=4
