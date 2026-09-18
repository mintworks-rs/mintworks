#!/bin/sh
# Lines-of-code report for the saas-framework workspace.
#
# Counts code lines only (blank lines and comments excluded), split into source vs. tests and
# grouped by server / crates / adapters, plus a section showing how much code is currently
# uncommitted (working tree vs. HEAD).
#
# Rust keeps most of its unit tests *inside* the source files, so classifying whole files is not
# enough: `#[cfg(test)]` items are located and counted separately, and the TESTS table reports them
# apart from the integration tests under `tests/`.
#
# Counting is done by cloc (github.com/AlDanial/cloc), run through `pnpm dlx` unless a `cloc` is
# already on PATH. cloc handles the hard part — language detection and comment stripping. The
# grouping, the source/test split, the diffing and the formatting are done here.
#
#   ./stat.sh        totals + local changes    ./stat.sh -c   local changes only (fast)
#   ./stat.sh -p     per-crate breakdown       ./stat.sh -n   totals only

set -eu

CLOC_SPEC='cloc@2.6.0-cloc'

usage() {
	cat <<'EOF'
usage: ./stat.sh [-p] [-n] [-c] [-h]
  -p, --by-crate       break crates/ and adapters/ down per package
  -n, --no-changes     skip the LOCAL CHANGES section (totals only)
  -c, --changes-only   show only the LOCAL CHANGES section (fast: touched files only)
  -h, --help           this help
EOF
}

die() {
	printf 'stat.sh: %s\n' "$1" >&2
	exit 1
}

by_package=0
do_totals=1
do_changes=1

while [ $# -gt 0 ]; do
	case "$1" in
		-p|--by-crate|--by-package) by_package=1 ;;
		-n|--no-changes) do_changes=0 ;;
		-c|--changes-only) do_totals=0 ;;
		-h|--help) usage; exit 0 ;;
		*) printf 'stat.sh: unknown option: %s\n' "$1" >&2; usage >&2; exit 2 ;;
	esac
	shift
done

[ "$do_totals" -eq 1 ] || [ "$do_changes" -eq 1 ] || die '-c and -n together leave nothing to report'

# Reported paths are relative to the repo root, so always run from there.
cd "$(dirname "$0")"

for req in jq tar diff; do
	command -v "$req" >/dev/null 2>&1 || die "$req is required but was not found on PATH"
done

if command -v cloc >/dev/null 2>&1; then
	run_cloc() { cloc "$@"; }
elif command -v pnpm >/dev/null 2>&1; then
	run_cloc() { pnpm dlx "$CLOC_SPEC" "$@"; }
else
	die "neither cloc nor pnpm found on PATH (one is needed to run $CLOC_SPEC)"
fi

# --timeout 0: without it cloc silently drops files it considers slow *and* appends a warning
#   paragraph to stdout, which breaks --json parsing.
# --skip-uniqueness: keep byte-identical files (small mod.rs stubs) counted separately.
CLOC_OPTS='--include-ext=rs --exclude-dir=target --by-file --json --quiet --timeout 0 --skip-uniqueness'

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT HUP TERM

