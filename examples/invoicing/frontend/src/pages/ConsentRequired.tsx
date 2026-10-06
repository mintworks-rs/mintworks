import { useQuery } from '@tanstack/react-query'
import * as React from 'react'

import type { LegalDoc, LegalKind } from '@mintworks/client'
import { ServerError, api, useAuth, useRecordConsent } from '@mintworks/client'
import { Modal } from '~/components/Modal'
import { AuthCard, Button, ErrorBanner, PageSpinner } from '~/components/ui'
import { useT } from '~/i18n'

/**
 * The way out of `E-AUTH-CONSENT-REQUIRED`. Bumping the legal version re-gates every account,
 * and the backend gates every application route, so without this screen a returning user logs
 * in and then sees an error banner on every tab with no path forward.
 *
 * `GET`/`POST /api/consents` and `/api/legal/{kind}` are all reachable while gated
 * (`crates/auth/src/routes.rs::consent_exempt`).
 */
export function ConsentRequired({ kinds }: { kinds: LegalKind[] }) {
	const { t, err } = useT()
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
			// One grant per owed kind, about the exact version and hash presented above — the
			// server refuses a superseded version with `E-CORE-CONFLICT`.
			for (const d of docs.data) {
				await record.mutateAsync({ kind: d.kind, version: d.version, docSha256: d.sha256 })
			}
			await reload()
		} catch (e) {
			// Refetched, or a retry resubmits the same superseded versions forever. The grants
			// already recorded are idempotent per kind, so retrying on fresh ones is safe.
			void docs.refetch()
			setChecked({})
			setError(
				e instanceof ServerError && e.errCode === 'E-CORE-CONFLICT'
					? t('consent.conflict')
					: err(e)
			)
		} finally {
			setBusy(false)
		}
	}

	if (docs.isPending) return <PageSpinner />
	if (docs.error) {
		return (
			<AuthCard title={t('consent.title')}>
				<ErrorBanner message={err(docs.error)} />
			</AuthCard>
		)
	}

	return (
		<AuthCard title={t('consent.title')} subtitle={t('consent.subtitle')}>
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
								{t('consent.accept', { title: d.title })}{' '}
								<span className="text-fg-muted">(v{d.version})</span>
							</span>
						</label>
						{/* Beside the label, never inside it: a button nested in a <label> is
						    activated by clicks meant for the checkbox. */}
						<button
							type="button"
							onClick={() => setReading(d)}
							className="mt-0.5 shrink-0 text-accent hover:underline"
						>
							{t('common.read')}
						</button>
					</div>
				))}
				<Button type="submit" loading={busy} disabled={!all}>
					{t('consent.submit')}
				</Button>
			</form>

			{/* The decline path. This screen replaces the whole shell, so without it a user who
			    will not accept is locked into a session with no sign-out. */}
			<p className="mt-6 text-sm text-fg-muted">
				{t('consent.decline')}{' '}
				<button
					type="button"
					onClick={() => void logout()}
					className="text-accent hover:underline"
				>
					{t('shell.signOut')}
				</button>
			</p>

			<Modal
				open={reading !== null}
				onClose={() => setReading(null)}
				title={reading ? `${reading.title} (v${reading.version})` : ''}
			>
				{/* Markdown from the server rendered as text: the requirement is the verbatim
				    wording, which neither needs a Markdown dependency nor raw HTML. */}
				<div className="max-h-[60vh] overflow-y-auto whitespace-pre-wrap text-sm text-fg">
					{reading?.body}
				</div>
				<div className="mt-6 flex justify-end">
					<Button variant="secondary" onClick={() => setReading(null)}>
						{t('common.close')}
					</Button>
				</div>
			</Modal>
		</AuthCard>
	)
}

// vim: ts=4
