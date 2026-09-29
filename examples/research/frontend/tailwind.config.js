/** Content globs only. Every colour is a CSS custom property declared in `src/index.css`
 *  and mapped through Tailwind 4's `@theme inline`, so dark mode is one redefinition
 *  rather than a second palette here. */
export default {
	content: ['./src/**/*.{ts,tsx}'],
	plugins: []
}

// vim: ts=4
