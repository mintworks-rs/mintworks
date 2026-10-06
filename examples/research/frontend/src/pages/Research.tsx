import { useEffect, useState } from 'react'
import { useParams } from 'react-router-dom'

import { useAuth } from '@mintworks/client'
import { useThreads } from '~/api/hooks'
import { Chat } from '~/components/Chat'
import { Notebook } from '~/components/Notebook'
import { ThreadList } from '~/components/ThreadList'
import { Usage } from '~/components/Usage'
import { Button } from '~/components/ui'

type Drawer = 'threads' | 'notebook' | null

// Below 900px a side pane is a fixed drawer over the chat; at 900px and up it sits in the grid.
const pane =
	'fixed inset-y-0 z-30 w-[min(20rem,85vw)] bg-surface-raised shadow-lg transition-transform ' +
	'motion-reduce:transition-none min-[900px]:static min-[900px]:z-auto min-[900px]:w-auto ' +
	'min-[900px]:translate-x-0 min-[900px]:shadow-none min-h-0'

const PROJECT_KEY = 'research.project'

function storedProject(): string | null {
	try {
		return localStorage.getItem(PROJECT_KEY)
	} catch {
		return null
	}
}

export function Research() {
	const { thread } = useParams()
	const { logout } = useAuth()
	const [drawer, setDrawer] = useState<Drawer>(null)
	const [project, setProject] = useState<string | null>(storedProject)
	const threadProject = useThreads().data?.find((t) => t.uid === thread)?.project

	// Opening a thread switches to its project; `undefined` is "not loaded", not "no project".
	useEffect(() => {
		if (threadProject !== undefined) setProject(threadProject)
	}, [threadProject])

	useEffect(() => {
		try {
			if (project) localStorage.setItem(PROJECT_KEY, project)
			else localStorage.removeItem(PROJECT_KEY)
		} catch {}
	}, [project])

	useEffect(() => {
		if (!drawer) return
		const onKey = (e: KeyboardEvent) => e.key === 'Escape' && setDrawer(null)
		window.addEventListener('keydown', onKey)
		return () => window.removeEventListener('keydown', onKey)
	}, [drawer])

	return (
		<div className="flex h-full flex-col">
			<header className="flex items-center gap-2 border-b border-line bg-surface-raised px-3 py-2">
				<Button
					variant="ghost"
					className="min-[900px]:hidden"
					aria-label="Threads"
					aria-expanded={drawer === 'threads'}
					onClick={() => setDrawer(drawer === 'threads' ? null : 'threads')}
				>
					☰
				</Button>
				<h1 className="text-base font-semibold">Research</h1>
				<div className="ml-auto flex items-center gap-2">
					<Usage />
					<Button
						variant="ghost"
						className="min-[900px]:hidden"
						aria-expanded={drawer === 'notebook'}
						onClick={() => setDrawer(drawer === 'notebook' ? null : 'notebook')}
					>
						Notebook
					</Button>
					<Button variant="ghost" onClick={() => void logout()}>
						Sign out
					</Button>
				</div>
			</header>
			<div className="relative grid min-h-0 flex-1 min-[900px]:grid-cols-[16rem_1fr_22rem]">
				{drawer && (
					<button
						type="button"
						aria-label="Close panel"
						className="fixed inset-0 z-20 bg-black/40 min-[900px]:hidden"
						onClick={() => setDrawer(null)}
					/>
				)}
				<aside
					className={`${pane} left-0 border-r border-line ${drawer === 'threads' ? 'translate-x-0' : '-translate-x-full'}`}
				>
					<ThreadList
						active={thread}
						project={project}
						onProject={setProject}
						onNavigate={() => setDrawer(null)}
					/>
				</aside>
				<main className="min-h-0 min-w-0">
					<Chat key={thread ?? 'new'} thread={thread} project={project} />
				</main>
				<aside
					className={`${pane} right-0 border-l border-line ${drawer === 'notebook' ? 'translate-x-0' : 'translate-x-full'}`}
				>
					<Notebook project={project} />
				</aside>
			</div>
		</div>
	)
}

// vim: ts=4
