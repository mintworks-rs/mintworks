import type * as React from 'react'
import { useEffect, useState } from 'react'

import {
	PasskeyCancelled,
	stepUpWithPasskey,
	usePasskeyAvailable,
	useStepUp
} from '@mintworks/client'
import { Modal } from '~/components/Modal'
import { Button, ErrorBanner, Field, Input } from '~/components/ui'
import { useT } from '~/i18n'

/**
 * One dialog for both destructive confirmations: `reasonLabel` asks for free text (storno),
 * `confirmPhrase` demands the user type an exact string back (delete account). Neither is
 * required, and with neither this is a plain yes/no.
 */
export function ConfirmDialog({
	open,
	title,
	description,
	confirmLabel,
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
	const { t } = useT()
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
			<p className="text-sm text-fg-muted">{description}</p>

			{reasonLabel !== undefined && (
				<div className="mt-4">
					<Field label={reasonLabel} htmlFor="confirm-reason" required={reasonRequired}>
						<Input value={reason} onChange={(e) => setReason(e.target.value)} />
					</Field>
				</div>
			)}

			{confirmPhrase !== undefined && (
				<div className="mt-4">
					<Field
						label={t('confirm.typeToConfirm', { phrase: confirmPhrase })}
						htmlFor="confirm-phrase"
						required
					>
						<Input
							autoComplete="off"
							value={typed}
							onChange={(e) => setTyped(e.target.value)}
						/>
					</Field>
				</div>
			)}

			<div className="mt-6 flex justify-end gap-2">
				<Button variant="secondary" onClick={onClose}>
					{t('common.cancel')}
				</Button>
				<Button
					variant="danger"
					loading={loading}
					disabled={blocked}
					onClick={() => onConfirm(reason.trim())}
				>
					{confirmLabel ?? t('common.confirm')}
				</Button>
			</div>
		</Modal>
	)
}

/**
 * The re-auth prompt for `E-AUTH-STEPUP`. Deleting the account, minting an API key, stornoing
 * an invoice and adding or removing a passkey all need it. The caller catches the code, shows this, and
 * retries the same action on success.
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
	const { t, err } = useT()
	const [password, setPassword] = useState('')
	const [error, setError] = useState<string | null>(null)
	const passkey = usePasskeyAvailable(open)
	const [passkeyBusy, setPasskeyBusy] = useState(false)
	const stepUp = useStepUp()

	useEffect(() => {
		if (open) {
			setPassword('')
			setError(null)
		}
	}, [open])

	async function submit(e: React.FormEvent) {
		e.preventDefault()
		setError(null)
		try {
			await stepUp.mutateAsync(password)
			onAuthenticated()
		} catch (e2) {
			setError(err(e2))
		}
	}

	async function withPasskey() {
		setError(null)
		setPasskeyBusy(true)
		try {
			await stepUpWithPasskey()
			onAuthenticated()
		} catch (e) {
			// Dismissing the prompt is an answer, not a failure.
			if (!(e instanceof PasskeyCancelled)) setError(err(e))
		} finally {
			setPasskeyBusy(false)
		}
	}

	return (
		<Modal open={open} onClose={onClose} title={t('stepup.title')}>
			<form onSubmit={submit}>
				<p className="text-sm text-fg-muted">{t('stepup.body')}</p>
				<div className="mt-3">
					<ErrorBanner message={error} />
				</div>
				<div className="mt-4">
					<Field label={t('common.password')} htmlFor="stepup-password" required>
						<Input
							type="password"
							autoComplete="current-password"
							value={password}
							onChange={(e) => setPassword(e.target.value)}
						/>
					</Field>
				</div>
				<div className="mt-6 flex justify-end gap-2">
					<Button variant="secondary" type="button" onClick={onClose}>
						{t('common.cancel')}
					</Button>
					<Button type="submit" loading={stepUp.isPending}>
						{t('stepup.continue')}
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
						{t('stepup.passkey')}
					</Button>
				)}
			</form>
		</Modal>
	)
}

// vim: ts=4
