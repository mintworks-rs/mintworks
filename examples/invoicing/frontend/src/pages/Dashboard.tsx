import { useState } from 'react'
import { Link } from 'react-router-dom'

import type { MonthBucket } from '@mintworks/client'
import { localDate, summaryTotals } from '@mintworks/client'
import { useSummary } from '~/api/hooks'
import { TaxLimits } from '~/components/TaxLimits'
import { Button, ErrorBanner, MoneyText, Skeleton, buttonClass } from '~/components/ui'
import { useT } from '~/i18n'

const MONTHS = 12

/** The last `MONTHS` `'YYYY-MM'` keys, newest last, ending at the current Budapest month —
 *  the server's calendar, whatever the browser's zone. */
function recentMonths(): string[] {
	const [y, m] = localDate().split('-').map(Number)
	const out: string[] = []
	for (let i = MONTHS - 1; i >= 0; i--) {
		const d = new Date(Date.UTC(y, m - 1 - i, 1))
		out.push(d.toISOString().slice(0, 7))
	}
	return out
}

export function Dashboard() {
	const { t, tn, err } = useT()
	const summary = useSummary(MONTHS)
	const months = recentMonths()
	const rows = summary.data ? summaryTotals(summary.data) : []
	const [currency, setCurrency] = useState<string | null>(null)
	const charted = currency ?? rows[0]?.currency ?? ''

	return (
		<div className="space-y-6">
			<div className="flex flex-wrap items-start justify-between gap-4">
				<div>
					<h1 className="text-lg font-semibold text-fg">{t('dashboard.title')}</h1>
					<p className="mt-1 max-w-2xl text-sm text-fg-muted">{t('dashboard.intro')}</p>
				</div>
				<Link to="/invoices/new" className={buttonClass()}>
					{t('invoices.new')}
				</Link>
			</div>

			{summary.isPending ? (
				<Skeleton className="h-32 w-full" />
			) : summary.error ? (
				<ErrorBanner message={err(summary.error)} />
			) : rows.length === 0 ? (
				<p className="rounded-xl border border-dashed border-line-strong p-8 text-center text-sm text-fg-muted">
					{t('dashboard.empty')}
				</p>
			) : (
				<>
					{/* One group per currency, never one summed number: no rate applies to a
					    mixed sum, and the server converts nothing. */}
					{rows.map((r) => (
						<section key={r.currency} className="space-y-3">
							<h2 className="text-sm font-semibold text-fg">{r.currency}</h2>
							<div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
								<Tile
									label={t('dashboard.outstanding')}
									amount={r.outstanding}
									currency={r.currency}
								/>
								<Tile
									label={t('dashboard.overdue')}
									amount={r.overdue}
									currency={r.currency}
									note={tn('dashboard.invoiceCount', r.overdueCount)}
									tone={r.overdueCount > 0 ? 'danger' : undefined}
								/>
								<Tile
									label={t('dashboard.drafts')}
									amount={r.drafts}
									currency={r.currency}
									note={tn('dashboard.invoiceCount', r.draftCount)}
								/>
								<Tile
									label={t('dashboard.paidThisMonth')}
									amount={r.paidThisMonth}
									currency={r.currency}
								/>
							</div>
						</section>
					))}

					{rows.length > 1 && (
						<div className="flex flex-wrap gap-1">
							{rows.map((r) => (
								<Button
									key={r.currency}
									variant="secondary"
									className={
										r.currency === charted ? 'border-accent text-accent' : ''
									}
									aria-pressed={r.currency === charted}
									onClick={() => setCurrency(r.currency)}
								>
									{r.currency}
								</Button>
							))}
						</div>
					)}

					{/* auto-fit, not a breakpoint: when `TaxLimits` renders nothing the chart
					    takes the whole row instead of half of it. */}
					<div className="grid gap-6 [grid-template-columns:repeat(auto-fit,minmax(min(100%,30rem),1fr))]">
						<TaxLimits />
						<Chart
							currency={charted}
							months={months}
							buckets={(summary.data?.months ?? []).filter(
								(b) => b.currency === charted
							)}
						/>
					</div>

					<p className="text-xs text-fg-muted">{t('dashboard.footnote')}</p>
				</>
			)}
		</div>
	)
}

