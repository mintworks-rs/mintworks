import type * as React from 'react'
import { useEffect, useState } from 'react'

import { errMsg } from '~/api/client'
import { useStepUp } from '~/api/hooks'
import { PasskeyCancelled, platformAuthenticatorAvailable, stepUpWithPasskey } from '~/auth/webauthn'
import { Modal } from '~/components/Modal'
import { Button, ErrorBanner, Field, Input } from '~/components/ui'

/**
 * One dialog for both destructive confirmations: `reasonLabel` asks for free text (storno),
 * `confirmPhrase` demands the user type an exact string back (delete account). Neither is
 * required, and with neither this is a plain yes/no.
 */
export function ConfirmDialog({
	open,
	title,
	description,
	confirmLabel = 'Confirm',
	reasonLabel,
	reasonRequired,
	confirmPhrase,
	loading,
	onConfirm,
	onClose
}: {
	open: boolean
	title: string
	description: string
	confirmLabel?: string
	reasonLabel?: string
	/** The server refuses a blank one — a storno's reason is on an immutable document. */
	reasonRequired?: boolean
	confirmPhrase?: string
	loading?: boolean
	onConfirm: (reason: string) => void
	onClose: () => void
}) {
	const [reason, setReason] = useState('')
	const [typed, setTyped] = useState('')

	useEffect(() => {
		if (open) {
			setReason('')
			setTyped('')
		}
	}, [open])

	const blocked =
		(confirmPhrase !== undefined && typed.trim() !== confirmPhrase) ||
		(reasonRequired === true && reason.trim() === '')

	return (
		<Modal open={open} onClose={onClose} title={title}>
			<p className="text-sm text-slate-600">{description}</p>

			{reasonLabel !== undefined && (
				<div className="mt-4">
					<Field label={reasonLabel} htmlFor="confirm-reason" required={reasonRequired}>
						<Input
							id="confirm-reason"
							value={reason}
							onChange={(e) => setReason(e.target.value)}
						/>
					</Field>
				</div>
			)}

			{confirmPhrase !== undefined && (
				<div className="mt-4">
					<Field
						label={`Type ${confirmPhrase} to confirm`}
						htmlFor="confirm-phrase"
						required
					>
						<Input
							id="confirm-phrase"
							value={typed}
							autoComplete="off"
							onChange={(e) => setTyped(e.target.value)}
						/>
					</Field>
				</div>
			)}

			<div className="mt-6 flex justify-end gap-2">
				<Button variant="secondary" onClick={onClose}>
					Cancel
				</Button>
				<Button
					variant="danger"
					loading={loading}
					disabled={blocked}
					onClick={() => onConfirm(reason.trim())}
				>
					{confirmLabel}
				</Button>
			</div>
		</Modal>
	)
}

/**
 * The re-auth prompt for `E-AUTH-STEPUP`. Deleting the account, minting an API key, adding or
 * removing a passkey and a storno all need it. The caller catches the code, shows this, and
 * retries the same action on success — the passkey path resolves the same action.
 */
export function StepUpDialog({
	open,
	onClose,
	onAuthenticated
}: {
	open: boolean
	onClose: () => void
	onAuthenticated: () => void
}) {
	const [password, setPassword] = useState('')
	const [error, setError] = useState<string | null>(null)
	const [passkey, setPasskey] = useState(false)
	const [passkeyBusy, setPasskeyBusy] = useState(false)
	const stepUp = useStepUp()

	useEffect(() => {
		if (open) {
			setPassword('')
			setError(null)
			void platformAuthenticatorAvailable().then(setPasskey)
		}
	}, [open])

	async function submit(e: React.FormEvent) {
		e.preventDefault()
		setError(null)
		try {
			await stepUp.mutateAsync(password)
			onAuthenticated()
		} catch (err) {
			setError(errMsg(err))
		}
	}

	async function withPasskey() {
		setError(null)
		setPasskeyBusy(true)
		try {
			await stepUpWithPasskey()
			onAuthenticated()
		} catch (err) {
			// Dismissing the prompt is an answer, not a failure.
			if (!(err instanceof PasskeyCancelled)) setError(errMsg(err))
		} finally {
			setPasskeyBusy(false)
		}
	}

	return (
		<Modal open={open} onClose={onClose} title="Confirm it is you">
			<form onSubmit={submit}>
				<p className="text-sm text-slate-600">
					For your security, confirm it is you before this goes through.
				</p>
				<ErrorBanner message={error} />
				<div className="mt-4">
					<Field label="Password" htmlFor="stepup-password" required>
						<Input
							id="stepup-password"
							type="password"
							autoComplete="current-password"
							value={password}
							onChange={(e) => setPassword(e.target.value)}
						/>
					</Field>
				</div>
				<div className="mt-6 flex justify-end gap-2">
					<Button variant="secondary" type="button" onClick={onClose}>
						Cancel
					</Button>
					<Button type="submit" loading={stepUp.isPending}>
						Continue
					</Button>
				</div>
				{passkey && (
					<Button
						className="mt-3 w-full"
						variant="ghost"
						type="button"
						loading={passkeyBusy}
						onClick={() => void withPasskey()}
					>
						Use a passkey
					</Button>
				)}
			</form>
		</Modal>
	)
}

// vim: ts=4
