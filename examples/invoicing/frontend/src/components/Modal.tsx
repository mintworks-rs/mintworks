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
	children,
	className = 'w-[min(32rem,calc(100vw-2rem))]'
}: {
	open: boolean
	onClose: () => void
	title: string
	children: ReactNode
	className?: string
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
		// biome-ignore lint/a11y/useKeyWithClickEvents: the click only dismisses the backdrop; <dialog> closes on Escape through onCancel.
		<dialog
			ref={ref}
			// React delivers a nested dialog's close/cancel to every ancestor's handler too.
			onClose={(e) => {
				if (e.target === ref.current) onClose()
			}}
			onCancel={(e) => {
				if (e.target === ref.current) onClose()
			}}
			// The element has no padding of its own, so a click whose target *is* the dialog
			// landed on the backdrop — `<dialog>` gives no other way to tell the two apart.
			onClick={(e) => {
				if (e.target === ref.current) onClose()
			}}
			aria-labelledby={titleId}
			className={`overflow-y-auto rounded-xl border border-line bg-surface-raised p-0 text-fg backdrop:bg-black/50 ${className}`}
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
