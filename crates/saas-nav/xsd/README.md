# Vendored NAV Online Számla schemas

Schema version **3.0** (Hungarian interface specification v3.0, dated 2026-02-12).
Fetched **2026-09-05** from the schema tree NAV publishes on GitHub, which is the drift
check's upstream (`../tests/drift.rs`).

| File | Namespace | Source URL |
| --- | --- | --- |
| `invoiceApi.xsd` | `http://schemas.nav.gov.hu/OSA/3.0/api` | <https://raw.githubusercontent.com/nav-gov-hu/Online-Invoice/master/src/schemas/nav/gov/hu/OSA/invoiceApi.xsd> |
| `invoiceData.xsd` | `http://schemas.nav.gov.hu/OSA/3.0/data` | <https://raw.githubusercontent.com/nav-gov-hu/Online-Invoice/master/src/schemas/nav/gov/hu/OSA/invoiceData.xsd> |
| `invoiceBase.xsd` | `http://schemas.nav.gov.hu/OSA/3.0/base` | <https://raw.githubusercontent.com/nav-gov-hu/Online-Invoice/master/src/schemas/nav/gov/hu/OSA/invoiceBase.xsd> |
| `invoiceAnnulment.xsd` | `http://schemas.nav.gov.hu/OSA/3.0/annul` | <https://raw.githubusercontent.com/nav-gov-hu/Online-Invoice/master/src/schemas/nav/gov/hu/OSA/invoiceAnnulment.xsd> |
| `serviceMetrics.xsd` | `http://schemas.nav.gov.hu/OSA/3.0/metrics` | <https://raw.githubusercontent.com/nav-gov-hu/Online-Invoice/master/src/schemas/nav/gov/hu/OSA/serviceMetrics.xsd> |
| `common.xsd` | `http://schemas.nav.gov.hu/NTCA/1.0/common` | <https://raw.githubusercontent.com/nav-gov-hu/Common/Common-1.0.RC3/src/schemas/nav/gov/hu/NTCA/common.xsd> |

Resolves the `[OPEN RISK]` in `claude-docs/legal-research.md` item 4:

- The `common` namespace project is **`github.com/nav-gov-hu/Common`**, and OSA 3.0 pins it
  at tag **`Common-1.0.RC3`** — that pin is stated in NAV's own
  `src/schemas/nav/gov/hu/OSA/catalog.xml`, which maps the two unresolved import namespaces
  to `common.xsd` and `invoiceBase.xsd`.
- No zip download from <https://onlineszamla.nav.gov.hu/dokumentaciok> was needed: the
  GitHub schema tree carries the same files and is stable enough to diff against.
- The schema NAV names `invoiceMetrics` in the specification prose is published as
  **`serviceMetrics.xsd`**.

## Audit data export

`claude-docs/legal-research.md` item 3 identifies **no separate schema** for the statutory
tax-authority audit data export (*adóhatósági ellenőrzési adatszolgáltatás*). The taxpayer
may choose, and this project chose the Online Számla **`invoiceData.xsd` 3.0** structure —
several invoices wrapped in a grouping root element, UTF-8 — over the 23/2014 (VI. 30.) NGM
rendelet's own `szamla.xsd` (3. melléklet). Nothing extra is vendored for it.

## Do not edit these files

They are compared byte-for-byte against the published copies by `../tests/drift.rs`. NAV's
schemas import each other by namespace with no `schemaLocation`, which libxml2 cannot
resolve; `../tests/xsd_validation.rs` patches a throwaway copy in the temp directory instead
of touching what is vendored here.
