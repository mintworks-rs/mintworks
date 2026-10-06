import { useState } from 'react'

import type { ApiKeyView, MintedKey } from '@mintworks/client'
import {
	ServerError,
	errMsg,
	useApiKeyScopes,
	useApiKeys,
	useCreateApiKey,
	useRenameApiKey,
	useRevokeApiKey
} from '@mintworks/client'
import { ConfirmDialog, StepUpDialog } from '~/components/ConfirmDialog'
import { useToast } from '~/components/Toast'
import { Badge, Button, ErrorBanner, Field, Input, PageSpinner } from '~/components/ui'
import { date, localDate } from '~/lib/money'

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
					// The picked day is the user's calendar day, so it ends at local midnight, converted to
					// UTC here: `T00:00:00Z` lapsed the key at the start of the chosen day, not its end.
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
			toast.error(errMsg(e))
		}
	}

	if (minted !== null) return <Reveal minted={minted} onSaved={() => setMinted(null)} />

	return (
		<div className="space-y-8">
			<section>
				<h1 className="text-lg font-semibold text-slate-900">API keys</h1>
				<p className="mt-1 text-sm text-slate-500">
					A key authenticates on its own and holds no role of its own: the scopes below
					are everything it can reach, and it can never mint another key.
				</p>
				<div className="mt-4 rounded-xl border border-slate-200 bg-white p-5">
					<div className="max-w-sm">
						<Field label="Name" htmlFor="key-name" required>
							<Input
								id="key-name"
								value={name}
								placeholder="CI deploy"
								onChange={(e) => setName(e.target.value)}
							/>
						</Field>
					</div>
					<fieldset className="mt-4">
						<legend className="text-sm font-medium text-slate-700">Scopes</legend>
						{prefixes.length === 0 ? (
							<p className="mt-1 text-sm text-slate-500">
								This deployment registers no scope prefixes.
							</p>
						) : (
							<div className="mt-2 flex flex-wrap gap-x-8 gap-y-2">
								{prefixes.map((p) => (
									<div key={p} className="flex items-center gap-3">
										<span className="w-20 text-sm text-slate-700">{p}</span>
										{['read', 'write'].map((verb) => {
											const scope = `${p}:${verb}`
											return (
												<label
													key={verb}
													className="flex items-center gap-1 text-sm text-slate-600"
												>
													<input
														type="checkbox"
														checked={chosen.includes(scope)}
														onChange={() => toggle(scope)}
														className="h-4 w-4 accent-brand-600"
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
						<Field label="Expires" htmlFor="key-until">
							<Input
								id="key-until"
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
						Create key
					</Button>
				</div>
			</section>

			<section>
				<h2 className="text-base font-semibold text-slate-900">This organisation's keys</h2>
				<div className="mt-4">
					{list.isPending ? (
						<PageSpinner />
					) : list.error ? (
						<ErrorBanner message={errMsg(list.error)} />
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
	if (keys.length === 0) return <p className="text-sm text-slate-500">No keys yet.</p>
	return (
		<ul className="divide-y divide-slate-100 rounded-xl border border-slate-200 bg-white">
			{keys.map((k) => (
				<KeyRow key={k.uid} apiKey={k} />
			))}
		</ul>
	)
}

function KeyRow({ apiKey }: { apiKey: ApiKeyView }) {
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
				aria-label="Key name"
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
						.then(() => toast.success('Renamed.'))
						.catch((e) => toast.error(errMsg(e)))
				}
			>
				Save
			</Button>
			<code className="text-xs text-slate-500">{apiKey.prefix}…</code>
			<span className="text-xs text-slate-600">{apiKey.scopes.join(', ')}</span>
			<span className="text-xs text-slate-500">
				{apiKey.lastUsedAt === null ? 'Never used' : `Used ${date(apiKey.lastUsedAt)}`}
			</span>
			{apiKey.expiresAt !== null &&
				(soon ? (
					<Badge tone="warning">Expires {date(apiKey.expiresAt)}</Badge>
				) : (
					<span className="text-xs text-slate-500">Expires {date(apiKey.expiresAt)}</span>
				))}
			<Button
				variant="danger"
				loading={revoke.isPending}
				onClick={() => setConfirmRevoke(true)}
			>
				Revoke
			</Button>
			<ConfirmDialog
				open={confirmRevoke}
				title="Revoke this key"
				description={`Anything still using “${apiKey.name}” stops working at once. This cannot be undone.`}
				confirmLabel="Revoke key"
				loading={revoke.isPending}
				onClose={() => setConfirmRevoke(false)}
				onConfirm={() => {
					setConfirmRevoke(false)
					revoke
						.mutateAsync(apiKey.uid)
						.then(() => toast.success('Key revoked.'))
						.catch((e) => toast.error(errMsg(e)))
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
	const toast = useToast()

	async function copy() {
		try {
			await navigator.clipboard.writeText(minted.key)
			toast.success('Key copied.')
		} catch {
			toast.error('Could not copy — select the key and copy it by hand.')
		}
	}

	return (
		<section className="max-w-2xl rounded-xl border border-slate-200 bg-white p-5">
			<h1 className="text-lg font-semibold text-slate-900">Your new API key</h1>
			<p className="mt-1 text-sm text-slate-600">
				This is the only time it is shown. Store it where the service that uses it can read
				it.
			</p>
			<div className="mt-4 flex flex-wrap items-center gap-2">
				<Input
					readOnly
					aria-label="API key"
					value={minted.key}
					className="min-w-[18rem] flex-1 font-mono"
				/>
				<Button variant="secondary" onClick={() => void copy()}>
					Copy
				</Button>
			</div>
			<p className="mt-4 text-sm text-slate-600">
				<span className="text-slate-500">Name:</span> {minted.name}
				<span className="ml-4 text-slate-500">Scopes:</span> {minted.scopes.join(', ')}
			</p>
			<Button className="mt-6" onClick={onSaved}>
				I've saved it
			</Button>
		</section>
	)
}

// vim: ts=4
