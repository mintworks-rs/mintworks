import * as React from 'react'

import type { ErrorDict, MoneyWire } from '@mintworks/client'
import {
	ERRORS_EN,
	ERRORS_HU,
	errText,
	fieldErrors,
	formatMoney,
	formatQty
} from '@mintworks/client'

import { type Key, en } from './en'
import { hu } from './hu'

export type Locale = 'en' | 'hu'
export type { Key }
/** A `tn` base: the key with `.one`/`.other` stripped. */
export type PluralKey = Base<Key>
type Base<K> = K extends `${infer B}.other` ? B : never

const DICTS: Record<Locale, Record<Key, string>> = { en, hu }
const ERRORS: Record<Locale, ErrorDict> = { en: ERRORS_EN, hu: ERRORS_HU }
/** The dictionary key is a language; `Intl` wants a region to pick separators and date order. */
const TAGS: Record<Locale, string> = { en: 'en-GB', hu: 'hu-HU' }
const STORE_KEY = 'invoicing.locale'

interface I18n {
	locale: Locale
	tag: string
	setLocale: (l: Locale) => void
	t: (key: Key, vars?: Record<string, string | number>) => string
	/** `key.one` / `key.other`, picked by `Intl.PluralRules`. `{n}` is filled in. */
	tn: (key: PluralKey, n: number) => string
	/** `E-*` prose, from the SDK's dictionary for the active locale. */
	err: (e: unknown) => string
	/** Per-field `E-*` codes off `E-CORE-VALIDATION`, mapped through the same dictionary. */
	fields: (e: unknown) => Record<string, string>
	money: (m: MoneyWire | null | undefined) => string
	qty: (decimal: string) => string
	date: (iso: string | null | undefined) => string
	dateTime: (iso: string | null | undefined) => string
}

const Ctx = React.createContext<I18n | null>(null)

function stored(): Locale | null {
	try {
		const v = localStorage.getItem(STORE_KEY)
		return v === 'en' || v === 'hu' ? v : null
	} catch {
		// Private mode, blocked site data: a remembered language is a convenience, never a
		// reason to fail to render.
		return null
	}
}

function fill(s: string, vars?: Record<string, string | number>): string {
	if (!vars) return s
	return s.replace(/\{(\w+)\}/g, (m, k) => (k in vars ? String(vars[k]) : m))
}

export function I18nProvider({ children }: { children: React.ReactNode }) {
	const [locale, setLocaleState] = React.useState<Locale>(
		() => stored() ?? (navigator.language.startsWith('hu') ? 'hu' : 'en')
	)

	// The document's own language, not just React's: it is what a screen reader switches
	// voices on and what `:lang()` and hyphenation read.
	React.useEffect(() => {
		document.documentElement.lang = locale
	}, [locale])

	const value = React.useMemo<I18n>(() => {
		const tag = TAGS[locale]
		const dict = DICTS[locale]
		const errors = ERRORS[locale]
		const dateFmt = new Intl.DateTimeFormat(tag, { dateStyle: 'medium' })
		const dateTimeFmt = new Intl.DateTimeFormat(tag, {
			dateStyle: 'medium',
			timeStyle: 'short'
		})
		const plural = new Intl.PluralRules(tag)

		return {
			locale,
			tag,
			setLocale: (l) => {
				setLocaleState(l)
				try {
					localStorage.setItem(STORE_KEY, l)
				} catch {
					// See `stored()`.
				}
			},
			// `?? key` for a key cast from a wire value the dictionary does not know yet.
			t: (key, vars) => fill(dict[key] ?? key, vars),
			tn: (key, n) => {
				const cat = plural.select(n)
				// `one` is absent from some keys; `other` is always there, which `PluralKey` guarantees.
				const s = (dict as Record<string, string>)[`${key}.${cat}`] ?? dict[`${key}.other`]
				return fill(s, { n })
			},
			err: (e) => errText(e, errors),
			fields: (e) => fieldErrors(e, errors),
			money: (m) => formatMoney(m, tag),
			qty: (decimal) => formatQty(decimal, tag),
			date: (iso) => (iso ? dateFmt.format(new Date(iso)) : '—'),
			dateTime: (iso) => (iso ? dateTimeFmt.format(new Date(iso)) : '—')
		}
	}, [locale])

	return <Ctx.Provider value={value}>{children}</Ctx.Provider>
}

export function useT(): I18n {
	const v = React.useContext(Ctx)
	if (!v) throw new Error('useT used outside I18nProvider')
	return v
}

/** Endonyms, never flags: a flag is a country and these are languages. */
export const LOCALE_NAMES: Record<Locale, string> = { en: 'English', hu: 'Magyar' }

// vim: ts=4
