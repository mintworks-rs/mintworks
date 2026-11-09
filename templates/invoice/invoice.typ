// The invoice page. `strings.typ` is concatenated in front of this file by
// `mintworks_invoice::pdf`, so `t()` and `strings` are already in scope — do not `#import`.
//
// Every amount arrives pre-formatted as a string from Rust. Nothing here does arithmetic:
// no float ever touches the money path, not even in the renderer.

#let d = json(bytes(sys.inputs.at("invoice")))
#let lang = d.lang
#let s = key => t(lang, key)

// PDF/A-3b rejects a document with no date, and `InvoiceWorld::today()` is `none` so the
// clock cannot supply one. The issue date is the only date that keeps a re-render identical.
#let iso(v) = datetime(year: int(v.slice(0, 4)), month: int(v.slice(5, 7)), day: int(v.slice(8, 10)))
#set document(date: iso(d.invoice.issuedAt))

// Branding. Every key is optional: a data document without `brand` renders the plain layout.
#let brand = d.at("brand", default: none)
#let logo = if brand != none { brand.at("logo", default: none) }
#let accent-hex = if brand != none { brand.at("accent", default: none) }
#let accent = if accent-hex != none { rgb(accent-hex) } else { luma(90) }

#set page(paper: "a4", margin: (x: 18mm, y: 16mm))
// Plain text only (PDF/A). The software line is good practice, not page content the law asks
// for; NAV's `softwareData` is the binding identification.
#let software = d.at("software", default: none)
#set page(footer: text(size: 7pt, fill: luma(90))[
	#if software != none [#s("software"): #software.name #software.version]
	#h(1fr)
	#context counter(page).display("1 / 1", both: true)
])
// Watermarks: test-mode invoices and `POST /api/invoices/preview` must never pass for an
// invoice. A solid light fill under the content, no transparency, so PDF/A stays happy.
// One line per language — the joined bilingual string is too long for the page diagonal.
#let wm(key) = if lang == "hu-en" {
	(strings.hu.at(key, default: key), strings.en.at(key, default: key))
} else { (s(key),) }
#let marks = (
	if d.at("testMode", default: false) { wm("watermark.test") } else { () }
) + (
	if d.at("preview", default: false) { ("ELŐNÉZET / PREVIEW",) } else { () }
)
#set page(background: if marks.len() > 0 {
	rotate(-45deg, stack(spacing: 8mm,
		..marks.map(m => align(center, text(size: 44pt, weight: "bold", fill: luma(225))[#m])),
	))
})
// `text(lang:)` takes an ISO 639 code only; a bilingual page hyphenates as Hungarian.
#set text(font: ("Libertinus Serif", "New Computer Modern", "DejaVu Sans"), size: 9.5pt,
	lang: if lang == "hu-en" { "hu" } else { lang })
#set table(stroke: 0.4pt + luma(160))

// A block, not a bare linebreak: consecutive fields in the party column ran together —
// "12345678242 Közösségi adószám" on one line — because only the label was broken off.
#let field(label, value) = if value != none and value != "" [
	#block(below: 1.2mm)[#text(size: 8pt, fill: luma(90))[#label] \ #value]
]

#let party(title, p) = [
	#text(size: 8pt, weight: "bold", fill: luma(90))[#upper(title)]
	#line(length: 100%, stroke: 0.4pt + luma(160))
	#text(weight: "bold")[#p.name] \
	#p.address \
	#field(s("tax-number"), p.at("taxNumber", default: none))
	#field(s("eu-vat-id"), p.at("euVatId", default: none))
	#field(s("group-tax-no"), p.at("groupTaxNo", default: none))
	#field(s("bank"), p.at("bankAccount", default: none))
	#field(s("email"), p.at("email", default: none))
]

