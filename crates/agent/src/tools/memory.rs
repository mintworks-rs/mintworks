//! `memory_list/read/write/append/search` over [`Memory`]. A run reaches only the spaces its
//! spec granted, and what it writes is authored by its `run_…` uid.

use std::sync::Arc;

use async_trait::async_trait;
use mintworks_memory::Memory;
use serde_json::{Value, json};

use crate::tool::{Access, SpaceGrant, Tool, ToolRun, shown};

const SEARCH_LIMIT: u64 = 20;

#[derive(Clone, Copy)]
enum Op {
	List,
	Read,
	Write,
	Append,
	Search,
}

struct MemoryTool {
	memory: Arc<Memory>,
	op: Op,
}

/// The five memory tools, for [`crate::Tools::add`].
pub fn memory_tools(memory: &Arc<Memory>) -> Vec<Arc<dyn Tool>> {
	[Op::List, Op::Read, Op::Write, Op::Append, Op::Search]
		.into_iter()
		.map(|op| Arc::new(MemoryTool { memory: Arc::clone(memory), op }) as Arc<dyn Tool>)
		.collect()
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
	args.get(key)
		.and_then(Value::as_str)
		.ok_or_else(|| format!("missing string argument {key}"))
}

/// The granted space named in `args`; `write` also requires the grant to allow writing.
fn space<'a>(run: &ToolRun<'_>, args: &'a Value, write: bool) -> Result<&'a str, String> {
	granted(run.spaces, str_arg(args, "space")?, write)
}

fn granted<'a>(grants: &[SpaceGrant], key: &'a str, write: bool) -> Result<&'a str, String> {
	match grants.iter().find(|g| g.key == key) {
		None => Err(format!("space {key} is not granted to this run")),
		Some(g) if write && g.access == Access::Read => {
			Err(format!("space {key} is read-only for this run"))
		}
		Some(_) => Ok(key),
	}
}

fn schema(props: &Value, required: &[&str]) -> Value {
	json!({ "type": "object", "properties": props, "required": required })
}

#[async_trait]
impl Tool for MemoryTool {
	fn name(&self) -> &str {
		match self.op {
			Op::List => "memory_list",
			Op::Read => "memory_read",
			Op::Write => "memory_write",
			Op::Append => "memory_append",
			Op::Search => "memory_search",
		}
	}

	fn description(&self) -> &str {
		match self.op {
			Op::List => "List the documents in a memory space.",
			Op::Read => "Read a memory document; the current version unless `version` is given.",
			Op::Write => "Replace a memory document's body, creating it if absent.",
			Op::Append => "Append text to a memory document, creating it if absent.",
			Op::Search => "Search memory documents for every word of `query`.",
		}
	}

	fn schema(&self) -> Value {
		let space = json!({ "type": "string", "description": "The memory space key." });
		let path = json!({ "type": "string", "description": "The document path." });
		let body = json!({ "type": "string" });
		match self.op {
			Op::List => schema(&json!({ "space": space }), &["space"]),
			Op::Read => schema(
				&json!({ "space": space, "path": path, "version": { "type": "integer" } }),
				&["space", "path"],
			),
			Op::Write | Op::Append => schema(
				&json!({ "space": space, "path": path, "body": body }),
				&["space", "path", "body"],
			),
			Op::Search => schema(
				&json!({
					"query": { "type": "string" },
					"space": { "type": "string", "description": "Omit to search every granted space." },
					"limit": { "type": "integer", "minimum": 1, "maximum": 100 },
				}),
				&["query"],
			),
		}
	}

	async fn call(&self, run: &ToolRun<'_>, args: Value) -> Result<Value, String> {
		let (mem, ctx) = (&self.memory, run.ctx);
		let author = Some(run.run.as_str());
		match self.op {
			Op::List => {
				let docs = mem
					.list(ctx, space(run, &args, false)?)
					.await
					.map_err(|e| shown(&e, self.name(), run.run))?;
				Ok(docs
					.iter()
					.map(
						|d| json!({ "path": d.path, "version": d.version, "updatedAt": d.updated_at }),
					)
					.collect())
			}
			Op::Read => {
				let ver = args.get("version").and_then(Value::as_i64);
				let v = mem
					.read(ctx, space(run, &args, false)?, str_arg(&args, "path")?, ver)
					.await
					.map_err(|e| shown(&e, self.name(), run.run))?;
				Ok(
					json!({ "version": v.version, "body": v.body, "author": v.author, "createdAt": v.created_at }),
				)
			}
			Op::Write | Op::Append => {
				let (s, p, b) =
					(space(run, &args, true)?, str_arg(&args, "path")?, str_arg(&args, "body")?);
				let v = if matches!(self.op, Op::Write) {
					mem.write(ctx, s, p, b, author).await
				} else {
					mem.append(ctx, s, p, b, author).await
				}
				.map_err(|e| shown(&e, self.name(), run.run))?;
				Ok(json!({ "path": p, "version": v.version }))
			}
			Op::Search => {
				let query = str_arg(&args, "query")?;
				let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(SEARCH_LIMIT);
				let limit = u32::try_from(limit).unwrap_or(u32::MAX);
				// Never the org-wide search: that would reach spaces the run was not granted.
				let spaces: Vec<&str> = if args.get("space").is_some_and(|v| !v.is_null()) {
					vec![space(run, &args, false)?]
				} else {
					run.spaces.iter().map(|g| g.key.as_str()).collect()
				};
				let mut per_space = Vec::new();
				for s in spaces {
					let found = mem
						.search(ctx, Some(s), query, limit)
						.await
						.map_err(|e| shown(&e, self.name(), run.run))?;
					per_space.push(found.into_iter().map(|h| {
						json!({ "space": h.space_key, "path": h.path, "version": h.version, "snippet": h.snippet })
					}).collect());
				}
				let mut hits = interleave(per_space);
				hits.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
				Ok(Value::Array(hits))
			}
		}
	}
}

/// Round-robin across spaces, so one busy space cannot fill the limit: hits carry no score.
fn interleave(lists: Vec<Vec<Value>>) -> Vec<Value> {
	let mut iters: Vec<_> = lists.into_iter().map(Vec::into_iter).collect();
	let mut out = Vec::new();
	loop {
		let before = out.len();
		out.extend(iters.iter_mut().filter_map(Iterator::next));
		if out.len() == before {
			return out;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn granted_refuses_ungranted_and_read_only_writes() {
		let grants = [SpaceGrant { key: "g:1".into(), access: Access::Read, about: None }];
		assert!(granted(&grants, "g:2", false).unwrap_err().contains("not granted"));
		assert_eq!(granted(&grants, "g:1", false), Ok("g:1"));
		assert!(granted(&grants, "g:1", true).unwrap_err().contains("read-only"));
	}

	#[test]
	fn interleave_takes_one_per_space_per_round() {
		let got = interleave(vec![vec![json!(1), json!(2), json!(3)], vec![], vec![json!(4)]]);
		assert_eq!(got, [json!(1), json!(4), json!(2), json!(3)]);
	}
}

// vim: ts=4
