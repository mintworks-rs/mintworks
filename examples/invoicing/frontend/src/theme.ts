import * as React from 'react'

export type Theme = 'system' | 'light' | 'dark'

const STORE_KEY = 'invoicing.theme'

function stored(): Theme {
	try {
		const v = localStorage.getItem(STORE_KEY)
		return v === 'light' || v === 'dark' ? v : 'system'
	} catch {
		return 'system'
	}
}

/**
 * `data-theme` on `<html>`, which `index.css` reads. `system` removes the attribute rather
 * than writing one, so the `prefers-color-scheme` rule is what applies — writing
 * `data-theme="system"` would match neither of the two selectors and freeze the page light.
 */
export function useTheme(): [Theme, (t: Theme) => void] {
	const [theme, setTheme] = React.useState<Theme>(stored)

	React.useEffect(() => {
		if (theme === 'system') delete document.documentElement.dataset.theme
		else document.documentElement.dataset.theme = theme
		try {
			if (theme === 'system') localStorage.removeItem(STORE_KEY)
			else localStorage.setItem(STORE_KEY, theme)
		} catch {
			// A remembered theme is a convenience; blocked storage must not break the toggle.
		}
	}, [theme])

	return [theme, setTheme]
}

// vim: ts=4
