import type { ReactNode } from 'react'

import { EmptyState } from '~/components/ui'

export interface Column<T> {
	key: string
	header: string
	cell: (row: T) => ReactNode
	/** Numbers right-align and get tabular figures; everything else is left. */
	numeric?: boolean
}

export function DataTable<T>({
	columns,
	rows,
	rowKey,
	caption,
	empty,
	onRowClick
}: {
	columns: Column<T>[]
	rows: T[]
	rowKey: (row: T) => string
	/** The table's accessible name. Visually hidden; a table without one is unnamed to a
	 *  screen reader. */
	caption: string
	empty: { title: string; description?: string; action?: ReactNode }
	onRowClick?: (row: T) => void
}) {
	if (rows.length === 0) return <EmptyState {...empty} />

	return (
		<div className="overflow-x-auto rounded-xl border border-line bg-surface-raised">
			<table className="w-full text-sm">
				<caption className="sr-only">{caption}</caption>
				<thead className="border-b border-line bg-surface-sunken text-left text-fg-muted">
					<tr>
						{columns.map((c) => (
							<th
								key={c.key}
								scope="col"
								className={`px-4 py-3 font-medium ${c.numeric ? 'text-right' : ''}`}
							>
								{c.header}
							</th>
						))}
					</tr>
				</thead>
				<tbody className="divide-y divide-line">
					{rows.map((row) => (
						<tr
							key={rowKey(row)}
							onClick={onRowClick && (() => onRowClick(row))}
							className={`h-10 ${onRowClick ? 'cursor-pointer hover:bg-surface-sunken' : ''}`}
						>
							{columns.map((c) => (
								<td
									key={c.key}
									className={`px-4 py-2.5 text-fg ${c.numeric ? 'text-right tnum' : ''}`}
								>
									{c.cell(row)}
								</td>
							))}
						</tr>
					))}
				</tbody>
			</table>
		</div>
	)
}

// vim: ts=4
