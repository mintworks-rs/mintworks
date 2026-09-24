import * as React from 'react'

import type { MoneyWire } from '@saas-framework/client'

import { useT } from '~/i18n'

type Variant = 'primary' | 'secondary' | 'danger' | 'ghost'

// Fill ≠ `--danger`: the dark `--danger` is a light text red and glares as a fill.
const variantClass: Record<Variant, string> = {
	primary: 'bg-accent text-accent-fg hover:opacity-90 disabled:opacity-50',
	secondary:
		'bg-surface-raised text-fg border border-line-strong shadow-sm hover:bg-surface-sunken disabled:opacity-50',
	danger: 'bg-danger-fill text-danger-fill-fg hover:opacity-90 disabled:opacity-50',
	ghost: 'text-fg-muted hover:bg-surface-sunken disabled:opacity-50'
}

/** The button look for an element that must not be a `<button>`, e.g. a router `Link`. */
export function buttonClass(variant: Variant = 'primary'): string {
	return (
		'inline-flex min-h-[44px] items-center justify-center gap-2 rounded-md px-4 py-2 ' +
		'text-sm font-medium transition-opacity disabled:cursor-not-allowed ' +
		variantClass[variant]
	)
}

export function Button({
	variant = 'primary',
	loading,
	className = '',
	children,
	...rest
}: React.ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant; loading?: boolean }) {
	return (
		<button
			{...rest}
			disabled={rest.disabled || loading}
			className={`${buttonClass(variant)} ${className}`}
		>
			{loading && <Spinner />}
			{children}
		</button>
	)
}

export function Spinner({ className = '' }: { className?: string }) {
	return (
		<span
			className={
				'inline-block h-4 w-4 animate-spin rounded-full border-2 border-current ' +
				'motion-reduce:animate-none border-t-transparent ' +
				className
			}
			aria-hidden="true"
		/>
	)
}

const fieldClass =
	'min-h-[44px] w-full rounded-md border border-line-strong bg-surface-raised px-3 py-2 text-sm ' +
	'text-fg placeholder:text-fg-muted focus:border-accent'

export const Input = React.forwardRef<
	HTMLInputElement,
	React.InputHTMLAttributes<HTMLInputElement>
>(function Input({ className = '', ...rest }, ref) {
	return <input ref={ref} {...rest} className={`${fieldClass} ${className}`} />
})

export const Select = React.forwardRef<
	HTMLSelectElement,
	React.SelectHTMLAttributes<HTMLSelectElement>
>(function Select({ className = '', children, ...rest }, ref) {
	return (
		<select ref={ref} {...rest} className={`${fieldClass} ${className}`}>
			{children}
		</select>
	)
})

export function Label({
	htmlFor,
	required,
	children
}: {
	htmlFor: string
	required?: boolean
	children: React.ReactNode
}) {
	return (
		<label htmlFor={htmlFor} className="text-sm font-medium text-fg">
			{children}
			{required && (
				<span className="ml-0.5 text-danger" aria-hidden="true">
					*
				</span>
			)}
		</label>
	)
}

/**
 * Label + control + error/hint, wired together with `aria-describedby`.
 *
 * The id and aria props are cloned onto the single child, so the caller writes
 * `<Field …><Input type="email" /></Field>` and never repeats the id.
 */
export function Field({
	label,
	htmlFor,
	required,
	error,
	hint,
	children
}: {
	label: string
	htmlFor: string
	required?: boolean
	error?: string
	hint?: string
	children: React.ReactElement
}) {
	const describedBy = error ? `${htmlFor}-error` : hint ? `${htmlFor}-hint` : undefined
	return (
		<div className="flex flex-col gap-1">
			<Label htmlFor={htmlFor} required={required}>
				{label}
			</Label>
			{React.cloneElement(children, {
				id: htmlFor,
				'aria-required': required,
				'aria-invalid': !!error,
				'aria-describedby': describedBy
			} as React.HTMLAttributes<HTMLElement>)}
			{hint && !error && (
				<p id={`${htmlFor}-hint`} className="text-xs text-fg-muted">
					{hint}
				</p>
			)}
			{error && (
				<p id={`${htmlFor}-error`} role="alert" className="text-xs text-danger">
					{error}
				</p>
			)}
		</div>
	)
}

export type Tone = 'neutral' | 'positive' | 'warning' | 'danger' | 'accent'

const toneClass: Record<Tone, string> = {
	neutral: 'border-line-strong text-fg-muted',
	positive: 'border-positive text-positive',
	warning: 'border-warning text-warning',
	danger: 'border-danger text-danger',
	accent: 'border-accent text-accent'
}

/**
 * Outlined, not filled: the same border-plus-text pair clears 4.5:1 on every surface in both
 * themes, where a tinted fill would need a second palette. The label is always the status
 * word — colour is the second signal here, never the only one.
 */
export function StatusChip({
	tone = 'neutral',
	children
}: {
	tone?: Tone
	children: React.ReactNode
}) {
	return (
		<span
			className={
				'inline-flex items-center rounded-full border px-2 py-0.5 text-xs font-medium ' +
				toneClass[tone]
			}
		>
			{children}
		</span>
	)
}

/** Every amount on the screen: `tnum` is the tabular-nums rule from `index.css`. */
export function MoneyText({
	value,
	className = ''
}: {
	value: MoneyWire | null | undefined
	className?: string
}) {
	const { money } = useT()
	return <span className={`tnum ${className}`}>{money(value)}</span>
}

export function Skeleton({ className = '' }: { className?: string }) {
	return (
		<div
			aria-hidden="true"
			className={`animate-pulse rounded bg-surface-sunken motion-reduce:animate-none ${className}`}
		/>
	)
}

/** A table-shaped placeholder, so the page does not jump when the rows arrive. */
export function SkeletonTable({ rows = 5 }: { rows?: number }) {
	return (
		<div className="space-y-2 rounded-xl border border-line bg-surface-raised p-4">
			{Array.from({ length: rows }, (_, i) => (
				<Skeleton key={i} className="h-10 w-full" />
			))}
		</div>
	)
}

export function EmptyState({
	title,
	description,
	action
}: {
	title: string
	description?: string
	action?: React.ReactNode
}) {
	return (
		<div className="flex flex-col items-center justify-center gap-3 rounded-lg border border-dashed border-line-strong bg-surface-raised p-12 text-center">
			<h3 className="text-base font-semibold text-fg">{title}</h3>
			{description && <p className="max-w-sm text-sm text-fg-muted">{description}</p>}
			{action}
		</div>
	)
}

/** Full-page centred spinner, for a route that is still resolving its session. */
export function PageSpinner() {
	return (
		<div className="flex min-h-full items-center justify-center p-12">
			<Spinner className="text-accent" />
		</div>
	)
}

/** The centred card every public auth screen sits in. */
export function AuthCard({
	title,
	subtitle,
	children
}: {
	title: string
	subtitle?: React.ReactNode
	children: React.ReactNode
}) {
	return (
		<main className="flex min-h-full items-center justify-center px-4 py-12">
			<div className="w-full max-w-md rounded-xl border border-line bg-surface-raised p-8 shadow-sm">
				<h1 className="text-xl font-semibold text-fg">{title}</h1>
				{subtitle && <p className="mt-1 text-sm text-fg-muted">{subtitle}</p>}
				<div className="mt-6">{children}</div>
			</div>
		</main>
	)
}

export function ErrorBanner({ message }: { message: string | null }) {
	if (!message) return null
	return (
		<p role="alert" className="rounded-md border border-danger px-3 py-2 text-sm text-danger">
			{message}
		</p>
	)
}

// vim: ts=4
