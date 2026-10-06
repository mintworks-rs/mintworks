// SPDX-License-Identifier: MIT-0
import type * as React from 'react'

import { ProtectedRoute as Gate } from '@mintworks/client'

import { Button, ErrorBanner, PageSpinner } from '~/components/ui'
import { useT } from '~/i18n'
import { ConsentRequired } from '~/pages/ConsentRequired'

// The package ships the branching, not the markup. This is the app's half: what each of
// the gate's three non-authenticated states looks like here.
export function ProtectedRoute({ children }: { children: React.ReactNode }) {
	const { t } = useT()
	return (
		<Gate
			fallback={<PageSpinner />}
			onError={(error, retry) => (
				<div className="mx-auto mt-16 max-w-sm space-y-3 px-4">
					<ErrorBanner message={error} />
					<Button onClick={retry}>{t('common.retry')}</Button>
				</div>
			)}
			consentGate={(kinds) => <ConsentRequired kinds={kinds} />}
		>
			{children}
		</Gate>
	)
}

// vim: ts=4
