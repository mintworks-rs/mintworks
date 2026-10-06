// SPDX-License-Identifier: MIT-0
import { type ReactNode, useState } from 'react'
import { Link } from 'react-router-dom'

import { type TaxLimit, type TaxLimits as Data, useTaxLimits } from '~/api/hooks'
import { ErrorBanner, Skeleton } from '~/components/ui'
import { type Key, useT } from '~/i18n'

/**
 * The yearly revenue limits of the seller's tax regimes (AAM, KATA, átalányadó), one bullet row
 * each, and their running totals in one chart. Hidden when no limit is in force.
 *
 * Amounts arrive as whole forints. Percentages and the projection are integer arithmetic; a
 * JavaScript number becomes a float only as a pixel position.
 */
export function TaxLimits() {
	const { t, err } = useT()
	const q = useTaxLimits()
	if (q.isPending) return <Skeleton className="h-40 w-full" />
	if (q.error) return <ErrorBanner message={err(q.error)} />
	const d = q.data
	if (!d || d.limits.length === 0) return null
	const pace = paceOf(d)

	return (
		<section className="space-y-4 rounded-xl border border-line bg-surface-raised p-4">
			<div className="flex flex-wrap items-baseline justify-between gap-2">
				<h2 className="text-sm font-semibold text-fg">
					{t('tax.title', { year: d.year })}
				</h2>
				<ElapsedChip data={d} pct={pace.pct} />
			</div>
			<ul className="divide-y divide-line">
				{d.limits.map((l) => (
					<li key={l.kind} className="py-3.5 first:pt-0">
						{l.kind === 'ATALANY_TAXFREE' ? (
							<Milestone l={l} year={d.year} />
						) : (
							<Bullet l={l} pace={pace} year={d.year} />
						)}
					</li>
				))}
			</ul>
			<div className="border-t border-line pt-4">
				<CumulativeChart data={d} pace={pace} />
			</div>
			<details className="text-xs text-fg-muted">
				<summary>{t('tax.about')}</summary>
				<p className="mt-1">{t('tax.footnote')}</p>
			</details>
		</section>
	)
}

// ---------------------------------------------------------------- pace

interface Pace {
	/** Elapsed days of `year` — the whole year once it is over, none before it starts. */
	day: number
	days: number
	pct: number
	/** A projection needs a twelfth of the year behind it; January alone is noise. */
	projects: boolean
}

function isLeap(y: number): boolean {
	return (y % 4 === 0 && y % 100 !== 0) || y % 400 === 0
}

function dayOfYear(iso: string): number {
	const [y, m, day] = iso.split('-').map(Number) as [number, number, number]
	const before = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334][m - 1] ?? 0
	return before + (m > 2 && isLeap(y) ? 1 : 0) + day
}

function paceOf(d: Data): Pace {
	const days = isLeap(d.year) ? 366 : 365
	const thisYear = Number(d.today.slice(0, 4))
	const day = thisYear > d.year ? days : thisYear < d.year ? 0 : dayOfYear(d.today)
	return { day, days, pct: Math.floor((day * 100) / days), projects: day * 12 >= days }
}

/** `used` carried at today's pace to 31 December. */
function projected(used: number, p: Pace): number | null {
	return p.projects && p.day > 0 ? Math.floor((used * p.days) / p.day) : null
}

// ---------------------------------------------------------------- rows

function useHuf() {
	const { money } = useT()
	return (n: number) => money({ amount: String(n), currency: 'HUF' })
}

/** `20 M`, `38,7 M` — a label, not a figure: one decimal, truncated. */
function useMega() {
	const { locale } = useT()
	return (n: number) => {
		const tenths = Math.floor(n / 100_000)
		const rest = tenths % 10
		const whole = String(Math.floor(tenths / 10))
		return rest === 0 ? `${whole} M` : `${whole}${locale === 'hu' ? ',' : '.'}${rest} M`
	}
}

function useMonthName() {
	const { tag } = useT()
	const fmt = new Intl.DateTimeFormat(tag, { month: 'long' })
	return (ym: string) =>
		fmt.format(new Date(Number(ym.slice(0, 4)), Number(ym.slice(5, 7)) - 1, 1))
}

function ElapsedChip({ data, pct }: { data: Data; pct: number }) {
	const { t, tag } = useT()
	const [y, m, d] = data.today.split('-').map(Number) as [number, number, number]
	const today = new Intl.DateTimeFormat(tag, { month: 'short', day: 'numeric' }).format(
		new Date(y, m - 1, d)
	)
	return (
		<p className="rounded-full border border-line px-2.5 py-0.5 text-xs text-fg-muted">
			{t('tax.elapsed', { pct, today })}
		</p>
	)
}

