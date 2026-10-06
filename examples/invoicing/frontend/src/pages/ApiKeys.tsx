// SPDX-License-Identifier: MIT-0
import { useState } from 'react'

import type { ApiKeyView, MintedKey } from '@mintworks/client'
import {
	ServerError,
	localDate,
	useApiKeyScopes,
	useApiKeys,
	useCreateApiKey,
	useRenameApiKey,
	useRevokeApiKey
} from '@mintworks/client'
import { ConfirmDialog, StepUpDialog } from '~/components/ConfirmDialog'
import { useToast } from '~/components/Toast'
import { Button, ErrorBanner, Field, Input, PageSpinner, StatusChip } from '~/components/ui'
import { useT } from '~/i18n'

/** How close an expiry has to be before it stops being a date and starts being a warning. */
const SOON_DAYS = 14

/**
 * The org's keys. Minting is step-up gated; revoking is not, and renaming is neither — the two
 * that grant nothing carry no prompt, so shutting a leaked key down is never behind a password.
 *
 * Nothing here is reachable by a key: `/api/api-keys` sets no scope prefix, and `auth_mw` fails
 * closed on an `Actor::Key` reaching an unscoped route.
 */
export function ApiKeys() {
	const { t, err } = useT()
	const toast = useToast()
	const list = useApiKeys()
	const scopes = useApiKeyScopes()
	const mint = useCreateApiKey()
	const [name, setName] = useState('')
	const [chosen, setChosen] = useState<string[]>([])
	const [until, setUntil] = useState('')
	// The plaintext exists in this one response and nowhere else, so it is held here until the
	// user says they have saved it — and then never again.
	const [minted, setMinted] = useState<MintedKey | null>(null)
	const [stepUp, setStepUp] = useState(false)
	const prefixes = scopes.data?.prefixes ?? []

	function toggle(scope: string) {
		setChosen((c) => (c.includes(scope) ? c.filter((s) => s !== scope) : [...c, scope]))
	}

	async function create() {
		try {
			setMinted(
				await mint.mutateAsync({
					name,
					scopes: chosen,
					// The user's calendar day ends at local midnight, converted to UTC here;
					// `T00:00:00Z` would lapse the key at the start of the chosen day.
					expiresAt:
						until === ''
							? undefined
							: new Date(`${until}T23:59:59`).toISOString().replace(/\.\d{3}Z$/, 'Z')
				})
			)
			setStepUp(false)
			setName('')
			setChosen([])
			setUntil('')
		} catch (e) {
			// Handled inline, like the export and delete buttons on the Account screen: a bounce
			// to /login would throw away the form the user just filled in.
			if (e instanceof ServerError && e.errCode === 'E-AUTH-STEPUP') {
				setStepUp(true)
				return
			}
			setStepUp(false)
			toast.error(err(e))
		}
	}

	if (minted !== null) return <Reveal minted={minted} onSaved={() => setMinted(null)} />

	return (
		<div className="space-y-8">
			<section>
				<h1 className="text-lg font-semibold text-fg">{t('keys.title')}</h1>
				<p className="mt-1 text-sm text-fg-muted">{t('keys.intro')}</p>
				<div className="mt-4 rounded-xl border border-line bg-surface-raised p-5">
					<div className="max-w-sm">
						<Field label={t('common.name')} htmlFor="key-name" required>
							<Input
								placeholder="CI deploy"
								value={name}
								onChange={(e) => setName(e.target.value)}
							/>
						</Field>
					</div>
					<fieldset className="mt-4">
						<legend className="text-sm font-medium text-fg">{t('keys.scopes')}</legend>
						{prefixes.length === 0 ? (
							<p className="mt-1 text-sm text-fg-muted">{t('keys.noScopes')}</p>
						) : (
							<div className="mt-2 flex flex-wrap gap-x-8 gap-y-2">
								{prefixes.map((p) => (
									<div key={p} className="flex items-center gap-3">
										<span className="w-20 text-sm text-fg">{p}</span>
										{['read', 'write'].map((verb) => {
											const scope = `${p}:${verb}`
											return (
												<label
													key={verb}
													className="flex items-center gap-1 text-sm text-fg-muted"
												>
													<input
														type="checkbox"
														checked={chosen.includes(scope)}
														onChange={() => toggle(scope)}
														className="h-4 w-4 accent-accent"
													/>
													{verb}
												</label>
											)
										})}
									</div>
								))}
							</div>
						)}
					</fieldset>
					<div className="mt-4 max-w-xs">
						<Field label={t('keys.expires')} htmlFor="key-until">
							{/* The native control, not a picker dependency. */}
							<Input
								type="date"
								min={localDate()}
								value={until}
								onChange={(e) => setUntil(e.target.value)}
							/>
						</Field>
					</div>
					<Button
						className="mt-4"
						loading={mint.isPending}
						disabled={name.trim() === '' || chosen.length === 0}
						onClick={() => void create()}
					>
						{t('keys.create')}
					</Button>
				</div>
			</section>

			<section>
				<h2 className="text-base font-semibold text-fg">{t('keys.list')}</h2>
				<div className="mt-4">
					{list.isPending ? (
						<PageSpinner />
					) : list.error ? (
						<ErrorBanner message={err(list.error)} />
					) : (
						<KeyList keys={list.data?.items ?? []} />
					)}
				</div>
			</section>

			<StepUpDialog
				open={stepUp}
				onClose={() => setStepUp(false)}
				onAuthenticated={() => {
					// Cleared before the retry, so a second Continue cannot mint a second key.
					setStepUp(false)
					void create()
				}}
			/>
		</div>
	)
}