// Info box: one label/value pair per row. A one-row strip of seven columns wrapped every
// joined hu-en label. Period and original rows exist only when the invoice carries them.
#let lbl(key) = text(size: 8pt, fill: luma(90))[#s(key)]
#let period-start = d.invoice.at("periodStart", default: none)
#let original = d.invoice.at("original", default: none)
#let info = (
	lbl("number"), text(weight: "bold")[#d.invoice.number],
	lbl("issued"), [#d.invoice.issuedAt],
	lbl("fulfilment"), [#d.invoice.fulfilmentDate],
) + (if period-start != none {
	(lbl("period"), [#period-start – #d.invoice.at("periodEnd", default: "")])
} else { () }) + (
	lbl("due"), [#d.invoice.dueDate],
	lbl("payment"), [#s("payment." + d.invoice.paymentMethod)],
	lbl("currency"), [#d.invoice.currency],
) + (if original != none { (lbl("original"), [#original]) } else { () })
#let info-box = block(stroke: 0.4pt + luma(160), inset: 2mm, table(
	columns: 2, stroke: none, inset: (x: 2mm, y: 1mm), align: (left, left), ..info,
))

// Logo above the title, top-left in the accent colour; info box top-right; one accent rule
// under all. A wide logo is shrunk to the column, which the info box leaves narrow.
// The title breaks at " / " like the table headers: the joined hu-en title is too wide here.
#let fit-logo = layout(size => {
	let img = image(logo, height: 14mm)
	if measure(img).width > size.width { image(logo, width: size.width) } else { img }
})
#let masthead = (
	if logo != none { (fit-logo,) } else { () }
) + (text(fill: accent)[= #s("title." + d.invoice.kind).split(" / ").join(linebreak())],)
#grid(columns: (1fr, auto), column-gutter: 6mm, align: (left + top, right + top),
	stack(spacing: 4mm, ..masthead),
	info-box,
)
#line(length: 100%, stroke: 0.8pt + accent)

#grid(columns: (1fr, 1fr), gutter: 10mm,
	party(s("seller"), d.seller),
	party(s("buyer"), d.buyer),
)

#v(4mm)

// Line table. The `discount` column is dropped entirely when no line carries one.
// Header labels break at " / ": joined hu-en labels on one line left the `1fr` description
// column no width at all.
#let any-discount = d.lines.any(l => l.at("discount", default: none) != none)
#let head = (
	s("col.no"), s("col.description"), s("col.qty"), s("col.unit"), s("col.unit-price"),
) + (if any-discount { (s("col.discount"),) } else { () }) + (
	s("col.net"), s("col.vat-rate"), s("col.vat"), s("col.gross"),
)

#table(
	columns: (auto, 1fr) + (auto,) * (head.len() - 2),
	align: (col, _) => if col == 1 { left } else { right },
	table.header(..head.map(h => text(size: 8pt, weight: "bold", h.split(" / ").join(linebreak())))),
	..d.lines.map(l => (
		[#l.no],
		[
			#l.description
			#if l.at("discountDescription", default: none) != none [
				\ #text(size: 8pt, fill: luma(90))[#l.discountDescription]
			]
			#if l.at("note", default: none) != none [
				\ #text(size: 8pt, fill: luma(90))[#l.note]
			]
		],
		[#l.qty], [#l.unit], [#l.unitPrice],
	) + (if any-discount { ([#l.at("discount", default: "")],) } else { () }) + (
		[#l.net], [#l.vatRate], [#l.vat], [#l.gross],
	)).flatten(),
)

#v(4mm)

#grid(columns: (1fr, auto), gutter: 8mm,
	[
		#text(size: 8pt, weight: "bold", fill: luma(90))[#upper(s("vat-summary"))]
		// The HUF column is dropped entirely when no group carries one — on a HUF invoice it
		// is a permanently empty column, and Áfa tv. 172. § only asks for it in a foreign
		// currency. Same rule as `any-discount` above.
		#let any-huf = d.groups.any(g => g.at("vatHuf", default: none) != none)
		#table(
			columns: if any-huf { 5 } else { 4 },
			align: (col, _) => if col == 0 { left } else { right },
			table.header(..((
				s("col.vat-rate"), s("col.net"), s("col.vat"), s("col.gross"),
			) + (if any-huf { (s("huf-equivalent"),) } else { () }))
				.map(h => text(size: 8pt, weight: "bold", h.split(" / ").join(linebreak())))),
			..d.groups.map(g => (
				[#g.vatRate], [#g.net], [#g.vat], [#g.gross],
			) + (if any-huf { ([#g.at("vatHuf", default: "")],) } else { () })).flatten(),
		)
	],
	[
		#table(
			columns: 2, stroke: none, align: (left, right),
			[#s("col.net")], [#d.totals.net #d.invoice.currency],
			[#s("col.vat")], [#d.totals.vat #d.invoice.currency],
			table.hline(stroke: 0.6pt),
			text(weight: "bold")[#s("total")],
			text(weight: "bold")[#d.totals.gross #d.invoice.currency],
			..if d.totals.at("payable", default: none) != none {(
				[#s("payable")], [#d.totals.payable #d.invoice.currency],
			)} else { () },
		)
	],
)

#if d.at("rate", default: none) != none [
	#v(3mm)
	#text(size: 8pt)[
		#s("rate"): 1 #d.rate.quote = #d.rate.value HUF ·
		#s("rate.date"): #d.rate.date ·
		#s("rate.source"): #d.rate.source
	]
]

// Every applicable Áfa tv. 169. § note, not just the first: a mixed AAM + TAM invoice
// owes a reference per exempt supply.
#let notes = d.invoice.at("vatNotes", default: ()) + (
	if d.at("cashAccounting", default: false) { ("vat.cash",) } else { () }
)
#if notes.len() > 0 [
	#v(3mm)
	#for k in notes [ #text(weight: "bold")[#s(k)] \ ]
]

#if d.invoice.at("notes", default: none) != none [
	#v(3mm)
	#text(size: 8pt, fill: luma(90))[#s("notes"): #d.invoice.notes]
]