const KIND_LABEL: Record<TaxLimit['kind'], Key> = {
	AAM: 'tax.kind.AAM',
	KATA: 'tax.kind.KATA',
	ATALANY: 'tax.kind.ATALANY',
	ATALANY_TAXFREE: 'tax.kind.ATALANY_TAXFREE'
}

const CONSEQUENCE: Record<'AAM' | 'KATA' | 'ATALANY', Key> = {
	AAM: 'tax.consequence.AAM',
	KATA: 'tax.consequence.KATA',
	ATALANY: 'tax.consequence.ATALANY'
}

function pctOf(used: number, limit: number): number {
	return limit > 0 ? Math.floor((used * 100) / limit) : 0
}

/** Filled = used, hatched = issued but unpaid, lighter = projected to 31 Dec, tick = the share
 *  of the year elapsed. */
function Bar({
	used,
	pending,
	limit,
	projection,
	tick,
	tone,
	basis
}: {
	used: number
	pending: number
	limit: number
	projection: number | null
	tick: number | null
	tone: 'ok' | 'warning' | 'danger'
	basis: TaxLimit['basis']
}) {
	const scale = Math.max(limit, projection ?? 0, used + pending, 1)
	const w = (n: number) => `${Math.min(100, (n / scale) * 100)}%`
	const fill =
		tone === 'danger'
			? 'bg-danger'
			: tone === 'warning'
				? 'bg-warning'
				: basis === 'INVOICED'
					? 'bg-series-invoiced'
					: 'bg-series-received'
	return (
		<div aria-hidden="true" className="relative mt-2 h-3 rounded-full bg-surface-sunken">
			{projection !== null && projection > used && (
				<div
					className={`absolute inset-y-0 left-0 rounded-full opacity-30 ${fill}`}
					style={{ width: w(projection) }}
				/>
			)}
			{pending > 0 && (
				<div
					className="fill-unpaid absolute inset-y-0 left-0 rounded-full"
					style={{ width: w(used + pending) }}
				/>
			)}
			<div
				className={`absolute inset-y-0 left-0 rounded-full ${fill}`}
				style={{ width: w(used) }}
			/>
			{/* The limit itself, when a projection past it stretches the scale. */}
			{scale > limit && (
				<div className="absolute -inset-y-1 w-0.5 bg-fg" style={{ left: w(limit) }} />
			)}
			{tick !== null && (
				<div
					className="absolute -inset-y-1 w-0.5 bg-fg-muted"
					style={{ left: `calc(${w((limit * tick) / 100)} - 1px)` }}
				/>
			)}
		</div>
	)
}

/** Name and the one figure to read at a glance; everything else sits under the bar. */
function RowHead({ title, aside, pct }: { title: ReactNode; aside?: ReactNode; pct: ReactNode }) {
	return (
		<div className="flex items-baseline justify-between gap-4">
			<h3 className="flex flex-wrap items-baseline gap-x-2 text-sm font-medium text-fg">
				{title}
				{aside && <span className="text-xs font-normal text-fg-muted">{aside}</span>}
			</h3>
			{pct}
		</div>
	)
}

/** `12,4 M / 20 M`, the exact forints on hover and for screen readers. */
function UsedOf({ used, limit }: { used: number; limit: number }) {
	const huf = useHuf()
	const mega = useMega()
	const exact = `${huf(used)} / ${huf(limit)}`
	return (
		<span className="tnum" title={exact}>
			<span aria-hidden="true">
				{mega(used)} / {mega(limit)}
			</span>
			<span className="sr-only">{exact}</span>
		</span>
	)
}

function Pending({ amount }: { amount: number }) {
	const { t } = useT()
	const mega = useMega()
	if (amount <= 0) return null
	return (
		<span className="flex items-center gap-1.5">
			<span aria-hidden="true" className="fill-unpaid inline-block h-2.5 w-3 rounded-sm" />
			{t('tax.pending', { amount: mega(amount) })}
		</span>
	)
}

const TONE_TEXT = { ok: 'text-fg', warning: 'text-warning', danger: 'text-danger' } as const

