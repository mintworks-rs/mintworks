// `sys.inputs.data` is the JSON `pdf::render` was given; `body` is `pdf::markdown` output.
#let data = json(bytes(sys.inputs.data))

= #data.title

#eval(data.body, mode: "markup")