function Tile({
	label,
	amount,
	currency,
	note,
	tone
}: {
	label: string
	amount: string
	currency: string
	note?: string
	tone?: 'danger'
}) {
	return (
		<div className="rounded-xl border border-line bg-surface-raised p-4">
			<p className="text-sm text-fg-muted">{label}</p>
			<p
				className={`mt-1 text-xl font-semibold ${tone === 'danger' ? 'text-danger' : 'text-fg'}`}
			>
				<MoneyText value={{ amount, currency }} />
			</p>
			{note && <p className="mt-1 text-xs text-fg-muted">{note}</p>}
		</div>
	)
}

const W = 720
const H = 180
const PAD = 24

/**
 * Twelve months of issued gross with paid overlaid, drawn by hand: a charting dependency for
 * one chart is a build-size and upgrade cost nobody asked for.
 *
 * The bar heights are the only place an amount becomes a JavaScript number — a pixel is not
 * money, and nothing computed here is ever displayed. Every figure the reader sees comes back
 * out of `money()`, and the same numbers are in the visually-hidden table below, which is what
 * a screen reader gets instead of the drawing.
 */
function Chart({
	currency,
	months,
	buckets
}: {
	currency: string
	months: string[]
	buckets: MonthBucket[]
}) {
	const { t, money } = useT()
	const [active, setActive] = useState<number | null>(null)
	const by = new Map(buckets.map((b) => [b.month, b]))
	const bars = months.map((m) => {
		const b = by.get(m)
		return {
			month: m,
			gross: b?.gross ?? { amount: '0', currency },
			paid: b?.paid ?? { amount: '0', currency }
		}
	})
	const peak = Math.max(...bars.map((b) => Number(b.gross.amount)), 1)
	const slot = (W - PAD * 2) / MONTHS
	const width = slot * 0.6
	const height = (amount: string) => Math.max(0, (Number(amount) / peak) * (H - PAD * 2))
	const shown = active === null ? null : bars[active]

	return (
		<section className="rounded-xl border border-line bg-surface-raised p-4">
			<h2 className="text-sm font-semibold text-fg">{t('dashboard.chart', { currency })}</h2>

			<svg viewBox={`0 0 ${W} ${H}`} className="mt-3 w-full" role="img">
				<title>{t('dashboard.chart', { currency })}</title>
				<desc>{t('dashboard.chartDesc')}</desc>
				{bars.map((b, i) => {
					const x = PAD + i * slot + (slot - width) / 2
					const g = height(b.gross.amount)
					const p = height(b.paid.amount)
					return (
						// Focusable, so the figures are reachable without a pointer; the <title>
						// serves the pointer and the paragraph below serves both.
						// biome-ignore lint/a11y/noStaticElementInteractions: an SVG <g> takes no role that leaves it focusable.
						<g
							key={b.month}
							tabIndex={0}
							onFocus={() => setActive(i)}
							onBlur={() => setActive(null)}
							onMouseEnter={() => setActive(i)}
							onMouseLeave={() => setActive(null)}
						>
							<title>{`${b.month} · ${money(b.gross)} · ${money(b.paid)}`}</title>
							<rect
								x={x}
								y={H - PAD - g}
								width={width}
								height={g}
								className="fill-accent opacity-40"
							/>
							<rect
								x={x}
								y={H - PAD - p}
								width={width}
								height={p}
								className="fill-positive"
							/>
							<text
								x={x + width / 2}
								y={H - PAD + 17}
								textAnchor="middle"
								className="fill-fg-muted text-[13px]"
							>
								{b.month.slice(5)}
							</text>
						</g>
					)
				})}
				<line
					x1={PAD}
					y1={H - PAD}
					x2={W - PAD}
					y2={H - PAD}
					className="stroke-line"
					strokeWidth="1"
				/>
			</svg>

			<p role="status" className="mt-2 min-h-5 text-sm text-fg-muted">
				{shown &&
					`${shown.month} · ${t('invoices.gross')} ${money(shown.gross)} · ${t('dashboard.paid')} ${money(shown.paid)}`}
			</p>

			<table className="sr-only">
				<caption>{t('dashboard.chart', { currency })}</caption>
				<thead>
					<tr>
						<th scope="col">{t('invoices.date')}</th>
						<th scope="col">{t('invoices.gross')}</th>
						<th scope="col">{t('dashboard.paid')}</th>
					</tr>
				</thead>
				<tbody>
					{bars.map((b) => (
						<tr key={b.month}>
							<th scope="row">{b.month}</th>
							<td>{money(b.gross)}</td>
							<td>{money(b.paid)}</td>
						</tr>
					))}
				</tbody>
			</table>
		</section>
	)
}

// vim: ts=4
