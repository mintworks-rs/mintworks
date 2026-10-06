// SPDX-License-Identifier: MIT-0
// The research routes served by `examples/research/app/main.rn`, behind the app's own
// `/api/app/` prefix. Amounts are integer micro-EUR; timestamps are ISO-8601 strings.

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import { api } from '@mintworks/client'

export interface Project {
	uid: string
	name: string
}

export interface ThreadSummary {
	uid: string
	title: string | null
	/** The project uid, `null` for a thread started without one. */
	project: string | null
	createdAt: string
	updatedAt: string
}

export interface ToolCallSummary {
	name: string
	arguments: string
}

/** Tool-role messages arrive with `content: ""`; only the call's name and arguments are kept. */
export interface ChatMessage {
	role: 'system' | 'user' | 'assistant' | 'tool'
	content: string
	toolCalls: ToolCallSummary[] | null
	createdAt: string
}

export interface Source {
	title: string
	url: string
}

export interface Usage {
	spent: number
	/** `null` while no budget has been set for the org. */
	budget: number | null
}

export interface NotebookEntry {
	path: string
	version: number
	updatedAt: string
}

export interface NotebookDoc {
	version: number
	body: string
	author: string
	createdAt: string
	pdfSha256: string | null
}

/** A notebook space: `global`, a project uid (its notes) or `reports-<project uid>`. */
export type Space = string

export const appUrls = {
	projects: '/api/app/projects',
	threads: '/api/app/threads',
	runs: (thread: string) => `/api/app/threads/${thread}/runs`,
	messages: (thread: string) => `/api/app/threads/${thread}/messages`,
	source: (uid: string) => `/api/app/sources/${uid}`,
	usage: '/api/app/usage',
	notebook: (space: Space) => `/api/app/notebook/${space}`,
	doc: (space: Space, name: string) =>
		`/api/app/notebook/${space}/${encodeURIComponent(name)}`,
	history: (space: Space, name: string) =>
		`/api/app/notebook/${space}/${encodeURIComponent(name)}/history`,
	pdf: (space: Space, name: string) =>
		`/api/app/notebook/${space}/${encodeURIComponent(name)}/pdf`
}

export const researchKeys = {
	projects: ['projects'] as const,
	threads: ['threads'] as const,
	messages: (thread: string) => ['threads', thread, 'messages'] as const,
	source: (uid: string) => ['sources', uid] as const,
	usage: ['usage'] as const,
	/** Every space's queries share this prefix, so invalidating it refetches them all. */
	notebooks: ['notebook'] as const,
	notebook: (space: Space) => ['notebook', space] as const,
	doc: (space: Space, name: string, version?: number) =>
		['notebook', space, name, version ?? null] as const,
	history: (space: Space, name: string) => ['notebook', space, name, 'history'] as const
}

export const useProjects = () =>
	useQuery({
		queryKey: researchKeys.projects,
		queryFn: ({ signal }) => api.get<Project[]>(appUrls.projects, signal)
	})

export function useCreateProject() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (name: string) => api.post<Project>(appUrls.projects, { name }),
		onSuccess: () => qc.invalidateQueries({ queryKey: researchKeys.projects })
	})
}

export const useThreads = () =>
	useQuery({
		queryKey: researchKeys.threads,
		queryFn: ({ signal }) => api.get<ThreadSummary[]>(appUrls.threads, signal)
	})

export const useMessages = (thread: string) =>
	useQuery({
		queryKey: researchKeys.messages(thread),
		queryFn: ({ signal }) => api.get<ChatMessage[]>(appUrls.messages(thread), signal),
		enabled: thread !== ''
	})

/** A source never changes once stored, so it is cached for the session. */
export const useSource = (uid: string) =>
	useQuery({
		queryKey: researchKeys.source(uid),
		queryFn: ({ signal }) => api.get<Source>(appUrls.source(uid), signal),
		enabled: uid !== '',
		staleTime: Number.POSITIVE_INFINITY
	})

export const useUsage = () =>
	useQuery({
		queryKey: researchKeys.usage,
		queryFn: ({ signal }) => api.get<Usage>(appUrls.usage, signal)
	})

export const useNotebook = (space: Space) =>
	useQuery({
		queryKey: researchKeys.notebook(space),
		queryFn: ({ signal }) => api.get<NotebookEntry[]>(appUrls.notebook(space), signal)
	})

export const useNotebookDoc = (space: Space, name: string, version?: number) =>
	useQuery({
		queryKey: researchKeys.doc(space, name, version),
		queryFn: ({ signal }) =>
			api.get<NotebookDoc>(
				version === undefined
					? appUrls.doc(space, name)
					: `${appUrls.doc(space, name)}?version=${version}`,
				signal
			),
		enabled: name !== ''
	})

/** Oldest first. */
export const useNotebookHistory = (space: Space, name: string) =>
	useQuery({
		queryKey: researchKeys.history(space, name),
		queryFn: ({ signal }) => api.get<NotebookDoc[]>(appUrls.history(space, name), signal),
		enabled: name !== ''
	})

/** Starts a thread and its first run; the thread list and the budget line both move. */
export function useStartThread() {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: ({ question, project }: { question: string; project: string | null }) =>
			api.post<{ thread: string; run: string }>(appUrls.threads, { question, project }),
		onSuccess: () =>
			Promise.all([
				qc.invalidateQueries({ queryKey: researchKeys.threads }),
				qc.invalidateQueries({ queryKey: researchKeys.usage })
			])
	})
}

export function useAsk(thread: string) {
	const qc = useQueryClient()
	return useMutation({
		mutationFn: (input: string) => api.post<{ run: string }>(appUrls.runs(thread), { input }),
		onSuccess: () => qc.invalidateQueries({ queryKey: researchKeys.threads })
	})
}

/** `GET /api/documents/{uid}` (the `pdf.documents` mount). */
export interface DocStatus {
	uid: string
	status: 'PENDING' | 'READY' | 'FAILED'
	sha256: string | null
	bytes: number | null
	createdAt: string
}

export const pdfUrl = (uid: string) => `/api/documents/${uid}/pdf`

/** Polls once a second while the render job is pending. */
export const useDocument = (uid: string) =>
	useQuery({
		queryKey: ['documents', uid] as const,
		queryFn: ({ signal }) => api.get<DocStatus>(`/api/documents/${uid}`, signal),
		enabled: uid !== '',
		refetchInterval: (q) => (q.state.data?.status === 'PENDING' ? 1000 : false)
	})

/** Answers the `documents` uid; poll `GET /api/documents/{uid}` until the PDF is ready. */
export const useExportPdf = (space: Space) =>
	useMutation({
		mutationFn: (name: string) =>
			api.post<{ uid: string; markdown: string }>(appUrls.pdf(space, name))
	})

// vim: ts=4
