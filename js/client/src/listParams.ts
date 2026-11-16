// SPDX-License-Identifier: MPL-2.0
import { useEffect, useRef } from 'react'
import { useSearchParams } from 'react-router-dom'

/** A list footer: 1-based `from`/`to` rows of `total`; a missing handler is a disabled button. */
export interface ListPagination {
	from: number
	to: number
	total?: number
	onPrev?: () => void
	onNext?: () => void
}

/** Page keys this hook owns; every other URL key is a filter. */
const OWN = new Set(['sort', 'page'])

/** The URL's filter keys (every key but `sort`/`page`), for spotting a filter change. */
export function filterKey(p: URLSearchParams): string {
	return [...p]
		.filter(([k]) => !OWN.has(k))
		.map(([k, v]) => `${k}=${v}`)
		.join('&')
}

/** The 1-based `?page=`; anything not a positive number is page 1. */
export function pageOf(p: URLSearchParams): number {
	return Math.max(1, Math.floor(Number(p.get('page'))) || 1)
}

/** The footer of `page` holding `count` rows of `total`; `goTo` takes a 1-based page. */
export function footer(
	page: number,
	limit: number,
	count: number,
	total: number | undefined,
	goTo: (n: number) => void
): ListPagination {
	const offset = (page - 1) * limit
	return {
		from: count ? offset + 1 : 0,
		to: offset + count,
		total,
		onPrev: page > 1 ? () => goTo(page - 1) : undefined,
		onNext: total !== undefined && offset + count < total ? () => goTo(page + 1) : undefined
	}
}

/** The page to fetch: 1 already on the render a filter changed away from `seen`, before the
 * effect drops `page` from the URL, so the stale page is never fetched. */
export function pageSince(params: URLSearchParams, seen: string): number {
	return filterKey(params) === seen ? pageOf(params) : 1
}

/**
 * Sort and page of a server-paged list, in the URL (`?sort=-issueDate&page=2`, 1-based) so a
 * reload or a shared link keeps them. Any other key changing (a filter chip, the `q` box) goes
 * back to page 1; a reload with `?page=3` does not.
 */
export function useListParams(defaultSort = '', limit = 50) {
	const [params, setParams] = useSearchParams()
	const sort = params.get('sort') ?? defaultSort
	const filters = filterKey(params)
	const seen = useRef(filters)
	const page = pageSince(params, seen.current)
	const offset = (page - 1) * limit

	const set = (k: string, v: string | null, replace = k !== 'page') =>
		setParams(
			(p) => {
				const n = new URLSearchParams(p)
				if (v === null) n.delete(k)
				else n.set(k, v)
				if (k === 'sort') n.delete('page')
				return n
			},
			{ replace }
		)

	// Only a filter change resets the page, replacing the entry the filter change pushed.
	useEffect(() => {
		if (seen.current === filters) return
		seen.current = filters
		if (params.has('page')) set('page', null, true)
	}, [filters])

	const goTo = (n: number) => set('page', n > 1 ? String(n) : null)

	return {
		sort,
		setSort: (s: string) => set('sort', s === defaultSort ? null : s),
		offset,
		limit,
		/** The footer for a page holding `count` rows of `total`. */
		pagination: (count: number, total?: number): ListPagination =>
			footer(page, limit, count, total, goTo)
	}
}

// vim: ts=4
