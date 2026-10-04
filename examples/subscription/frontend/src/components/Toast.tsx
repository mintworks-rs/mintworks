import * as React from 'react'

type ToastKind = 'success' | 'error' | 'info'

interface Toast {
	id: number
	kind: ToastKind
	message: string
}

interface ToastApi {
	success: (msg: string) => void
	error: (msg: string) => void
	info: (msg: string) => void
}

const ToastCtx = React.createContext<ToastApi | null>(null)

let nextId = 1

/** Mounted once at app root; owns the stack and renders the viewport. */
export function ToastProvider({ children }: { children: React.ReactNode }) {
	const [toasts, setToasts] = React.useState<Toast[]>([])
	const remove = React.useCallback((id: number) => {
		setToasts((t) => t.filter((x) => x.id !== id))
	}, [])
	const api = React.useMemo<ToastApi>(() => {
		const push = (kind: ToastKind, message: string) => {
			const id = nextId++
			setToasts((t) => [...t, { id, kind, message }])
			// An error stays until dismissed: a user who looked away would never learn that an
			// action did not happen. WCAG 2.2.1 wants this too.
			if (kind !== 'error') setTimeout(() => remove(id), 6000)
		}
		return {
			success: (m) => push('success', m),
			error: (m) => push('error', m),
			info: (m) => push('info', m)
		}
	}, [remove])

	return (
		<ToastCtx.Provider value={api}>
			{children}
			{/* The live region is the toast's own `role`, not this wrapper: nesting live regions
			    is undefined across screen readers, and `aria-atomic` here re-announced the whole
			    stack every time any toast appeared or expired. */}
			<div className="fixed bottom-4 right-4 z-50 flex w-[min(20rem,calc(100vw-2rem))] flex-col gap-2">
				{toasts.map((t) => (
					<div
						key={t.id}
						role={t.kind === 'error' ? 'alert' : 'status'}
						className={
							'flex items-start gap-2 rounded-lg px-4 py-3 text-sm shadow-lg ' +
							(t.kind === 'success'
								? 'bg-green-600 text-white'
								: t.kind === 'error'
									? 'bg-red-600 text-white'
									: 'bg-slate-800 text-white')
						}
					>
						<span className="font-medium">
							{t.kind === 'success'
								? 'Success'
								: t.kind === 'error'
									? 'Error'
									: 'Info'}
							:
						</span>
						<span className="flex-1">{t.message}</span>
						<button
							type="button"
							onClick={() => remove(t.id)}
							aria-label="Dismiss notification"
							className="flex h-8 w-8 shrink-0 items-center justify-center rounded text-white/80 hover:text-white"
						>
							×
						</button>
					</div>
				))}
			</div>
		</ToastCtx.Provider>
	)
}

export function useToast(): ToastApi {
	const api = React.useContext(ToastCtx)
	if (!api) throw new Error('useToast used outside ToastProvider')
	return api
}

// vim: ts=4
