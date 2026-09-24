// The draft lifecycle, behind this application's own `/api/app/` prefix: the mounted
// `invoice.org_read` bundle owns `/api/invoices`, and a duplicate axum path panics at startup,
// so the writes are wrapped in `examples/invoicing/script/invoices.rn` and built from `api.*`
// here rather than taken from the SDK, which ships framework routes only.

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import type { InvoiceSummary, InvoiceView, Page } from '@saas-framework/client'
import { ServerError, api, keys } from '@saas-framework/client'

export const appUrls = {
	invoices: '/api/app/invoices',
	invoice: (uid: string) => `/api/app/invoices/${uid}`,
	lines: (uid: string) => `/api/app/invoices/${uid}/lines`,
	line: (uid: string, lineNo: number) => `/api/app/invoices/${uid}/lines/${lineNo}`,
	issue: (uid: string) => `/api/app/invoices/${uid}/issue`,
	storno: (uid: string) => `/api/app/invoices/${uid}/storno`,
	paid: (uid: string) => `/api/app/invoices/${uid}/paid`,
	summary: '/api/app/summary',
	taxLimits: '/api/app/tax-limits',
	projects: '/api/app/projects',
	project: (uid: string) => `/api/app/projects/${uid}`,
	projectInvoices: (uid: string) => `/api/app/projects/${uid}/invoices`,
	invoiceProject: (uid: string) => `/api/app/invoices/${uid}/project`
}

/** Every write invalidates `keys.invoices` and nothing narrower: `keys.invoice(uid)` sits
 *  under it, so the list and every open detail refetch from one call. */
function useInvoiceWrite<V, R>(mutationFn: (v: V) => Promise<R>) {
	const qc = useQueryClient()
	return useMutation({
		mutationFn,
		onSuccess: () => qc.invalidateQueries({ queryKey: keys.invoices })
	})
}

/** `lines` is required even when it is empty — `new_draft` reads it with `required()`. */
export const useCreateDraft = () =>
	useInvoiceWrite((body: Record<string, unknown>) =>
		api.post<InvoiceView>(appUrls.invoices, body)
	)

export const usePatchInvoice = (uid: string) =>
	useInvoiceWrite((body: Record<string, unknown>) =>
		api.patch<InvoiceView>(appUrls.invoice(uid), body)
	)

export const useDeleteDraft = () =>
	useInvoiceWrite((uid: string) => api.delete<null>(appUrls.invoice(uid)))

export const useAddLine = (uid: string) =>
	useInvoiceWrite((line: Record<string, unknown>) =>
		api.post<InvoiceView>(appUrls.lines(uid), line)
	)

/** An absent key stays absent rather than clearing the field (`invoices.rn::line_patch`), and
 *  `unit` is not patchable at all. */
export const useEditLine = (uid: string) =>
	useInvoiceWrite((d: { lineNo: number; patch: Record<string, unknown> }) =>
		api.patch<InvoiceView>(appUrls.line(uid, d.lineNo), d.patch)
	)

export const useRemoveLine = (uid: string) =>
	useInvoiceWrite((lineNo: number) => api.delete<InvoiceView>(appUrls.line(uid, lineNo)))

/** The last write an invoice takes: the number is allocated inside the issue transaction and
 *  the row is immutable afterwards. */
export const useIssueInvoice = () =>
	useInvoiceWrite((uid: string) => api.post<InvoiceView>(appUrls.issue(uid)))

export const useStornoInvoice = () =>
	useInvoiceWrite((d: { uid: string; reason: string }) =>
		api.post<InvoiceView>(appUrls.storno(d.uid), { reason: d.reason })
	)

/** A bank transfer received in full on `paidOn`. It moves the dashboard figures too, so both
 *  aggregates refetch. */
export function useMarkPaid() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (d: { uid: string; paidOn: string }) =>
			api.post<InvoiceView>(appUrls.paid(d.uid), { paidOn: d.paidOn }),
		onSuccess: () =>
			Promise.all([
				qc.invalidateQueries({ queryKey: keys.invoices }),
				qc.invalidateQueries({ queryKey: ['summary'] }),
				qc.invalidateQueries({ queryKey: ['tax-limits'] })
			])
	})
}

export function useSummary(months = 12) {
	return useQuery({
		queryKey: ['summary', months] as const,
		queryFn: ({ signal }) =>
			api.get<InvoiceSummary>(`${appUrls.summary}?months=${months}`, signal)
	})
}

// ---------------------------------------------------------------- tax limits
//
// `examples/invoicing/script/taxlimits.rn` is the shape. Every amount is whole forints.

