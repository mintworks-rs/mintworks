// SPDX-License-Identifier: MIT-0
import * as React from 'react'

import { useT } from '~/i18n'

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
	const { t } = useT()
	const [toasts, setToasts] = React.useState<Toast[]>([])
	const remove = React.useCallback((id: number) => {
		setToasts((x) => x.filter((y) => y.id !== id))
	}, [])
	const api = React.useMemo<ToastApi>(() => {
		const push = (kind: ToastKind, message: string) => {
			const id = nextId++
			setToasts((x) => [...x, { id, kind, message }])
			// An error stays until dismissed: it is the only failure surface for issue, storno
			// and key revocation, and a user who looked away would never learn that an
			// irreversible action did not happen. WCAG 2.2.1 wants this too.
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
				{toasts.map((x) => (
					<div
						key={x.id}
						role={x.kind === 'error' ? 'alert' : 'status'}
						className={
							'flex items-start gap-2 rounded-lg border bg-surface-raised px-4 py-3 text-sm shadow-lg ' +
							(x.kind === 'success'
								? 'border-positive text-positive'
								: x.kind === 'error'
									? 'border-danger text-danger'
									: 'border-line-strong text-fg')
						}
					>
						{/* The word, not just the colour. */}
						<span className="font-medium">
							{x.kind === 'success'
								? t('toast.success')
								: x.kind === 'error'
									? t('toast.error')
									: t('toast.info')}
							:
						</span>
						<span className="flex-1 text-fg">{x.message}</span>
						<button
							type="button"
							onClick={() => remove(x.id)}
							aria-label={t('toast.dismiss')}
							className="flex h-8 w-8 shrink-0 items-center justify-center rounded text-fg-muted hover:text-fg"
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
