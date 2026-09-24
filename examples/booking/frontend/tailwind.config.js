/** @type {import('tailwindcss').Config} */
export default {
	content: ['./src/**/*.{ts,tsx}'],
	theme: {
		extend: {
			colors: {
				brand: {
					50: '#eef5ff',
					100: '#d9e8ff',
					500: '#2563eb',
					600: '#1d4ed8',
					700: '#1e40af'
				}
			}
		}
	},
	plugins: []
}