export type LimitKind = 'AAM' | 'KATA' | 'ATALANY' | 'ATALANY_TAXFREE'

export interface TaxLimit {
	kind: LimitKind
	basis: 'INVOICED' | 'RECEIVED'
	/** Pro-rated when `since` is set; `null` when the year's figure is not published. */
	limit: number | null
	fullLimit: number | null
	since: string | null
	used: number
	/** Issued, not yet paid, this year — `0` on an `INVOICED` basis. Shown, not counted. */
	pending: number
	/** `'YYYY-MM'`, the first month the running total passed `limit`. */
	crossedOn: string | null
	costPct?: number | null
	taxFreeIncome?: number | null
}

export interface TaxLimits {
	year: number
	today: string
	/** Running totals, January first. */
	months: { month: string; invoiced: number; received: number; outstanding: number }[]
	limits: TaxLimit[]
}

export function useTaxLimits(year?: number) {
	return useQuery({
		queryKey: ['tax-limits', year ?? null] as const,
		queryFn: ({ signal }) =>
			api.get<TaxLimits>(
				year === undefined ? appUrls.taxLimits : `${appUrls.taxLimits}?year=${year}`,
				signal
			)
	})
}

// ---------------------------------------------------------------- projects
//
// A project is a script-declared object type, so the wire shape is the object store's
// envelope (`crates/saas-script/src/objects.rs::object_json`), not a framework view.

export type ProjectStatus = 'OPEN' | 'CLOSED'

export interface ProjectBody {
	name: string
	partyUid: string
	status: ProjectStatus
	notes?: string
}

export interface ProjectObject {
	uid: string
	body: ProjectBody
}

export const projectKeys = {
	all: ['projects'] as const,
	one: (uid: string) => ['projects', uid] as const,
	invoices: (uid: string) => ['projects', uid, 'invoices'] as const,
	ofInvoice: (uid: string) => ['projects', 'of-invoice', uid] as const
}

/** Every project write invalidates the whole subtree: an assignment changes both a project's
 *  invoice list and an invoice's project, and both sit under `projectKeys.all`. */
function useProjectWrite<V, R>(mutationFn: (v: V) => Promise<R>) {
	const qc = useQueryClient()
	return useMutation({
		mutationFn,
		onSuccess: () => qc.invalidateQueries({ queryKey: projectKeys.all })
	})
}

export const useProjects = () =>
	useQuery({
		queryKey: projectKeys.all,
		queryFn: ({ signal }) => api.get<Page<ProjectObject>>(appUrls.projects, signal)
	})

export const useProject = (uid: string) =>
	useQuery({
		queryKey: projectKeys.one(uid),
		queryFn: ({ signal }) => api.get<ProjectObject>(appUrls.project(uid), signal),
		enabled: uid !== ''
	})

/** One index lookup plus one fetch per invoice server-side, capped at 50 rows
 *  (`projects.rn::project_invoices`); `nextCursor` is not followed, the page says so instead. */
export const useProjectInvoices = (uid: string) =>
	useQuery({
		queryKey: projectKeys.invoices(uid),
		queryFn: ({ signal }) => api.get<Page<InvoiceView>>(appUrls.projectInvoices(uid), signal),
		enabled: uid !== ''
	})

export const useSaveProject = (uid: string | null) =>
	useProjectWrite((body: ProjectBody) =>
		uid === null
			? api.post<ProjectObject>(appUrls.projects, body)
			: api.patch<ProjectObject>(appUrls.project(uid), body)
	)

export const useDeleteProject = () =>
	useProjectWrite((uid: string) => api.delete<null>(appUrls.project(uid)))

/** A 404 is "not assigned", not a failure: the `invoice.ext` row exists only once something
 *  writes it, and `projects.rn::inv_get_project` has nothing else to answer with. */
export function useInvoiceProject(uid: string) {
	return useQuery({
		queryKey: projectKeys.ofInvoice(uid),
		queryFn: ({ signal }) =>
			api
				.get<{ projectUid: string }>(appUrls.invoiceProject(uid), signal)
				.catch((e: unknown) => {
					if (e instanceof ServerError && e.httpStatus === 404) return null
					throw e
				}),
		enabled: uid !== ''
	})
}

/** Legal on an `ISSUED` invoice: the link is an ext row beside the invoice, not a column on
 *  it, so the immutability rule does not reach it. */
export const useSetInvoiceProject = (uid: string) =>
	useProjectWrite((projectUid: string) =>
		api.put<{ projectUid: string }>(appUrls.invoiceProject(uid), { projectUid })
	)

// vim: ts=4
