// SPDX-License-Identifier: MIT-0
import { Link } from 'react-router-dom'

import { AuthCard } from '~/components/ui'
import { useT } from '~/i18n'

export function CheckEmail() {
	const { t } = useT()
	return (
		<AuthCard title={t('auth.checkEmail')} subtitle={t('auth.checkEmail.subtitle')}>
			<p className="text-sm text-fg-muted">{t('auth.checkEmail.body')}</p>
			<p className="mt-6 text-sm text-fg-muted">
				<Link to="/login" className="text-accent hover:underline">
					{t('auth.backToSignIn')}
				</Link>
			</p>
		</AuthCard>
	)
}

// vim: ts=4
