// errCode → prose for the framework's codes: its five prefixes plus `E-SCRIPT-*`; the app owns
// every other string it shows.
//
// Codes that never reach a browser are left out on purpose: `E-EMAIL-*` and `E-CORE-JOB-POISON`
// are `jobs.err_code` values, and the `nav_submissions.error_code` markers are not errCodes.

import type { ErrorDict } from './index'

export const ERRORS_EN: ErrorDict = {
	// --- E-CORE-* ---
	'E-CORE-VALIDATION': 'Please check the highlighted fields.',
	'E-CORE-FORMAT': 'Not a valid value.',
	'E-CORE-RANGE': 'Outside the permitted range.',
	'E-CORE-NOTFOUND': 'Not found.',
	'E-CORE-CONFLICT': 'That conflicts with something that already exists.',
	'E-CORE-RATELIMIT': 'Too many attempts. Please wait a moment and try again.',
	'E-CORE-POW': 'Verification expired. Please try again.',
	'E-CORE-UNSUPPORTED': 'Unsupported value or content type.',
	'E-CORE-INTERNAL': 'Something went wrong on our side.',
	'E-CORE-UNAVAILABLE': 'The service is temporarily unavailable. Please try again.',
	'E-CORE-TIMEOUT': 'The request timed out. It may still have been processed.',
	'E-CORE-SETTING': 'Unknown setting, or a value that setting does not accept.',

	// --- E-AUTH-* ---
	'E-AUTH-CREDENTIALS': 'Incorrect email address or password.',
	'E-AUTH-TOKEN': 'Your session has expired. Please sign in again.',
	'E-AUTH-STEPUP': 'Please confirm your password to continue.',
	'E-AUTH-STEPUP-IMPOSSIBLE': 'This action needs a password, which this sign-in method has none.',
	'E-AUTH-RETRY': 'Your session was renewed. Please try that again.',
	'E-AUTH-WEBAUTHN': 'The passkey was not accepted.',
	'E-AUTH-CHALLENGE': 'The passkey request expired. Please try again.',
	'E-AUTH-QR-STATE': 'This sign-in request has already been answered.',
	'E-AUTH-QR-CODE': 'The code does not match the one on the other screen.',
	'E-AUTH-FORBIDDEN': 'You do not have permission to do that.',
	'E-AUTH-PENDING': 'Your account is not activated yet. Please check your email.',
	'E-AUTH-SUSPENDED': 'This account is suspended.',
	'E-AUTH-ANONYMIZED': 'This account has been erased.',
	'E-AUTH-TOTP-REQUIRED': 'Enter the code from your authenticator app.',
	'E-AUTH-TOTP-INVALID': 'That code is not valid.',
	'E-AUTH-TOTP-ENROLLED': 'Two-factor authentication is already set up.',
	'E-AUTH-EMAIL-TAKEN': 'An account with that email address already exists.',
	'E-AUTH-CLOSED': 'Registration is closed.',
	'E-AUTH-OWNER-ERASURE': 'You own an organisation. Transfer or delete it first.',
	'E-AUTH-TRANSFER-TARGET': 'That person is not an accepted member of this organisation.',
	'E-AUTH-ORG': 'You are not a member of that organisation.',
	'E-AUTH-ORG-NOT-EMPTY': 'This organisation still has members or records.',
	'E-AUTH-CONSENT-REQUIRED': 'Please accept the current terms to continue.',
	'E-AUTH-CONSENT-SCOPE': 'Terms and privacy consent apply to the account, not one organisation.',
	'E-AUTH-SCOPE': 'This API key does not cover that operation.',
	'E-AUTH-KEY-REVOKED': 'This API key is no longer valid.',
	'E-AUTH-PASSWORD-REQUIRED': 'Please choose a password.',
	'E-AUTH-PASSWORD-SET': 'This account already has a password.',

	// --- E-INV-* ---
	'E-INV-IMMUTABLE': 'An issued invoice cannot be changed.',
	'E-INV-NOT-DRAFT': 'Only a draft can be changed.',
	'E-INV-LOCKED': 'A payment is open on this invoice, so its amounts are frozen.',
	'E-INV-STALE': 'Someone else changed this draft. Please try again.',
	'E-INV-LINE': 'Check the line: quantity, unit price, description and unit.',
	'E-INV-EMPTY': 'Add at least one line before issuing.',
	'E-INV-NO-BUYER': 'Choose a buyer before issuing.',
	'E-INV-BUYER-INCOMPLETE': "The buyer's details are incomplete.",
	'E-INV-BUYER-TAXNUMBER': "The buyer's tax number is missing or malformed.",
	'E-INV-BUYER-ADDRESS': 'A company buyer needs a postcode, city and street.',
	'E-INV-SELLER-INCOMPLETE': 'Your seller details are incomplete.',
	'E-INV-SELLER-TAXNUMBER': 'The seller tax number is malformed.',
	'E-INV-SELLER-ADDRESS': 'The seller address is malformed.',
	'E-INV-SELLER-NO-DRAFT': 'There is no staged seller change to publish.',
	'E-INV-SELLER-DRAFT-OPEN': 'A seller change is staged. Publish or discard it first.',
	'E-INV-SELLER-EXISTS': 'This company is already set up.',
	'E-INV-SELLER-ORG-KIND': 'Only a company workspace can be set up for invoicing.',
	'E-INV-SELLER-TAXNUMBER-LOCKED':
		'The tax number can no longer be changed: invoices have been issued under it. A different tax number is a new company.',
	'E-INV-SELLER-CLOSED': 'This company is read-only. Only payments can be recorded.',
	'E-INV-SELLER-PENDING': 'A card payment is in progress. Try again when it has finished.',
	'E-INV-SELLER-DEPLOYMENT': "The operator's own company cannot be made read-only.",
	'E-INV-COUNTRY': 'Not a valid country code.',
	'E-INV-TOO-LONG': 'That text is too long.',
	'E-INV-BAD-TEXT': 'That text contains characters an invoice cannot carry.',
	'E-INV-DATE-RANGE': 'The fulfilment date must be within a year of today.',
	'E-INV-DATE-ORDER': 'The due date cannot be before the fulfilment date.',
	'E-INV-ALREADY-STORNOED': 'This invoice has already been cancelled.',
	'E-INV-STORNO-OF-STORNO': 'A cancellation invoice cannot itself be cancelled.',
	'E-INV-NOT-ISSUED': 'This invoice has not been issued yet.',
	'E-INV-NOT-STORNOABLE': 'This invoice cannot be cancelled.',
	'E-INV-VATHUF': 'The HUF VAT amount is missing.',
	'E-INV-NO-RATE': 'No exchange rate is available for that date.',
	'E-INV-RATE': 'Changing the rate needs a currency as well.',
	'E-INV-CURRENCY-DISABLED': 'That currency is not enabled.',
	'E-INV-VAT-CODE': 'Not a valid VAT code.',
	'E-INV-DISCOUNT': 'Check the discount: it needs a value and cannot exceed the line net.',
	'E-INV-SERIES': 'The invoice number series is unavailable.',
	'E-INV-PDF-PENDING': 'The PDF is still being prepared.',
	'E-INV-CHANGED': 'This draft changed while it was being issued. Please try again.',
	'E-INV-LINE-NOTFOUND': 'No such line on this invoice.',
	'E-INV-PAYMENT-DAYS': 'Payment terms must be between 0 and 36500 days.',
	'E-INV-CASH-DATES': 'A cash invoice is dated and paid on the day it is issued.',
	'E-INV-METHOD-UNSUPPORTED': 'Choose bank transfer or cash.',

	// --- E-PAY-* ---
	'E-PAY-PROVIDER': 'Unknown or disabled payment provider.',
	'E-PAY-PROVIDER-DOWN': 'The payment provider is unreachable. Please try again.',
	'E-PAY-CAPABILITY': 'The payment provider does not support that.',
	'E-PAY-STATE': 'Not possible in the payment’s current state.',
	'E-PAY-AMOUNT': 'Invalid amount.',
	'E-PAY-CURRENCY': 'The payment currency must match the invoice.',
	'E-PAY-ROUNDING': 'This currency only accepts whole units.',
	'E-PAY-ALLOC-EXCEEDS': 'That would allocate more than the payment covers.',
	'E-PAY-ALREADY-ALLOCATED': 'This payment is already allocated to that invoice.',
	'E-PAY-NOT-PAYABLE': 'This invoice cannot be paid.',
	'E-PAY-RETURN-URL': 'Invalid return address.',

	// --- E-NAV-* ---
	'E-NAV-CREDENTIALS': 'The tax authority rejected our credentials.',
	'E-NAV-CREDENTIALS-INVALID':
		'NAV did not accept these credentials. Check the login and password.',
	'E-NAV-TAXPAYER-UNKNOWN':
		"NAV does not recognise the company's tax number. It must be the tax number your technical user belongs to. Check it under Company details.",
	'E-NAV-CREDENTIALS-GLOBAL': 'This company’s NAV connection is managed by the operator.',
	'E-NAV-UNAVAILABLE': 'The tax authority is unreachable. The filing will be retried.',
	'E-NAV-BUSINESS': 'The tax authority refused the filing.',
	'E-NAV-UNFILABLE':
		'The tax authority refused the filing. Correct the invoice and issue it again.',
	'E-NAV-REQUEST-ID-SPENT':
		'This filing cannot be retried. Cancel the invoice and issue it again.',
	'E-NAV-REQUEST-ID-REUSED': 'This invoice may already be filed. Check its status before acting.',
	'E-NAV-FORBIDDEN': 'You do not have permission to query the tax authority.',
	'E-NAV-TAX-NUMBER': 'Not a Hungarian tax number.',
	'E-NAV-TAXPAYER-NOTFOUND': 'The tax authority does not know that tax number.',
	'E-NAV-VIES-UNAVAILABLE': 'EU VAT validation is unavailable. Please try again.',
	'E-NAV-EXPORT-RANGE': 'Give exactly one date or number range.',
	'E-NAV-SUBMISSION-STATE': 'Not possible in this filing’s current state.',
	'E-NAV-NOT-ISSUED': 'Only an issued invoice can be filed.',
	'E-NAV-NOT-REDRIVABLE': 'This filing is not failed, so it cannot be re-driven.',
	'E-NAV-BATCH-MEMBER': 'This invoice is filed in a batch. Cancel the batch instead.',
	'E-NAV-FILING-IN-FLIGHT': 'The filing is in progress. Wait for it to settle.',
	'E-NAV-AUTH-UNREADABLE':
		'The tax authority replied with something unreadable. It will be retried.',
	'E-NAV-HTTP-STATUS': 'The tax authority refused the request. Nothing was filed.',
	'E-NAV-NO-TRANSACTION-ID':
		'The filing was accepted but its reference was lost. It is being reconciled.',
	'E-NAV-UNREADABLE-REPLY':
		'The tax authority’s reply was unreadable. The filing state is being checked.',

	// --- E-SCRIPT-* (crates/saas-script/src/{error,api,lib}.rs) ---
	'E-SCRIPT-COMPILE': 'The application script failed to compile.',
	'E-SCRIPT-BUDGET': 'The application script ran out of budget.',
	'E-SCRIPT-TIMEOUT': 'The application script timed out.',
	'E-SCRIPT-RUNTIME': 'The application script failed.',
	'E-SCRIPT-INIT-ONLY': 'That is only allowed while the application starts up.',
	'E-SCRIPT-TX-TIMEOUT': 'The database transaction timed out.',
	'E-SCRIPT-TX-REMOTE': 'The application script failed.'
}

// vim: ts=4