function Bullet({ l, pace, year }: { l: TaxLimit; pace: Pace; year: number }) {
	const { t, date } = useT()
	const mega = useMega()
	const monthName = useMonthName()
	const kind = l.kind as 'AAM' | 'KATA' | 'ATALANY'
	const since = l.since ? t('tax.prorated', { since: date(l.since) }) : undefined
	if (l.limit === null) {
		return (
			<>
				<RowHead title={t(KIND_LABEL[kind])} aside={since} pct={null} />
				<p className="mt-1 text-xs text-fg-muted">{t('tax.unpublished', { year })}</p>
			</>
		)
	}
	const projection = projected(l.used, pace)
	const status: 'ok' | 'warning' | 'danger' = l.crossedOn
		? 'danger'
		: (projection !== null && projection > l.limit) || l.used + l.pending > l.limit
			? 'warning'
			: 'ok'
	return (
		<>
			<RowHead
				title={t(KIND_LABEL[kind])}
				aside={since}
				pct={
					<span className={`tnum text-lg font-semibold ${TONE_TEXT[status]}`}>
						{pctOf(l.used, l.limit)} %
					</span>
				}
			/>
			<Bar
				used={l.used}
				pending={l.pending}
				limit={l.limit}
				projection={projection}
				tick={pace.pct}
				tone={status}
				basis={l.basis}
			/>
			<div className="mt-1.5 flex flex-wrap justify-between gap-x-4 gap-y-1 text-xs text-fg-muted">
				<span className="flex flex-wrap gap-x-3">
					<UsedOf used={l.used} limit={l.limit} />
					<Pending amount={l.pending} />
				</span>
				{status === 'danger' ? (
					<span className="font-medium text-danger">
						✕ {t('tax.exceeded', { month: monthName(l.crossedOn ?? '') })}
					</span>
				) : status === 'warning' ? (
					<span className="font-medium text-warning">
						▲{' '}
						{projection !== null && projection > l.limit
							? t('tax.projection', { amount: mega(projection) })
							: t('tax.overOncePaid')}
					</span>
				) : (
					<span className="text-positive">✓ {t('tax.onPace')}</span>
				)}
			</div>
			{status !== 'ok' && (
				<p className={`mt-0.5 text-right text-xs ${TONE_TEXT[status]}`}>
					{t(CONSEQUENCE[kind])}
				</p>
			)}
		</>
	)
}

/** The adómentes keret: a milestone where szja starts, never a status lost — so no alarm tones,
 *  no elapsed tick and no pace verdict. */
function Milestone({ l, year }: { l: TaxLimit; year: number }) {
	const { t } = useT()
	const mega = useMega()
	const monthName = useMonthName()
	const cost = (
		<Link to="/settings/company" className="underline decoration-dotted hover:text-accent">
			{t('tax.costPct', { pct: l.costPct ?? '—' })}
		</Link>
	)
	if (l.limit === null) {
		return (
			<>
				<RowHead title={t('tax.kind.ATALANY_TAXFREE')} aside={cost} pct={null} />
				<p className="mt-1 text-xs text-fg-muted">{t('tax.unpublished', { year })}</p>
			</>
		)
	}
	return (
		<>
			<RowHead
				title={t('tax.kind.ATALANY_TAXFREE')}
				aside={cost}
				pct={
					<span className="tnum text-lg font-semibold text-fg">
						{pctOf(l.used, l.limit)} %
					</span>
				}
			/>
			<Bar
				used={l.used}
				pending={l.pending}
				limit={l.limit}
				projection={null}
				tick={null}
				tone="ok"
				basis="RECEIVED"
			/>
			<div className="mt-1.5 flex flex-wrap justify-between gap-x-4 gap-y-1 text-xs text-fg-muted">
				<span className="flex flex-wrap gap-x-3">
					<UsedOf used={l.used} limit={l.limit} />
					<Pending amount={l.pending} />
				</span>
				<span>
					{l.crossedOn
						? t('tax.taxfree.spent', { month: monthName(l.crossedOn) })
						: t('tax.taxfree.left', { amount: mega(l.limit - l.used) })}
				</span>
			</div>
		</>
	)
}

// ---------------------------------------------------------------- chart

const W = 720
const H = 240
const PAD_L = 16
const PAD_R = 140
const PAD_Y = 20
// The next limit above the data is drawn only within this factor, keeping the lines in ~half the plot.
const HEADROOM = 2

type Month = Data['months'][number]
interface Series {
	key: 'invoiced' | 'received'
	stroke: string
	fill: string
	label: Key
}

const SERIES: Record<TaxLimit['basis'], Series> = {
	INVOICED: {
		key: 'invoiced',
		stroke: 'stroke-series-invoiced',
		fill: 'fill-series-invoiced',
		label: 'tax.basis.INVOICED'
	},
	RECEIVED: {
		key: 'received',
		stroke: 'stroke-series-received',
		fill: 'fill-series-received',
		label: 'tax.basis.RECEIVED'
	}
}

