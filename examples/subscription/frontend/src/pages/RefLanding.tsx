import { useQuery } from '@tanstack/react-query'
import { Link, useParams } from 'react-router-dom'

import { errMsg, previewRef } from '@mintworks/client'
import { AuthCard, ErrorBanner, PageSpinner } from '~/components/ui'

/** `/r/:code` — what an invitation or referral link lands on before sign-up. */
export function RefLanding() {
	const code = useParams().code ?? ''
	const ref = useQuery({
		queryKey: ['ref', code],
		queryFn: ({ signal }) => previewRef(code, signal),
		retry: false
	})

	if (ref.isPending) return <PageSpinner />
	if (ref.error || !ref.data.valid) {
		return (
			<AuthCard title="This link no longer works">
				<ErrorBanner message={ref.error ? errMsg(ref.error) : null} />
				<Link to="/register" className="text-sm text-brand-700 hover:underline">
					Create an account anyway
				</Link>
			</AuthCard>
		)
	}

	return (
		<AuthCard
			title={ref.data.orgName ? `Join ${ref.data.orgName}` : 'You have been invited'}
			subtitle={
				ref.data.type === 'signup' ? 'You both get a month of Pro after the first payment.' : undefined
			}
		>
			<div className="flex flex-col gap-3 text-sm">
				<Link
					to={`/register?ref=${encodeURIComponent(code)}`}
					className="inline-flex min-h-[44px] items-center justify-center rounded-md bg-brand-600 px-4 font-medium text-white hover:bg-brand-700"
				>
					Create an account
				</Link>
				<Link to="/login" className="text-brand-700 hover:underline">
					I already have an account
				</Link>
			</div>
		</AuthCard>
	)
}

// vim: ts=4
