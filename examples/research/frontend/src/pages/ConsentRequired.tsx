import { useQuery } from '@tanstack/react-query'
import * as React from 'react'

import type { LegalDoc, LegalKind } from '@mintworks/client'
import {
	ERRORS_EN,
	ServerError,
	api,
	errText,
	useAuth,
	useRecordConsent
} from '@mintworks/client'
import { AuthCard, Button, ErrorBanner, Modal, PageSpinner } from '~/components/ui'

/**
 * The way out of `E-AUTH-CONSENT-REQUIRED`: bumping a legal version re-gates every account,
 * and without this screen a returning user has no path forward.
 */
export function ConsentRequired({ kinds }: { kinds: LegalKind[] }) {
	const { logout, reload } = useAuth()
	const record = useRecordConsent()
	const [checked, setChecked] = React.useState<Record<string, boolean>>({})
	const [reading, setReading] = React.useState<LegalDoc | null>(null)
	const [error, setError] = React.useState<string | null>(null)
	const [busy, setBusy] = React.useState(false)

	const docs = useQuery({
		queryKey: ['legal', kinds],
		queryFn: ({ signal }) =>
			Promise.all(kinds.map((k) => api.get<LegalDoc>(`/api/legal/${k}`, signal)))
	})

	const all = docs.data?.every((d) => checked[d.kind]) ?? false

	async function submit(ev: React.FormEvent) {
		ev.preventDefault()
		if (!docs.data) return
		setError(null)
		setBusy(true)
		try {
			// The server refuses a superseded version with `E-CORE-CONFLICT`.
			for (const d of docs.data) {
				await record.mutateAsync({ kind: d.kind, version: d.version, docSha256: d.sha256 })
			}
			await reload()
		} catch (e) {
			// Refetched, or a retry resubmits the same superseded versions forever.
			void docs.refetch()
			setChecked({})
			setError(
				e instanceof ServerError && e.errCode === 'E-CORE-CONFLICT'
					? 'These documents changed while you were reading them; they have been reloaded. Please read and accept the current versions.'
					: errText(e, ERRORS_EN)
			)
		} finally {
			setBusy(false)
		}
	}

	if (docs.isPending) return <PageSpinner />
	if (docs.error) {
		return (
			<AuthCard title="Before you continue">
				<ErrorBanner message={errText(docs.error, ERRORS_EN)} />
			</AuthCard>
		)
	}

	return (
		<AuthCard
			title="Before you continue"
			subtitle="These documents have changed since you last accepted them."
		>
			<form onSubmit={submit} className="flex flex-col gap-4">
				<ErrorBanner message={error} />
				{docs.data.map((d) => (
					<div key={d.kind} className="flex items-start gap-2 text-sm text-fg">
						<label className="flex items-start gap-2">
							<input
								type="checkbox"
								checked={checked[d.kind] ?? false}
								onChange={(e) =>
									setChecked((c) => ({ ...c, [d.kind]: e.target.checked }))
								}
								className="mt-1 h-4 w-4 accent-accent"
								required
							/>
							<span>
								I accept the {d.title}{' '}
								<span className="text-fg-muted">(v{d.version})</span>
							</span>
						</label>
						{/* Beside the label, never inside it: a nested button steals its clicks. */}
						<button
							type="button"
							onClick={() => setReading(d)}
							className="mt-0.5 shrink-0 text-accent hover:underline"
						>
							Read
						</button>
					</div>
				))}
				<Button type="submit" loading={busy} disabled={!all}>
					Accept and continue
				</Button>
			</form>

			{/* The decline path: this screen replaces the whole app, so it needs its own sign-out. */}
			<p className="mt-6 text-sm text-fg-muted">
				Not willing to accept?{' '}
				<button
					type="button"
					onClick={() => void logout()}
					className="text-accent hover:underline"
				>
					Sign out
				</button>
			</p>

			<Modal
				open={reading !== null}
				onClose={() => setReading(null)}
				title={reading ? `${reading.title} (v${reading.version})` : ''}
			>
				<div className="max-h-[60vh] overflow-y-auto whitespace-pre-wrap text-sm text-fg">
					{reading?.body}
				</div>
				<div className="mt-6 flex justify-end">
					<Button variant="secondary" onClick={() => setReading(null)}>
						Close
					</Button>
				</div>
			</Modal>
		</AuthCard>
	)
}

// vim: ts=4
