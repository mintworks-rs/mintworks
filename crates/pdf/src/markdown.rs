//! Markdown → typst markup. The result is a string a template evaluates with
//! `#eval(sys.inputs.<key>, mode: "markup")`; text is escaped, so markdown can never inject typst
//! code. Raw HTML, images (alt text only), footnotes and math are not rendered.

use std::fmt::Write as _;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// Typst markup characters; typst accepts a backslash before any of them.
const SPECIAL: &str = "\\#*_`$<>@[]~=-+/";

/// Converts CommonMark (plus tables and strikethrough) to typst markup.
#[must_use]
pub fn to_typst(md: &str) -> String {
	let mut out = String::new();
	// One entry per open tag: (trim trailing whitespace first, closing text).
	let mut closers: Vec<(bool, &'static str)> = Vec::new();
	let mut code: Option<String> = None;
	for event in Parser::new_ext(md, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH) {
		match event {
			Event::Start(tag) => {
				let closer = open(&mut out, tag, &mut code);
				closers.push(closer);
			}
			Event::End(TagEnd::CodeBlock) => {
				closers.pop();
				let text = code.take().unwrap_or_default();
				string(&mut out, text.strip_suffix('\n').unwrap_or(&text));
				out.push_str(");\n\n");
			}
			Event::End(_) => {
				let (trim, text) = closers.pop().unwrap_or((false, ""));
				if trim {
					out.truncate(out.trim_end().len());
				}
				out.push_str(text);
			}
			Event::Text(t) => match code.as_mut() {
				Some(c) => c.push_str(&t),
				None => escape(&mut out, &t),
			},
			Event::Code(t) => {
				out.push_str("#raw(");
				string(&mut out, &t);
				out.push_str(");");
			}
			Event::SoftBreak => out.push('\n'),
			Event::HardBreak => out.push_str("\\\n"),
			Event::Rule => {
				block(&mut out);
				out.push_str("#line(length: 100%);\n\n");
			}
			// Raw HTML, and constructs whose options are not enabled.
			_ => {}
		}
	}
	out.truncate(out.trim_end().len());
	out
}

/// Writes the opening markup for `tag` and returns its closer.
fn open(out: &mut String, tag: Tag, code: &mut Option<String>) -> (bool, &'static str) {
	match tag {
		Tag::Paragraph => (false, "\n\n"),
		Tag::Heading { level, .. } => {
			block(out);
			let _ = write!(out, "#heading(level: {})[", level as usize);
			(false, "];\n\n")
		}
		Tag::BlockQuote(_) => {
			block(out);
			out.push_str("#quote(block: true)[");
			(true, "];\n\n")
		}
		Tag::CodeBlock(kind) => {
			block(out);
			out.push_str("#raw(block: true, ");
			if let CodeBlockKind::Fenced(info) = kind
				&& let Some(lang) = info.split_whitespace().next()
			{
				out.push_str("lang: ");
				string(out, lang);
				out.push_str(", ");
			}
			*code = Some(String::new());
			(false, "")
		}
		Tag::List(start) => {
			block(out);
			match start {
				Some(n) => {
					let _ = writeln!(out, "#enum(start: {n},");
				}
				None => out.push_str("#list(\n"),
			}
			(true, "\n);\n\n")
		}
		Tag::Item | Tag::TableCell => {
			out.push('[');
			(true, "],\n")
		}
		Tag::Table(aligns) => {
			block(out);
			let _ = write!(out, "#table(columns: {}, align: (", aligns.len());
			for a in &aligns {
				out.push_str(match a {
					Alignment::None => "auto, ",
					Alignment::Left => "left, ",
					Alignment::Center => "center, ",
					Alignment::Right => "right, ",
				});
			}
			out.push_str("),\n");
			(true, "\n);\n\n")
		}
		Tag::TableHead => {
			out.push_str("table.header(\n");
			(true, "\n),\n")
		}
		Tag::Emphasis => inline(out, "#emph["),
		Tag::Strong => inline(out, "#strong["),
		Tag::Strikethrough => inline(out, "#strike["),
		// Only http(s)/mailto: model-written markdown must not plant `javascript:`/`file:` links;
		// `link("")` is a typst error too.
		Tag::Link { dest_url, .. } if safe_link(&dest_url) => {
			out.push_str("#link(");
			string(out, &dest_url);
			inline(out, ")[")
		}
		// Images keep their alt text: the World serves nothing outside the template set.
		_ => (false, ""),
	}
}

fn safe_link(url: &str) -> bool {
	let url = url.to_ascii_lowercase();
	["http://", "https://", "mailto:"].iter().any(|p| url.starts_with(p))
}

/// Every call ends in `;` so following text such as `.x` or `(` cannot extend the expression.
fn inline(out: &mut String, start: &str) -> (bool, &'static str) {
	out.push_str(start);
	(false, "];")
}

/// Starts a block element on its own line.
fn block(out: &mut String) {
	if !out.is_empty() && !out.ends_with('\n') && !out.ends_with('[') {
		out.push('\n');
	}
}

/// Escapes markup text. A `.` after a digit is escaped too: `1.` at a line start is a list.
fn escape(out: &mut String, text: &str) {
	for c in text.chars() {
		if SPECIAL.contains(c) || (c == '.' && out.ends_with(|p: char| p.is_ascii_digit())) {
			out.push('\\');
		}
		out.push(c);
	}
}

/// Writes `s` as a typst string literal.
fn string(out: &mut String, s: &str) {
	out.push('"');
	for c in s.chars() {
		match c {
			'\\' => out.push_str("\\\\"),
			'"' => out.push_str("\\\""),
			'\n' => out.push_str("\\n"),
			'\r' => out.push_str("\\r"),
			'\t' => out.push_str("\\t"),
			c => out.push(c),
		}
	}
	out.push('"');
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeMap;

	use super::*;
	use crate::{Files, render};

	#[test]
	fn escapes_every_markup_character() {
		let mut out = String::new();
		escape(&mut out, "a#b*c_d`e$f<g>h@i[j]k~l=m-n+o/p\\q 1. 2.5");
		assert_eq!(
			out,
			"a\\#b\\*c\\_d\\`e\\$f\\<g\\>h\\@i\\[j\\]k\\~l\\=m\\-n\\+o\\/p\\\\q 1\\. 2\\.5"
		);
	}

	#[test]
	fn typst_code_in_markdown_stays_text() {
		assert_eq!(to_typst("#set page(width: 1pt)"), "\\#set page(width: 1pt)");
	}

	#[test]
	fn headings_and_inline_styles() {
		assert_eq!(to_typst("## Hi *a*"), "#heading(level: 2)[Hi #emph[a];];");
		assert_eq!(to_typst("**b** ~~c~~."), "#strong[b]; #strike[c];.");
	}

	#[test]
	fn lists_nest_and_keep_their_start() {
		assert_eq!(to_typst("- a\n- b"), "#list(\n[a],\n[b],\n);");
		assert_eq!(to_typst("3. x\n   - y"), "#enum(start: 3,\n[x\n#list(\n[y],\n);],\n);");
	}

	#[test]
	fn links_and_code() {
		assert_eq!(to_typst("[t](https://e.com/\"x)"), "#link(\"https://e.com/\\\"x\")[t];");
		assert_eq!(to_typst("[t]()"), "t");
		assert_eq!(to_typst("[t](javascript:alert(1))"), "t");
		assert_eq!(to_typst("`a\"b`"), "#raw(\"a\\\"b\");");
		assert_eq!(
			to_typst("```rust\nfn f() {}\n```"),
			"#raw(block: true, lang: \"rust\", \"fn f() {}\");"
		);
	}

	#[test]
	fn block_quotes_and_tables() {
		assert_eq!(to_typst("> q"), "#quote(block: true)[q];");
		assert_eq!(
			to_typst("| a | b |\n|:--|--:|\n| 1 | 2 |"),
			"#table(columns: 2, align: (left, right, ),\ntable.header(\n[a],\n[b],\n),\n[1],\n[2],\n);"
		);
	}

	#[test]
	fn every_construct_compiles() {
		let md = "# T\n\nPara *e* **s** ~~x~~ `c` [l](https://e.com) 1. #x $y$ <z>\\\nnext\n\n\
			> quote\n\n- a\n  - b\n\n4. c\n\n   loose\n5. d\n\n```sh\necho \"hi\"\n```\n\n---\n\n\
			| a | b |\n|---|:-:|\n| 1 | *2* |\n\n![alt](img.png)";
		let files = Files::from([(
			"main.typ".to_owned(),
			b"#eval(sys.inputs.body, mode: \"markup\")".to_vec(),
		)]);
		let inputs = BTreeMap::from([("body".to_owned(), to_typst(md))]);
		let pdf = render(&files, "main.typ", &inputs, false).unwrap();
		assert!(pdf.starts_with(b"%PDF"));
	}
}

// vim: ts=4
