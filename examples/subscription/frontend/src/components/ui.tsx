import * as React from 'react'

type Variant = 'primary' | 'secondary' | 'danger' | 'warning' | 'ghost'

const variantClass: Record<Variant, string> = {
	primary: 'bg-brand-600 text-white hover:bg-brand-700 disabled:bg-brand-600/50',
	secondary:
		'bg-white text-slate-800 border border-slate-400 shadow-sm hover:bg-slate-50 disabled:opacity-50',
	danger: 'bg-red-600 text-white hover:bg-red-700 disabled:bg-red-600/50',
	warning: 'bg-amber-700 text-white hover:bg-amber-800 disabled:bg-amber-700/50',
	ghost: 'text-slate-600 hover:bg-slate-100 disabled:opacity-50'
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
			className={
				'inline-flex min-h-[44px] items-center justify-center gap-2 rounded-md px-4 py-2 ' +
				'text-sm font-medium transition-colors disabled:cursor-not-allowed ' +
				variantClass[variant] +
				' ' +
				className
			}
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
				'motion-reduce:animate-none ' +
				'border-t-transparent ' +
				className
			}
			aria-hidden="true"
		/>
	)
}

const fieldClass =
	'min-h-[44px] w-full rounded-md border border-slate-500 bg-white px-3 py-2 text-sm ' +
	'text-slate-900 placeholder:text-slate-500 focus:border-brand-500'

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
		<label htmlFor={htmlFor} className="text-sm font-medium text-slate-700">
			{children}
			{required && (
				<span className="ml-0.5 text-red-600" aria-hidden="true">
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
				<p id={`${htmlFor}-hint`} className="text-xs text-slate-500">
					{hint}
				</p>
			)}
			{error && (
				<p id={`${htmlFor}-error`} role="alert" className="text-xs text-red-600">
					{error}
				</p>
			)}
		</div>
	)
}

export function Badge({
	children,
	tone = 'neutral'
}: {
	children: React.ReactNode
	tone?: 'neutral' | 'success' | 'warning' | 'danger' | 'info'
}) {
	const cls = {
		neutral: 'bg-slate-100 text-slate-700',
		success: 'bg-green-100 text-green-800',
		warning: 'bg-amber-100 text-amber-800',
		danger: 'bg-red-100 text-red-800',
		info: 'bg-brand-100 text-brand-700'
	}[tone]
	return (
		<span
			className={
				'inline-flex items-center rounded-full px-2 py-0.5 text-xs font-medium ' + cls
			}
		>
			{children}
		</span>
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
		<div className="flex flex-col items-center justify-center gap-3 rounded-lg border border-dashed border-slate-300 bg-white p-12 text-center">
			<h3 className="text-base font-semibold text-slate-700">{title}</h3>
			{description && <p className="max-w-sm text-sm text-slate-500">{description}</p>}
			{action}
		</div>
	)
}

/** Full-page centred spinner, for a route that is still resolving its session. */
export function PageSpinner() {
	return (
		<div className="flex min-h-full items-center justify-center">
			<Spinner className="text-brand-600" />
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
			<div className="w-full max-w-md rounded-xl border border-slate-200 bg-white p-8 shadow-sm">
				<h1 className="text-xl font-semibold text-slate-900">{title}</h1>
				{subtitle && <p className="mt-1 text-sm text-slate-500">{subtitle}</p>}
				<div className="mt-6">{children}</div>
			</div>
		</main>
	)
}

/** Error banner for a failed submit. */
export function ErrorBanner({ message }: { message: string | null }) {
	if (!message) return null
	return (
		<p role="alert" className="rounded-md bg-red-50 px-3 py-2 text-sm text-red-700">
			{message}
		</p>
	)
}

// vim: ts=4
