import type { ReactNode } from 'react'
import { useEffect, useId, useRef } from 'react'

/**
 * Native `<dialog>`: the focus trap, the backdrop, Esc-to-close and `aria-modal` all come
 * with it, which is why this file has no focus management of its own.
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
	children: ReactNode
}) {
	const ref = useRef<HTMLDialogElement>(null)
	// Per instance: a screen mounting two Modals gave both headings the same id, so
	// `aria-labelledby` pointed at whichever came first in the document.
	const titleId = useId()

	useEffect(() => {
		const d = ref.current
		if (!d) return
		if (open && !d.open) d.showModal()
		if (!open && d.open) d.close()
	}, [open])

	return (
		<dialog
			ref={ref}
			onClose={onClose}
			onCancel={onClose}
			aria-labelledby={titleId}
			className="w-[min(32rem,calc(100vw-2rem))] rounded-xl border border-slate-200 p-0 backdrop:bg-slate-900/40"
		>
			<div className="border-b border-slate-200 px-5 py-4">
				<h2 id={titleId} className="text-base font-semibold text-slate-900">
					{title}
				</h2>
			</div>
			<div className="px-5 py-4">{children}</div>
		</dialog>
	)
}

// vim: ts=4
