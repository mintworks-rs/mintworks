import * as React from 'react'

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

/** Label + control + hint, wired with `aria-describedby`; the id is cloned onto the child. */
export function Field({
	label,
	htmlFor,
	required,
	hint,
	children
}: {
	label: string
	htmlFor: string
	required?: boolean
	hint?: string
	children: React.ReactElement
}) {
	return (
		<div className="flex flex-col gap-1">
			<label htmlFor={htmlFor} className="text-sm font-medium text-fg">
				{label}
				{required && (
					<span className="ml-0.5 text-danger" aria-hidden="true">
						*
					</span>
				)}
			</label>
			{React.cloneElement(children, {
				id: htmlFor,
				'aria-required': required,
				'aria-describedby': hint ? `${htmlFor}-hint` : undefined
			} as React.HTMLAttributes<HTMLElement>)}
			{hint && (
				<p id={`${htmlFor}-hint`} className="text-xs text-fg-muted">
					{hint}
				</p>
			)}
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

/**
 * Native `<dialog>`: the focus trap, the backdrop, Esc-to-close and `aria-modal` all come
 * with it, which is why there is no focus management here.
 */
export function Modal({
	open,
	onClose,
	title,
	children
}: {
	open: boolean
	onClose: () => void
	title: string
	children: React.ReactNode
}) {
	const ref = React.useRef<HTMLDialogElement>(null)
	const titleId = React.useId()

	React.useEffect(() => {
		const d = ref.current
		if (!d) return
		if (open && !d.open) d.showModal()
		if (!open && d.open) d.close()
	}, [open])

	return (
		// biome-ignore lint/a11y/useKeyWithClickEvents: the click only dismisses the backdrop; <dialog> closes on Escape through onCancel.
		<dialog
			ref={ref}
			onClose={(e) => {
				if (e.target === ref.current) onClose()
			}}
			onCancel={(e) => {
				if (e.target === ref.current) onClose()
			}}
			// No padding on the element itself, so a click whose target *is* the dialog
			// landed on the backdrop.
			onClick={(e) => {
				if (e.target === ref.current) onClose()
			}}
			aria-labelledby={titleId}
			className="w-[min(32rem,calc(100vw-2rem))] overflow-y-auto rounded-xl border border-line bg-surface-raised p-0 text-fg backdrop:bg-black/50"
		>
			<div className="border-b border-line px-5 py-4">
				<h2 id={titleId} className="text-base font-semibold text-fg">
					{title}
				</h2>
			</div>
			<div className="px-5 py-4">{children}</div>
		</dialog>
	)
}

// vim: ts=4
