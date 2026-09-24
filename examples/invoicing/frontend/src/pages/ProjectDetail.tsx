import { Link, useParams } from 'react-router-dom'

import type { InvoiceView } from '@saas-framework/client'
import { useParties } from '@saas-framework/client'
import { useProject, useProjectInvoices } from '~/api/hooks'
import type { Column } from '~/components/DataTable'
import { DataTable } from '~/components/DataTable'
import { ErrorBanner, MoneyText, PageSpinner, SkeletonTable, buttonClass } from '~/components/ui'
import { useT } from '~/i18n'
import { InvoiceStatusChip } from '~/pages/Invoices'
import { ProjectStatusChip } from '~/pages/Projects'

export function ProjectDetail() {
	const { uid = '' } = useParams()
	const { t, err, date } = useT()
	const q = useProject(uid)
	const invoices = useProjectInvoices(uid)
	const parties = useParties()

	if (q.isPending) return <PageSpinner />
	if (q.error) return <ErrorBanner message={err(q.error)} />
	if (!q.data) return null

	const p = q.data.body
	const party = parties.data?.items.find((x) => x.uid === p.partyUid)

	const columns: Column<InvoiceView>[] = [
		{
			key: 'number',
			header: t('invoices.number'),
			cell: (i) => (
				<Link to={`/invoices/${i.uid}`} className="text-accent underline">
					{i.number ?? t('invoices.status.DRAFT')}
				</Link>
			)
		},
		{
			key: 'status',
			header: t('common.status'),
			cell: (i) => <InvoiceStatusChip status={i.status} />
		},
		{ key: 'issued', header: t('detail.issuedAt'), cell: (i) => date(i.issuedAt) },
		{
			key: 'gross',
			header: t('invoices.gross'),
			cell: (i) => <MoneyText value={i.gross} />,
			numeric: true
		}
	]

	return (
		<div className="space-y-6">
			<div className="flex flex-wrap items-center gap-3">
				<Link to="/projects" className="text-sm text-accent underline">
					{t('projects.title')}
				</Link>
				<h1 className="text-lg font-semibold text-fg">{p.name}</h1>
				<ProjectStatusChip status={p.status} />
			</div>

			<dl className="grid gap-x-6 gap-y-2 rounded-xl border border-line bg-surface-raised p-4 text-sm sm:grid-cols-3">
				<div>
					<dt className="text-fg-muted">{t('projects.party')}</dt>
					<dd className="text-fg">{party?.name ?? t('common.none')}</dd>
				</div>
				<div className="sm:col-span-2">
					<dt className="text-fg-muted">{t('projects.notes')}</dt>
					<dd className="text-fg">
						{p.notes === undefined || p.notes === '' ? t('common.none') : p.notes}
					</dd>
				</div>
			</dl>

			<div>
				<h2 className="mb-2 text-base font-semibold text-fg">{t('projects.invoices')}</h2>
				{invoices.isPending ? (
					<SkeletonTable />
				) : invoices.error ? (
					<ErrorBanner message={err(invoices.error)} />
				) : (
					<>
						<DataTable
							columns={columns}
							rows={invoices.data?.items ?? []}
							rowKey={(i) => i.uid}
							caption={t('projects.invoices')}
							empty={{
								title: t('projects.noInvoices'),
								description: t('projects.noInvoices.body'),
								action: (
									<Link to="/invoices" className={buttonClass()}>
										{t('invoices.title')}
									</Link>
								)
							}}
						/>
						{/* Said rather than paged: the endpoint is 1 + N round trips by design,
						    so a "load more" would multiply it. */}
						{invoices.data?.nextCursor !== null && (
							<p className="mt-2 text-sm text-fg-muted">{t('projects.truncated')}</p>
						)}
					</>
				)}
			</div>
		</div>
	)
}

// vim: ts=4
