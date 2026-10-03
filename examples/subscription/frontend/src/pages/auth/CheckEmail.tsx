import { Link } from 'react-router-dom'

import { AuthCard } from '~/components/ui'

export function CheckEmail() {
	return (
		<AuthCard title="Check your email" subtitle="Registration accepted.">
			<p className="text-sm text-slate-600">
				We sent an activation link. Open it to set your password and finish signing up.
			</p>
			<p className="mt-6 text-sm text-slate-500">
				<Link to="/login" className="text-brand-700 hover:underline">
					Back to sign in
				</Link>
			</p>
		</AuthCard>
	)
}

// vim: ts=4