function KeyList({ keys }: { keys: ApiKeyView[] }) {
	const { t } = useT()
	if (keys.length === 0) return <p className="text-sm text-fg-muted">{t('keys.empty')}</p>
	return (
		<ul className="divide-y divide-line rounded-xl border border-line bg-surface-raised">
			{keys.map((k) => (
				<KeyRow key={k.uid} apiKey={k} />
			))}
		</ul>
	)
}

function KeyRow({ apiKey }: { apiKey: ApiKeyView }) {
	const { t, err, date } = useT()
	const toast = useToast()
	const [name, setName] = useState(apiKey.name)
	const [confirmRevoke, setConfirmRevoke] = useState(false)
	const rename = useRenameApiKey()
	const revoke = useRevokeApiKey()
	const soon =
		apiKey.expiresAt !== null &&
		Date.parse(apiKey.expiresAt) - Date.now() < SOON_DAYS * 86_400_000

	return (
		<li className="flex flex-wrap items-center gap-2 p-4">
			<Input
				aria-label={t('keys.name')}
				value={name}
				className="max-w-xs"
				onChange={(e) => setName(e.target.value)}
			/>
			<Button
				variant="secondary"
				disabled={name.trim() === '' || name === apiKey.name}
				loading={rename.isPending}
				onClick={() =>
					rename
						.mutateAsync({ uid: apiKey.uid, name })
						.then(() => toast.success(t('keys.renamed')))
						.catch((e) => toast.error(err(e)))
				}
			>
				{t('common.save')}
			</Button>
			<code className="text-xs text-fg-muted">{apiKey.prefix}…</code>
			<span className="text-xs text-fg-muted">{apiKey.scopes.join(', ')}</span>
			<span className="text-xs text-fg-muted">
				{apiKey.lastUsedAt === null
					? t('keys.never')
					: t('keys.used', { date: date(apiKey.lastUsedAt) })}
			</span>
			{apiKey.expiresAt !== null &&
				(soon ? (
					<StatusChip tone="warning">
						{t('keys.expiresOn', { date: date(apiKey.expiresAt) })}
					</StatusChip>
				) : (
					<span className="text-xs text-fg-muted">
						{t('keys.expiresOn', { date: date(apiKey.expiresAt) })}
					</span>
				))}
			<Button
				variant="danger"
				loading={revoke.isPending}
				onClick={() => setConfirmRevoke(true)}
			>
				{t('keys.revoke')}
			</Button>
			<ConfirmDialog
				open={confirmRevoke}
				title={t('keys.revokeTitle')}
				description={t('keys.revokeBody', { name: apiKey.name })}
				confirmLabel={t('keys.revokeConfirm')}
				loading={revoke.isPending}
				onClose={() => setConfirmRevoke(false)}
				onConfirm={() => {
					setConfirmRevoke(false)
					revoke
						.mutateAsync(apiKey.uid)
						.then(() => toast.success(t('keys.revoked')))
						.catch((e) => toast.error(err(e)))
				}}
			/>
		</li>
	)
}

/**
 * The one time the plaintext exists. It replaces the page rather than opening a dialog: no
 * corner close, no click-outside and no Esc, so the only way past it is the button that says
 * the key has been saved — a dismissal would be a key nobody can ever read again.
 */
function Reveal({ minted, onSaved }: { minted: MintedKey; onSaved: () => void }) {
	const { t } = useT()
	const toast = useToast()

	async function copy() {
		try {
			await navigator.clipboard.writeText(minted.key)
			toast.success(t('keys.copied'))
		} catch {
			toast.error(t('keys.copyFailed'))
		}
	}

	return (
		<section className="max-w-2xl rounded-xl border border-line bg-surface-raised p-5">
			<h1 className="text-lg font-semibold text-fg">{t('keys.new')}</h1>
			<p className="mt-1 text-sm text-fg-muted">{t('keys.newHint')}</p>
			<div className="mt-4 flex flex-wrap items-center gap-2">
				<Input
					readOnly
					aria-label={t('keys.value')}
					value={minted.key}
					className="min-w-[18rem] flex-1 font-mono"
				/>
				<Button variant="secondary" onClick={() => void copy()}>
					{t('keys.copy')}
				</Button>
			</div>
			<p className="mt-4 text-sm text-fg-muted">
				<span>{t('common.name')}:</span> <span className="text-fg">{minted.name}</span>
				<span className="ml-4">{t('keys.scopes')}:</span>{' '}
				<span className="text-fg">{minted.scopes.join(', ')}</span>
			</p>
			<Button className="mt-6" onClick={onSaved}>
				{t('keys.savedIt')}
			</Button>
		</section>
	)
}

// vim: ts=4
