//! `web_search` and `fetch` over [`Search`]. A fetched page comes back as a `src_…` id plus an
//! excerpt, which the model cites as `[src_…]`; the row keeps the full text it was shown.

use std::sync::Arc;

use async_trait::async_trait;
use mintworks_core::App;
use mintworks_search::Search;
use serde_json::{Value, json};

use crate::tool::{Tool, ToolRun, shown};

/// Characters of page text returned to the model; the rest stays in the `sources` row.
const EXCERPT_CHARS: usize = 4000;

#[derive(Clone, Copy)]
enum Op {
	WebSearch,
	Fetch,
}

struct SearchTool {
	search: Search,
	op: Op,
}

/// `web_search` and `fetch`, for [`crate::Tools::add`].
pub fn search_tools(app: &App) -> Vec<Arc<dyn Tool>> {
	[Op::WebSearch, Op::Fetch]
		.into_iter()
		.map(|op| Arc::new(SearchTool { search: Search::new(app.clone()), op }) as Arc<dyn Tool>)
		.collect()
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
	args.get(key)
		.and_then(Value::as_str)
		.ok_or_else(|| format!("missing string argument {key}"))
}

#[async_trait]
impl Tool for SearchTool {
	fn name(&self) -> &str {
		match self.op {
			Op::WebSearch => "web_search",
			Op::Fetch => "fetch",
		}
	}

	fn description(&self) -> &str {
		match self.op {
			Op::WebSearch => {
				"Search the web. Returns title, url and snippet per hit; fetch a url to read and \
				 cite it. A hit with a `source` is a review site: never fetchable, cite its \
				 snippet as [src_…]."
			}
			Op::Fetch => {
				"Fetch a web page as text. Returns its `source` id and an excerpt; cite it as \
				 [src_…]."
			}
		}
	}

	fn schema(&self) -> Value {
		let (props, required) = match self.op {
			Op::WebSearch => (
				json!({
					"query": { "type": "string" },
					"lang": { "type": "string", "description": "e.g. hu, en" },
					"market": { "type": "string", "description": "e.g. HU" },
				}),
				vec!["query"],
			),
			Op::Fetch => (json!({ "url": { "type": "string" } }), vec!["url"]),
		};
		json!({ "type": "object", "properties": props, "required": required })
	}

	async fn call(&self, run: &ToolRun<'_>, args: Value) -> Result<Value, String> {
		let (ctx, id) = (run.ctx, Some(run.run.as_str()));
		match self.op {
			Op::WebSearch => {
				let opt = |k| args.get(k).and_then(Value::as_str);
				let hits = self
					.search
					.search(
						ctx,
						str_arg(&args, "query")?,
						opt("lang"),
						opt("market"),
						run.subject,
						id,
					)
					.await
					.map_err(|e| shown(&e, self.name(), run.run))?;
				Ok(json!({ "hits": hits }))
			}
			Op::Fetch => {
				let src = self
					.search
					.fetch(ctx, str_arg(&args, "url")?, run.subject, id)
					.await
					.map_err(|e| shown(&e, self.name(), run.run))?;
				let excerpt: String = src.text.chars().take(EXCERPT_CHARS).collect();
				Ok(json!({
					"source": src.uid.as_str(),
					"url": src.url,
					"title": src.title,
					"excerpt": excerpt,
					"truncated": excerpt.len() < src.text.len(),
				}))
			}
		}
	}
}

// vim: ts=4