/**
 * The running totals of every basis in use against every limit, on one HUF axis from zero —
 * the same drawn-by-hand approach and accessibility pattern as the dashboard's `Chart`.
 */
function CumulativeChart({ data, pace }: { data: Data; pace: Pace }) {
	const { t } = useT()
	const huf = useHuf()
	const mega = useMega()
	const monthName = useMonthName()
	const [active, setActive] = useState<number | null>(null)

	const bases = [...new Set(data.limits.map((l) => l.basis))]
	const series = bases.map((b) => SERIES[b])
	const lastMonth = pace.day >= pace.days ? 12 : Math.ceil((pace.day * 12) / pace.days)
	const last = data.months[Math.max(0, lastMonth - 1)]
	const finals = series.map((s) => last?.[s.key] ?? 0)
	const projections = finals.map((f) => projected(f, pace) ?? f)
	const unpaid = (key: Series['key'], m: Month | undefined) =>
		key === 'received' ? (m?.outstanding ?? 0) : 0
	const maxes = series.map((s, i) =>
		Math.max(finals[i] ?? 0, projections[i] ?? 0, (finals[i] ?? 0) + unpaid(s.key, last))
	)
	// Every limit under the data, plus the next one if it is the basis's only line or near
	// enough; a far ceiling would flatten the lines, so it is named in the legend instead.
	const drawn: TaxLimit[] = []
	const offScale: TaxLimit[] = []
	bases.forEach((b, i) => {
		const max = maxes[i] ?? 0
		const ls = data.limits
			.filter((l) => l.basis === b && l.limit !== null)
			.sort((p, q) => (p.limit ?? 0) - (q.limit ?? 0))
		const cut = ls.findIndex((l) => (l.limit ?? 0) > max)
		const below = cut === -1 ? ls : ls.slice(0, cut)
		const above = cut === -1 ? [] : ls.slice(cut)
		drawn.push(...below)
		const next = above[0]
		if (next && (below.length === 0 || (next.limit ?? 0) <= HEADROOM * max)) {
			drawn.push(next)
			above.shift()
		}
		offScale.push(...above)
	})
	const top = Math.max(...drawn.map((l) => l.limit ?? 0), ...maxes, 1) * 1.05
	const hasUnpaid = series.some((s) => s.key === 'received') && unpaid('received', last) > 0
	const figures = (m: Month) =>
		series
			.map((s) => {
				const u = unpaid(s.key, m)
				const tail = u > 0 ? ` (${t('tax.unpaid')} ${huf(u)})` : ''
				return `${t(s.label)} ${huf(m[s.key])}${tail}`
			})
			.join(' · ')

	const slot = (W - PAD_L - PAD_R) / 12
	const x = (month: number) => PAD_L + month * slot
	const y = (n: number) => H - PAD_Y - (n / top) * (H - PAD_Y * 2)
	const todayX = PAD_L + (pace.day / pace.days) * (W - PAD_L - PAD_R)
	const shown = active === null ? null : data.months[active]

	return (
		<figure className="space-y-2">
			<figcaption className="flex flex-wrap gap-4 text-xs text-fg-muted">
				{series.map((s) => (
					<span key={s.key} className="flex items-center gap-1.5">
						<svg width="16" height="4" aria-hidden="true">
							<line
								x1="0"
								y1="2"
								x2="16"
								y2="2"
								strokeWidth="2"
								className={s.stroke}
							/>
						</svg>
						{t(s.label)}
					</span>
				))}
				{hasUnpaid && (
					<span className="flex items-center gap-1.5">
						<span aria-hidden="true" className="fill-unpaid inline-block h-2.5 w-4" />
						{t('tax.pendingLegend')}
					</span>
				)}
				{offScale.map((l) => (
					<span key={l.kind} className="flex items-center gap-1.5">
						<span aria-hidden="true">↑</span>
						{t('tax.offScale', {
							name: t(`tax.short.${l.kind}` as Key),
							amount: mega(l.limit ?? 0)
						})}
					</span>
				))}
			</figcaption>
			<svg viewBox={`0 0 ${W} ${H}`} className="w-full" role="img">
				<title>{t('tax.chart')}</title>
				<desc>{t('tax.chartDesc')}</desc>
				<defs>
					<pattern
						id="unpaid"
						width="5"
						height="5"
						patternUnits="userSpaceOnUse"
						patternTransform="rotate(45)"
					>
						<line
							x1="0"
							y1="0"
							x2="0"
							y2="5"
							strokeWidth="2"
							className="stroke-series-received"
						/>
					</pattern>
				</defs>
				<line
					x1={PAD_L}
					y1={H - PAD_Y}
					x2={W - PAD_R}
					y2={H - PAD_Y}
					className="stroke-line"
					strokeWidth="1"
				/>
				{drawn.map((l) =>
					l.limit === null ? null : (
						<g key={l.kind}>
							<line
								x1={PAD_L}
								x2={W - PAD_R}
								y1={y(l.limit)}
								y2={y(l.limit)}
								strokeWidth="1.5"
								strokeDasharray={l.kind === 'ATALANY_TAXFREE' ? '1 4' : '6 4'}
								strokeLinecap="round"
								className="stroke-fg-muted"
							/>
							<text
								x={W - PAD_R + 6}
								y={y(l.limit) + 5}
								className="fill-fg-muted text-[14px]"
							>
								{`${t(`tax.short.${l.kind}` as Key)} ${mega(l.limit)}`}
							</text>
						</g>
					)
				)}
				{series.map((s, i) => {
					let d = `M ${x(0)} ${y(0)}`
					for (let m = 0; m < lastMonth; m++) {
						const v = data.months[m]?.[s.key] ?? 0
						d += ` V ${y(v)} H ${x(m + 1)}`
					}
					const end = x(lastMonth)
					return (
						<g key={s.key}>
							{data.months.slice(0, lastMonth).map((m, i) => {
								const u = unpaid(s.key, m)
								return u > 0 ? (
									<rect
										key={m.month}
										x={x(i)}
										y={y(m[s.key] + u)}
										width={slot}
										height={y(m[s.key]) - y(m[s.key] + u)}
										fill="url(#unpaid)"
									/>
								) : null
							})}
							<path d={d} fill="none" strokeWidth="2" className={s.stroke} />
							{lastMonth < 12 && pace.projects && (
								<line
									x1={end}
									y1={y(finals[i] ?? 0)}
									x2={x(12)}
									y2={y(projections[i] ?? 0)}
									strokeWidth="2"
									strokeDasharray="2 4"
									strokeLinecap="round"
									className={s.stroke}
								/>
							)}
						</g>
					)
				})}
				{pace.day > 0 && pace.day < pace.days && (
					<g>
						<line
							x1={todayX}
							x2={todayX}
							y1={PAD_Y / 2}
							y2={H - PAD_Y}
							strokeWidth="1"
							className="stroke-fg-muted"
						/>
						<text x={todayX + 4} y={PAD_Y} className="fill-fg-muted text-[14px]">
							{t('tax.today')}
						</text>
					</g>
				)}
				{data.months.map((m, i) => (
					// biome-ignore lint/a11y/noStaticElementInteractions: an SVG <g> takes no role that leaves it focusable.
					<g
						key={m.month}
						tabIndex={0}
						onFocus={() => setActive(i)}
						onBlur={() => setActive(null)}
						onMouseEnter={() => setActive(i)}
						onMouseLeave={() => setActive(null)}
					>
						<title>{`${monthName(m.month)} · ${figures(m)}`}</title>
						<rect
							x={x(i)}
							y={PAD_Y / 2}
							width={slot}
							height={H - PAD_Y * 1.5}
							className={active === i ? 'fill-fg opacity-5' : 'fill-transparent'}
						/>
						<text
							x={x(i) + slot / 2}
							y={H - PAD_Y + 16}
							textAnchor="middle"
							className="fill-fg-muted text-[13px]"
						>
							{m.month.slice(5)}
						</text>
					</g>
				))}
			</svg>
			<p role="status" className="min-h-5 text-sm text-fg-muted">
				{shown && `${monthName(shown.month)} · ${figures(shown)}`}
			</p>
			<table className="sr-only">
				<caption>{t('tax.chart')}</caption>
				<thead>
					<tr>
						<th scope="col">{t('invoices.date')}</th>
						{series.map((s) => (
							<th key={s.key} scope="col">
								{t(s.label)}
							</th>
						))}
						{hasUnpaid && <th scope="col">{t('tax.pendingLegend')}</th>}
					</tr>
				</thead>
				<tbody>
					{data.months.map((m) => (
						<tr key={m.month}>
							<th scope="row">{m.month}</th>
							{series.map((s) => (
								<td key={s.key}>{huf(m[s.key])}</td>
							))}
							{hasUnpaid && <td>{huf(unpaid('received', m))}</td>}
						</tr>
					))}
				</tbody>
			</table>
		</figure>
	)
}

// vim: ts=4
