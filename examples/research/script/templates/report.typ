// `sys.inputs.data` is the JSON `pdf::render` was given; `body` is `pdf::markdown` output.
#let data = json(bytes(sys.inputs.data))

#set page(paper: "a4", margin: 2cm, numbering: "1")
#set text(size: 11pt)
#set par(justify: true)

#align(center, text(size: 18pt, weight: "bold", data.title))
#v(1em)

// `pdf::markdown` escapes markup in the notebook text, so eval sees only what it generated.
#eval(data.body, mode: "markup")
