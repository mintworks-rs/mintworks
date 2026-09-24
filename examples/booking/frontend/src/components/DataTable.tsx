import type { ReactNode } from 'react'

import { EmptyState } from '~/components/ui'

export interface Column<T> {
	key: string
	header: string
	cell: (row: T) => ReactNode
	/** Numbers right-align; everything else is left. */
	numeric?: boolean
}

export function DataTable<T>({
	columns,
	rows,
	rowKey,
	caption,
	empty
}: {
	columns: Column<T>[]
	rows: T[]
	rowKey: (row: T) => string
	/** The table's accessible name. Visually hidden; a table without one is unnamed to a
	 *  screen reader. */
	caption: string
	empty: { title: string; description?: string; action?: ReactNode }
}) {
	if (rows.length === 0) return <EmptyState {...empty} />

	return (
		<div className="overflow-x-auto rounded-xl border border-slate-200 bg-white">
			<table className="w-full text-sm">
				<caption className="sr-only">{caption}</caption>
				<thead className="border-b border-slate-200 bg-slate-50 text-left text-slate-600">
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
				<tbody className="divide-y divide-slate-100">
					{rows.map((row) => (
						<tr key={rowKey(row)} className="hover:bg-slate-50">
							{columns.map((c) => (
								<td
									key={c.key}
									className={`px-4 py-3 text-slate-800 ${c.numeric ? 'text-right tabular-nums' : ''}`}
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
