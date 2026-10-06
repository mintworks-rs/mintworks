// SPDX-License-Identifier: MIT-0
import { useUsage } from '~/api/hooks'

/** Micro-EUR → "€1.23", integer math only; a non-zero spend under a cent shows as "<€0.01". */
export function euros(micro: number): string {
	if (micro > 0 && micro < 10_000) return '<€0.01'
	const cents = (micro - (micro % 10_000)) / 10_000
	return `€${(cents - (cents % 100)) / 100}.${String(cents % 100).padStart(2, '0')}`
}

export function Usage() {
	const { data } = useUsage()
	if (!data) return null
	return (
		<span className="tnum text-xs text-fg-muted">
			spent {euros(data.spent)}
			{data.budget !== null && ` of ${euros(data.budget)}`}
		</span>
	)
}

// vim: ts=4