header_done=0
cloc_version=''
print_header() { # print_header [cloc-json-to-read-the-version-from]
	if [ "$header_done" -eq 1 ]; then return 0; fi
	header_done=1
	if [ -z "$cloc_version" ] && [ $# -ge 1 ] && [ -f "$1" ]; then
		cloc_version=$(jq -r '.header.cloc_version // ""' "$1")
	fi
	# Never start cloc just to name it: the paths that print the header without having run it
	# (no changes, no git) should stay instant.
	if [ -n "$cloc_version" ]; then
		printf 'saas-framework LOC report — cloc %s, blank lines and comments excluded\n' "$cloc_version"
	else
		printf 'saas-framework LOC report — blank lines and comments excluded\n'
	fi
}

# --- classifiers, shared by both awk passes -------------------------------------------------
AWK_LIB='
function grp(p) {
	if (p ~ /^server\//) return "server"
	if (p ~ /^crates\//) return "crates"
	if (p ~ /^adapters\//) return "adapters"
	return "other"
}
function pkg(p,   a, n) {
	if (p ~ /^(crates|adapters)\//) {
		n = split(p, a, "/")
		if (n >= 2) return a[1] "/" a[2]
	}
	return grp(p)
}
# Integration tests are whole files under a package tests/ directory. Unit tests live inside the
# source files and are counted by AWK_INLINE instead.
function istest(p) { return (p ~ /\/tests\//) }
function short(k,   i) { i = index(k, "/"); return i ? substr(k, i + 1) : k }
function bar(n,   s) { s = ""; while (n-- > 0) s = s "-"; return s }
'

# Locates `#[cfg(test)]` items in a comment-stripped Rust file and reports how many code lines they
# occupy, per file. Reading the *stripped* copy is what makes this safe: a `#[cfg(test)]` written
# inside a comment or a doc-comment is already gone, so only real attributes are seen.
#
# Only top-level items are recognised — their attribute sits in column 0, so the item ends at the
# first line that is a bare `}` in column 0. That is exact for rustfmt output and, unlike brace
# counting, cannot be fooled by a brace inside a string literal. Test-only helpers nested in an
# `impl` block therefore stay counted as source.
AWK_INLINE='
function flush() { if (intest && n > 0) tot[f] += n; intest = 0; n = 0 }
FNR == 1 {
	flush()
	f = FILENAME
	sub(/\.nc$/, "", f)
	if (index(f, pre) == 1) f = substr(f, length(pre) + 1)
}
{
	if (!intest && /^#\[cfg\(test\)\]/) { intest = 1; brace = 0; n = 0 }
	if (!intest) next
	n++
	if (!brace && index($0, "{")) brace = 1
	if (brace && /^}[ \t]*$/) { tot[f] += n; intest = 0; n = 0 }
	else if (!brace && !/^#\[/ && /;[ \t]*$/) { tot[f] += n; intest = 0; n = 0 }
}
END {
	flush()
	for (f in tot) if (tot[f] > 0) printf "%s\t%d\n", f, tot[f]
}
'

# --- totals pass ----------------------------------------------------------------------------
if [ "$do_totals" -eq 1 ]; then
	SRC_DIRS=''
	for d in server/src crates/*/src adapters/*/src crates/*/tests adapters/*/tests; do
		if [ -d "$d" ]; then SRC_DIRS="$SRC_DIRS $d"; fi
	done
	[ -n "$SRC_DIRS" ] || die 'no source directories found — is this the repo root?'

	# cloc's --strip-comments writes its output next to the input, so it runs against a copy of the
	# tree rather than the repo itself. --original-dir preserves the tree shape; without it every
	# file would flatten into one directory, where the many mod.rs would collide.
	mkdir -p "$tmp/src"
	tar -cf - $SRC_DIRS | tar -x -C "$tmp/src"
	run_cloc "$tmp/src" $CLOC_OPTS --strip-comments=nc --original-dir > "$tmp/totals.json"
	print_header "$tmp/totals.json"

	find "$tmp/src" -name '*.nc' -exec awk -v pre="$tmp/src/" "$AWK_INLINE" {} + > "$tmp/inline"

	jq -r --arg pre "$tmp/src/" 'to_entries[] | select(.key != "header" and .key != "SUM")
		| [(.key | ltrimstr($pre)), .value.code] | @tsv' "$tmp/totals.json" \
		| sort \
		| awk -F'\t' -v bypkg="$by_package" -v inlfile="$tmp/inline" "$AWK_LIB"'
function add(g, k, m, v) { V[m,g] += v; P[m,k] += v }
# Not the usual FNR == NR: that idiom mistakes the second input for the first whenever the first
# one is empty, which is exactly the case for a tree with no #[cfg(test)] items in it at all.
FILENAME == inlfile { inl[$1] = $2 + 0; next }
{
	p = $1; sub(/^\.\//, "", p)
	c = $2 + 0
	g = grp(p); k = pkg(p)
	if (istest(p)) {
		add(g, k, "gf", 1); add(g, k, "gl", c)
	} else {
		t = inl[p]
		if (t > c) t = c
		add(g, k, "sf", 1); add(g, k, "sl", c - t)
		if (t > 0) { add(g, k, "xf", 1); add(g, k, "xl", t) }
	}
	if (!(k in kgrp)) { kgrp[k] = g; korder[++nk] = k }
	if (g == "other") anyothergrp = 1
}
function srow(label, f, c) { printf "%-24s%8d%10d\n", label, f, c }
function trow(label, f, i, x, t) { printf "%-24s%8d%10d%10d%10d\n", label, f, i, x, t }
function pkgrows(t, g,   k, key) {
	if (!bypkg || g == "server") return
	for (k = 1; k <= nk; k++) {
		key = korder[k]
		if (kgrp[key] != g) continue
		if (t == "s") {
			if (P["sf",key]) srow("  " short(key), P["sf",key], P["sl",key])
		} else if (P["xf",key] + P["gf",key]) {
			trow("  " short(key), P["xf",key] + P["gf",key], P["xl",key], \
				P["gl",key], P["xl",key] + P["gl",key])
		}
	}
}
END {
	ngrp = split("server crates adapters other", groups, " ")

	printf "\nSOURCE (tests excluded)\n"
	printf "%-24s%8s%10s\n", "group", "files", "code"
	printf "%s\n", bar(42)
	for (i = 1; i <= ngrp; i++) {
		g = groups[i]
		if (!V["sf",g]) continue
		srow(g, V["sf",g], V["sl",g])
		pkgrows("s", g)
		sf += V["sf",g]; sl += V["sl",g]
	}
	printf "%s\n", bar(42)
	srow("total", sf, sl)

	printf "\nTESTS  (inline = #[cfg(test)] items in the source files, integr. = tests/*.rs)\n"
	printf "%-24s%8s%10s%10s%10s\n", "group", "files", "inline", "integr.", "total"
	printf "%s\n", bar(62)
	for (i = 1; i <= ngrp; i++) {
		g = groups[i]
		if (!(V["xf",g] + V["gf",g])) continue
		trow(g, V["xf",g] + V["gf",g], V["xl",g], V["gl",g], V["xl",g] + V["gl",g])
		pkgrows("t", g)
		xf += V["xf",g]; xl += V["xl",g]
		gf += V["gf",g]; gl += V["gl",g]
	}
	printf "%s\n", bar(62)
	trow("total", xf + gf, xl, gl, xl + gl)

	# Every file is either a source file or an integration test file, so those two counts add up.
	# The inline-test files are a subset of the source files and must not be added again.
	printf "\nSUMMARY\n"
	printf "%-30s%8s%10s\n", "", "files", "code"
	printf "%-30s%8d%10d\n", "source", sf, sl
	printf "%-30s%8d%10d\n", "tests", xf + gf, xl + gl
	printf "%-30s%8d%10d\n", "  inline in source files", xf, xl
	printf "%-30s%8d%10d\n", "  integration test files", gf, gl
	printf "%s\n", bar(48)
	printf "%-30s%8d%10d\n", "all (distinct files)", sf + gf, sl + xl + gl
	printf "%-30s%18s\n", "tests as share of all", \
		(sl + xl + gl ? sprintf("%d%%", int(100 * (xl + gl) / (sl + xl + gl) + 0.5)) : "-")
}
' "$tmp/inline" -
fi

[ "$do_changes" -eq 1 ] || exit 0

# --- local changes pass ---------------------------------------------------------------------
# Working tree vs. HEAD, staged or not. Only touched files are handed to cloc, so this stays
# proportional to the size of the change rather than to the size of the repo.
if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
	print_header
	printf '\nLOCAL CHANGES  — skipped: not inside a git work tree\n'
	exit 0
fi
if ! git rev-parse --verify HEAD >/dev/null 2>&1; then
	print_header
	printf '\nLOCAL CHANGES  — skipped: HEAD does not resolve (no commits yet)\n'
	exit 0
fi

in_scope() {
	case "$1" in
		server/src/*|crates/*/src/*|adapters/*/src/*|crates/*/tests/*|adapters/*/tests/*) ;;
		*) return 1 ;;
	esac
	case "$1" in
		*.rs) return 0 ;;
		*) return 1 ;;
	esac
}

# Every touched, in-scope path: tracked changes vs. HEAD (staged or not) plus untracked files.
{
	git diff --name-only -z --diff-filter=ACMRD HEAD
	git ls-files --others --exclude-standard -z
} | tr '\0' '\n' |
while IFS= read -r p; do
	[ -n "$p" ] || continue
	in_scope "$p" || continue
	printf '%s\n' "$p"
done | sort -u > "$tmp/paths"

no_changes() {
	print_header
	printf '\nLOCAL CHANGES  (working tree vs HEAD)\n'
	printf '               —  no in-scope code changes\n'
	exit 0
}

[ -s "$tmp/paths" ] || no_changes

# Materialise both sides of the comparison, then let cloc strip blank and commented lines from
# each. Diffing the *stripped* trees is what makes this a code-only diff.
#
# Why not `cloc --diff`: it is comment-aware on its own, but its Perl differ costs seconds per
# heavily-rewritten file and cannot be parallelised — the npm distribution has no
# Parallel::ForkManager, so --processes is unavailable. Stripping is the cheap half of cloc's work,
# and GNU diff then does the comparison in well under a second for the whole tree.
#
# Both trees are populated with tar so that no per-file process is spawned, and no path is ever
# word-split: `git archive` streams all of HEAD and tar extracts only the listed members.
# Split the touched paths by which side they exist on. Membership is tested by exact string in awk
# rather than with comm: comm demands both inputs in *its* idea of sorted order, which is not always
# the order sort(1) produced, and it aborts rather than degrading when the two disagree. `ls-tree -z`
# keeps git from backslash-quoting unusual paths into a spelling the other lists never use.
git ls-tree -r -z --name-only HEAD | tr '\0' '\n' > "$tmp/head-paths"
awk -v hf="$tmp/head-paths" 'FILENAME == hf { h[$0] = 1; next } ($0 in h)' \
	"$tmp/head-paths" "$tmp/paths" > "$tmp/paths-head"
while IFS= read -r p; do
	if [ -f "$p" ]; then printf '%s\n' "$p"; fi
done < "$tmp/paths" > "$tmp/paths-work"

mkdir -p "$tmp/a" "$tmp/b"
if [ -s "$tmp/paths-head" ]; then
	git archive HEAD | tar -x -C "$tmp/a" -T "$tmp/paths-head"
fi
if [ -s "$tmp/paths-work" ]; then
	tar -cf - -T "$tmp/paths-work" | tar -x -C "$tmp/b"
fi

# Resolve the version up front: it feeds the report header, and for `pnpm dlx` it also warms the
# package cache so the two concurrent runs below cannot race on populating it. Already warm (and
# already known) when the totals pass ran.
if [ -z "$cloc_version" ]; then
	cloc_version=$(run_cloc --version 2>/dev/null | tail -n 1)
fi
print_header

# --strip-comments=nc writes <file>.nc next to each input with blank and commented lines removed;
# --original-dir keeps it beside the original instead of flattening into the cwd.
for side in a b; do
	run_cloc "$tmp/$side" --include-ext=rs --exclude-dir=target \
		--strip-comments=nc --original-dir --quiet --timeout 0 --skip-uniqueness \
		>/dev/null 2>&1 &
done
wait

find "$tmp/a" "$tmp/b" -type f ! -name '*.nc' -delete

# diff -N treats a file missing on one side as empty, so additions and deletions are counted in
# full; -U0 keeps the hunk headers tight. Exit status 1 just means "differences found".
diff -r -U0 -N "$tmp/a" "$tmp/b" > "$tmp/diff" 2>/dev/null || [ $? -eq 1 ] \
	|| die 'diff failed while comparing the stripped trees'

[ -s "$tmp/diff" ] || no_changes

# Counts come from the hunk headers (@@ -old,N +new,M @@) rather than from counting +/- lines, so
# a code line that happens to start with '+' or '-' can never be mistaken for diff syntax.
awk -v pre="$tmp/b/" -v bypkg="$by_package" "$AWK_LIB"'
/^\+\+\+ /{
	# Real headers name a file inside the new-side tree; anything else is an added line that
	# merely looks like one, and is ignored either way.
	h = substr($0, 5)
	sub(/\t.*$/, "", h)
	if (index(h, pre) != 1) next
	f = substr(h, length(pre) + 1)
	sub(/\.nc$/, "", f)
	next
}
/^@@ /{
	# @@ -l[,c] +l[,c] @@ — a missing count means exactly one line.
	old = $2; new = $3
	sub(/^-/, "", old); sub(/^\+/, "", new)
	r = (index(old, ",") ? substr(old, index(old, ",") + 1) : 1) + 0
	a = (index(new, ",") ? substr(new, index(new, ",") + 1) : 1) + 0
	if (a + r == 0) next
	if (!(f in fseen)) { fseen[f] = 1; F[grp(f)]++; PF[pkg(f)]++; F["TOTAL"]++ }
	A[grp(f)] += a; R[grp(f)] += r
	PA[pkg(f)] += a; PR[pkg(f)] += r
	A["TOTAL"] += a; R["TOTAL"] += r
	if (!(pkg(f) in kgrp)) { kgrp[pkg(f)] = grp(f); korder[++nk] = pkg(f) }
	seen[grp(f)] = 1
	any = 1
}
function row(label, f, a, r) { printf "%-24s%8d%9d%9d\n", label, f, a, r }
END {
	printf "\nLOCAL CHANGES  (working tree vs HEAD, staged or not; untracked files included)\n"
	printf "               added/removed code lines — blank and commented lines excluded\n"
	if (!any) {
		printf "               —  no in-scope code changes\n"
		exit
	}
	printf "%-24s%8s%9s%9s\n", "group", "files", "+", "-"
	printf "%s\n", bar(50)
	ngrp = split("server crates adapters other", groups, " ")
	for (i = 1; i <= ngrp; i++) {
		g = groups[i]
		if (!seen[g]) continue
		row(g, F[g], A[g], R[g])
		if (!bypkg || g == "server") continue
		for (k = 1; k <= nk; k++) {
			if (kgrp[korder[k]] != g) continue
			row("  " short(korder[k]), PF[korder[k]], PA[korder[k]], PR[korder[k]])
		}
	}
	printf "%s\n", bar(50)
	row("total", F["TOTAL"], A["TOTAL"], R["TOTAL"])
}
' "$tmp/diff"
