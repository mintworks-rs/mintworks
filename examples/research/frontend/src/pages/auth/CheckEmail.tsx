// SPDX-License-Identifier: MIT-0
import { Link } from 'react-router-dom'

import { AuthCard } from '~/components/ui'

export function CheckEmail() {
	return (
		<AuthCard title="Check your email" subtitle="Registration accepted.">
			<p className="text-sm text-fg-muted">
				We sent an activation link. Open it to set your password and finish signing up.
			</p>
			<p className="mt-6 text-sm text-fg-muted">
				<Link to="/login" className="text-accent hover:underline">
					Back to sign in
				</Link>
			</p>
		</AuthCard>
	)
}

// vim: ts=4
