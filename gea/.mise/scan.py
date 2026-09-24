#!/usr/bin/env python3
"""Source-scanning primitives for the gea ratchets (`mise run panic-check` etc.).

This is the gea analogue of kopiur's `crates/xtask/src/scan.rs`, and it makes the
same trade deliberately: it reads the workspace's `.rs` files as *text* and never
parses Rust. A parser is a dependency, a build-time cost, and a new way for a gate
to fail on valid source. Text scanning can only ever be conservative, which is the
property a ratchet needs -- an over-removal makes the count too low, never too
high, so the gate can nag but cannot cry wolf.

Three scrubbing passes, matching scan.rs:

  scrub()          -- strip comments and string/char literals, so a name that
                      appears only in a doc comment or an error message is not
                      mistaken for code. (`checks.rs` mentions `std::process::exit`
                      twice in prose for every one time it calls it.)
  strip_cfg_test() -- strip `#[cfg(test)]` items by brace matching, so test
                      fixtures do not count as shipping code.
  sources()        -- the `.rs` walker, excluding `generated/` and `tests/`.

Run any check with `--list` to print the offending file:line rows instead of just
the count; that is how you drain one.

Deliberately depends on nothing but a stock python3 (already assumed by this
repo's CI). Keep it that way: a lint gate that needs its own install step is a
lint gate people delete.
"""

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CRATES = os.path.join(ROOT, "crates")

# Crates that are test infrastructure rather than shipping code. `gea-itest` is
# `publish = false` and its `src/lib.rs` is, by its own doc comment, "Harness for
# tests that run against a real Gitea": it boots a container, bootstraps a
# token and tears it down. An unwrap there fails a test loudly, which is the
# behaviour you want — the same reason `tests/` directories are excluded. It is
# named here rather than silently skipped so the carve-out is reviewable; only
# the panic check honours it (see `check_panic`), because the other two checks
# ask questions that are still meaningful about a harness.
TEST_CRATES = ("gea-itest",)


# --- scrubbing -------------------------------------------------------------


def scrub(src: str, keep_quotes: bool = False) -> str:
    """Replace comments and string/char literals with spaces, preserving newlines.

    Handles line comments (`//`, `///`, `//!`), nesting-aware block comments, raw
    strings (`r"..."`, `r#"..."#`, ...), normal strings with escapes, and byte
    strings. Line numbers over the result still name the right source line.

    `keep_quotes` blanks a string literal's *interior* but leaves its delimiters
    standing, so `"main"` becomes `"    "` while `""` stays `""`. The zero-value
    check needs that one bit -- it has to tell `unwrap_or("")` from
    `unwrap_or("main")` -- and this is the cheapest way to expose it without
    exposing the contents: prose inside a literal still cannot match anything,
    and a literal that is merely full of spaces is not empty and does not match
    either. Offsets and line numbers are unchanged, so the flag is invisible to
    every caller that does not ask for it.
    """
    out = []
    i, n = 0, len(src)

    def blank(span: str) -> None:
        out.append("".join(c if c == "\n" else " " for c in span))

    def blank_str(span: str) -> None:
        # Keep the outer delimiters, blank everything between them.
        if keep_quotes and len(span) >= 2:
            out.append(span[0])
            blank(span[1:-1])
            out.append(span[-1])
        else:
            blank(span)

    while i < n:
        c = src[i]
        if c == "/" and src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            blank(src[i:j])
            i = j
            continue
        if c == "/" and src.startswith("/*", i):
            start, depth, i = i, 1, i + 2
            while i < n and depth:
                if src.startswith("/*", i):
                    depth += 1
                    i += 2
                elif src.startswith("*/", i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            blank(src[start:i])
            continue
        # Raw string: r"..." / br#"..."# / ...
        m = re.match(r'(?:b?r)(#*)"', src[i:])
        if m and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")):
            hashes = m.group(1)
            close = '"' + hashes
            j = src.find(close, i + m.end())
            j = n if j < 0 else j + len(close)
            blank_str(src[i:j])
            i = j
            continue
        if c == '"':
            start, i = i, i + 1
            while i < n:
                if src[i] == "\\":
                    i += 2
                    continue
                if src[i] == '"':
                    i += 1
                    break
                i += 1
            blank_str(src[start:i])
            continue
        # Char literal, but not a lifetime (`'a`) and not a label (`'outer:`).
        if c == "'" and re.match(r"'(?:\\.|[^\\'])'", src[i:]):
            j = i + len(re.match(r"'(?:\\.|[^\\'])'", src[i:]).group(0))
            blank(src[i:j])
            i = j
            continue
        out.append(c)
        i += 1
    return "".join(out)


def strip_cfg_test(src: str) -> str:
    """Blank out `#[cfg(test)]`-annotated items by brace matching.

    Line numbers survive, so a caller can still report file:line against the
    original. `mod tests { ... }` without the attribute is left alone on purpose:
    the attribute is the thing that means "not shipped".
    """
    out = list(src)
    for m in re.finditer(r"#\[cfg\(test\)\]", src):
        i = src.find("{", m.end())
        if i < 0:
            continue
        # Nothing but an item header may sit between the attribute and the brace.
        if "}" in src[m.end():i] or ";" in src[m.end():i]:
            continue
        depth, j = 0, i
        while j < len(src):
            if src[j] == "{":
                depth += 1
            elif src[j] == "}":
                depth -= 1
                if depth == 0:
                    j += 1
                    break
            j += 1
        for k in range(m.start(), min(j, len(src))):
            if out[k] != "\n":
                out[k] = " "
    return "".join(out)


def _cfg_test_modules():
    """Files that are a `#[cfg(test)] mod name;` submodule of another file.

    `strip_cfg_test` only sees attributes in the file it is given, so a test
    module living in its OWN file -- `#[cfg(test)] mod tests;` next to
    `tests.rs` -- would otherwise count as shipping code. (It is not
    hypothetical: `gitea-core/src/context/git/tests.rs` is 54 unwraps of
    perfectly good test fixture.) Resolve the declaration to the path Rust
    would, and drop the whole subtree.
    """
    decl = re.compile(r"#\[cfg\(test\)\]\s*(?:pub\s+)?mod\s+(\w+)\s*;")
    out = set()
    for base, dirs, files in os.walk(CRATES):
        dirs[:] = [d for d in dirs if d != "generated"]
        for f in files:
            if not f.endswith(".rs"):
                continue
            path = os.path.join(base, f)
            with open(path, encoding="utf-8", errors="replace") as fh:
                src = scrub(fh.read())
            for name in decl.findall(src):
                owner = base if f in ("mod.rs", "lib.rs", "main.rs") else os.path.join(base, f[:-3])
                out.add(os.path.join(owner, name + ".rs"))
                out.add(os.path.join(owner, name))
    return out


def sources(skip_tests_dirs: bool = True, skip_test_crates: bool = False):
    """Every `.rs` file under crates/, minus generated output and test trees.

    `generated/` is excluded because it is machine-written: a ratchet over it
    would be a ratchet over the code generator's taste, and the drift guard
    (`cargo xtask codegen --check`) already owns that file tree. `tests/` is
    excluded because a test that unwraps is a test that fails loudly, which is
    the desired behaviour there.
    """
    cfg_test = _cfg_test_modules()
    excluded = tuple(os.path.join(CRATES, c) + os.sep for c in TEST_CRATES)
    for base, dirs, files in os.walk(CRATES):
        if skip_test_crates and (base + os.sep).startswith(excluded):
            dirs[:] = []
            continue
        dirs[:] = [
            d
            for d in sorted(dirs)
            if d != "generated"
            and not (skip_tests_dirs and d == "tests")
            and os.path.join(base, d) not in cfg_test
        ]
        for f in sorted(files):
            if not f.endswith(".rs"):
                continue
            path = os.path.join(base, f)
            if skip_tests_dirs and path in cfg_test:
                continue
            yield path


def rel(path: str) -> str:
    return os.path.relpath(path, ROOT)


def read(path: str, cfg_test: bool = True, keep_quotes: bool = False) -> str:
    with open(path, encoding="utf-8", errors="replace") as fh:
        src = fh.read()
    src = scrub(src, keep_quotes=keep_quotes)
    if cfg_test:
        src = strip_cfg_test(src)
    return src


# --- the checks ------------------------------------------------------------

PANIC_RE = re.compile(r"\bunwrap\(\)|\bexpect\(|\bpanic!|\btodo!|\bunimplemented!")


def check_panic():
    """Count panicking constructs in shipping (non-test, non-generated) code."""
    hits = []
    for path in sources(skip_test_crates=True):
        src = read(path)
        for lineno, line in enumerate(src.splitlines(), 1):
            for m in PANIC_RE.finditer(line):
                hits.append((rel(path), lineno, m.group(0)))
    return hits


def check_exit():
    """Count `std::process::exit` calls outside the one place that owns them."""
    owner = os.path.join(CRATES, "gea", "src", "main.rs")
    hits = []
    for path in sources():
        if os.path.abspath(path) == owner:
            continue
        src = read(path)
        for lineno, line in enumerate(src.splitlines(), 1):
            if "std::process::exit" in line or re.search(r"\bprocess::exit\s*\(", line):
                hits.append((rel(path), lineno, "process::exit"))
    return hits


def check_support():
    """Count files outside cmd/admin/ that import the admin group's support module."""
    admin = os.path.join(CRATES, "gea", "src", "cmd", "admin") + os.sep
    hits = []
    for path in sources(skip_tests_dirs=False):
        if os.path.abspath(path).startswith(admin):
            continue
        # cfg(test) imports count: a test-only reach into another group's private
        # module is still an edge in the module graph, and the target is zero.
        src = read(path, cfg_test=False)
        for lineno, line in enumerate(src.splitlines(), 1):
            if "crate::cmd::admin::support" in line:
                hits.append((rel(path), lineno, "admin::support"))
                break  # one per file: the unit here is the module, not the import
    return hits



# --- the zero-value-in-a-request-body check --------------------------------
#
# The bug this counts is small to write and expensive to ship. A generated
# request-body model spells a not-required field `Option<T>` with
# `skip_serializing_if = "Option::is_none"`, so a field the user never set is
# OMITTED from the JSON. That is not a style choice: it is the fix for a
# data-loss defect (commit b62c3d1), where every PATCH was a full overwrite that
# blanked whatever the user had not mentioned. A call site that writes
# `Some(x.unwrap_or_default())` hands the model the exact zero value the
# `Option` existed to omit, and the overwrite comes straight back.
#
# The live symptom was `gea repo fork` answering `500 name is empty`, because
# the call site turned "no --fork-name" into `Some("")`. No test caught it and
# no test could have: `FakeTransport` accepts any body, so only an assertion on
# the SHAPE of the request -- `sent.get("name").is_none()` -- would fail, and
# almost none exist. This is a class the suite is structurally blind to, which
# is what a ratchet is for.
#
# WHY THIS IS TYPE-SCOPED AND NOT A GREP. A bare search for
# `Some(...unwrap_or_default())` is worse than no gate: this tree has
# `GlobalOpts { json: Some(String::new()) }`, `PageInfo { total_count: ... }`,
# `ResolveOptions`, and query clamps like
# `Some(i32::try_from(limit).unwrap_or(i32::MAX))`, none of which is the bug.
# So the set of fields that can be wrong is DERIVED from the generated models
# (`request_body_fields`): a field only counts if it really carries
# `skip_serializing_if = "Option::is_none"`. Query structs are not body models
# and drop out entirely; when the spec changes, the set follows codegen instead
# of rotting in a literal here.


def _category(inner: str) -> str:
    """Bucket an omittable field's inner type. The bucket decides one thing only:
    whether an UNCONDITIONAL zero literal is a defect or ordinary API usage."""
    t = inner.strip()
    if t == "String":
        return "string"
    if t in ("bool", "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "f32", "f64"):
        return "scalar"
    if t.startswith(("Vec<", "BTreeMap<", "HashMap<", "BTreeSet<", "HashSet<")):
        return "collection"
    return "other"


_MODEL_STRUCT = re.compile(r"pub struct (\w+) \{(.*?)\n\}", re.S)
_MODEL_FIELD = re.compile(
    r'skip_serializing_if = "Option::is_none"\)\]\s*\n\s*pub (\w+): Option<(.+?)>,'
)


def request_body_fields():
    """`{TypeName: {field: category}}` for every field in the workspace whose
    `Option` exists to be OMITTED.

    Derived, not listed. The membership test is the serde attribute itself --
    `skip_serializing_if = "Option::is_none"` on an `Option<T>` -- because that
    attribute IS the contract being protected; a naming convention would be a
    guess, and a literal list here would rot the first time the spec moved.
    Two consequences worth stating:

    * 119 of the generated models qualify and the rest do not, which is exactly
      the Response/RequestBody split the emitter makes (b62c3d1). Query structs
      have no such attribute, so the whole `Some(...unwrap_or(i32::MAX))` clamp
      family drops out of the scan before any expression is looked at.
    * the HAND-WRITTEN patch bodies qualify too -- `IssuePatch`, `LabelPatch`,
      `MilestonePatch`, `ReleasePatch` in `cmd/issue/shared.rs` are sparse PATCH
      bodies with the identical contract, and they are where the overwrite bug
      would hurt most. Scoping to `gitea-model/src/generated/` would have left
      them uncovered.
    """
    out = {}
    # Its own walk, not `sources()`: that one skips `generated/`, which is
    # correct for call sites and exactly wrong here -- the generated models are
    # most of what defines the contract.
    for base, dirs, files in os.walk(CRATES):
        dirs[:] = sorted(dirs)
        for f in sorted(files):
            if not f.endswith(".rs"):
                continue
            with open(os.path.join(base, f), encoding="utf-8", errors="replace") as fh:
                src = fh.read()
            if "skip_serializing_if" not in src:
                continue
            for m in _MODEL_STRUCT.finditer(src):
                fields = {n: _category(t) for n, t in _MODEL_FIELD.findall(m.group(2))}
                if fields:
                    out.setdefault(m.group(1), {}).update(fields)
    return out


# A value indistinguishable from "the user said nothing". `""` survives scrubbing
# only because `keep_quotes` leaves the delimiters standing; a literal with any
# content scrubs to `"   "`, which does not match.
_ZERO_LITERAL = re.compile(
    r"""^(?:
        String::new\(\) | String::default\(\) | String::from\(""\)
      | ""(?:\s*\.\s*(?:to_string|to_owned|into)\(\))?
      | Vec::new\(\) | Vec::default\(\) | vec!\[\s*\]
      | (?:BTreeMap|HashMap|BTreeSet|HashSet)::new\(\)
      | (?:[\w:]+::)?default\(\) | <[^<>]+>::default\(\)
      | 0(?:_?[iuf]\d+)? | 0\.0 | false
    )$""",
    re.X,
)
# Callables that PRODUCE one, for `unwrap_or_else`.
_ZERO_FN = re.compile(r"^(?:String|Vec|BTreeMap|HashMap|BTreeSet|HashSet|Default)::(?:new|default)$")


def _matching(s: str, i: int) -> int:
    """Index of the bracket closing the one at s[i], or -1."""
    pairs = {"(": ")", "[": "]", "{": "}"}
    close, depth = pairs[s[i]], 0
    for j in range(i, len(s)):
        if s[j] in pairs:
            depth += 1
        elif s[j] in ")]}":
            depth -= 1
            if depth == 0:
                return j if s[j] == close else -1
    return -1


def _split_top(s: str, sep: str = ","):
    """Split on `sep` at bracket depth 0. Angle brackets are NOT tracked: `<` is
    also a comparison operator, and guessing wrong there would invent syntax. A
    generic with a comma in it therefore splits one expression into fragments
    that simply fail to match -- an undercount, which is the direction a ratchet
    is allowed to be wrong in."""
    out, cur, depth = [], "", 0
    for ch in s:
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        if ch == sep and depth == 0:
            out.append(cur)
            cur = ""
        else:
            cur += ch
    out.append(cur)
    return out


def _tail_call(e: str):
    """`(method, arg_text)` if `e` is a method call in RESULT position -- i.e. the
    whole expression is `<receiver>.method(...)`. Anything nested deeper is not
    what the field receives, so `Some(xs.map(|x| x.unwrap_or_default()).collect())`
    is not a hit."""
    if not e.endswith(")"):
        return None
    open_paren = -1
    depth = 0
    for j in range(len(e) - 1, -1, -1):
        if e[j] == ")":
            depth += 1
        elif e[j] == "(":
            depth -= 1
            if depth == 0:
                open_paren = j
                break
    if open_paren < 0:
        return None
    head = e[:open_paren]
    m = re.search(r"\.\s*(\w+)\s*$", head)
    if not m:
        return None
    # The dot must itself sit at depth 0 of the whole expression.
    depth = 0
    for c in head[: m.start()]:
        if c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
    if depth != 0:
        return None
    return m.group(1), e[open_paren + 1 : -1]


def _blocks(e: str):
    """Every brace group at depth 0 -- the arms of an `if` chain or a `match`."""
    out, i = [], 0
    while i < len(e):
        if e[i] == "{":
            j = _matching(e, i)
            if j < 0:
                break
            out.append(e[i + 1 : j])
            i = j + 1
        elif e[i] in "([":
            j = _matching(e, i)
            i = len(e) if j < 0 else j + 1
        else:
            i += 1
    return out


def _tail_expr(block: str) -> str:
    """The value a block evaluates to: the text after its last top-level `;`."""
    return _split_top(block, ";")[-1].strip()


def zero_kind(e: str, _depth: int = 0):
    """`"fallback"`, `"literal"`, or None for an expression in value position.

    `fallback`  -- an absence is being collapsed into a zero. This IS the bug,
                   whatever the field's type: `unwrap_or_default()`,
                   `unwrap_or("")`, `unwrap_or_else(String::new)`, or an
                   `if`/`match` with a zero arm.
    `literal`   -- an unconditional zero constant. The bug only for a string or
                   a nested model; for a collection or a number an explicit
                   empty is ordinary API usage (see the caller).
    """
    if _depth > 8:
        return None
    e = " ".join(e.split())
    while e.startswith("(") and _matching(e, 0) == len(e) - 1:
        e = e[1:-1].strip()
    if not e:
        return None
    if e.startswith("{") and _matching(e, 0) == len(e) - 1:
        return zero_kind(_tail_expr(e[1:-1]), _depth + 1)
    if re.match(r"^if\b", e) or re.match(r"^match\b", e):
        arms = []
        for b in _blocks(e):
            if re.match(r"^if\b", e):
                arms.append(_tail_expr(b))
            else:
                for part in _split_top(b):
                    if "=>" in part:
                        arms.append(part.split("=>", 1)[1].strip())
        # A conditional that can yield a zero is collapsing a case into "unset".
        return "fallback" if any(zero_kind(a, _depth + 1) for a in arms) else None
    call = _tail_call(e)
    if call:
        name, arg = call
        arg = " ".join(arg.split())
        if name == "unwrap_or_default":
            return "fallback"
        if name == "unwrap_or" and _ZERO_LITERAL.match(arg):
            return "fallback"
        if name == "unwrap_or_else":
            body = re.sub(r"^\|[^|]*\|\s*", "", arg).strip()
            if _ZERO_FN.match(arg) or zero_kind(body, _depth + 1):
                return "fallback"
        # Any OTHER trailing method falls through to the literal test rather
        # than returning: `"".to_owned()` is a method call and is still a zero.
    return "literal" if _ZERO_LITERAL.match(e) else None


def option_fallback(e):
    """Is this an `Option` expression that turns `None` into a zero?

    `x.or(Some(String::new()))` and `x.or_else(|| Some(0))` are the same bug as
    `Some(x.unwrap_or_default())` -- an absence collapsed into a zero the model
    would otherwise omit -- but they are not spelled `Some(...)` at the top
    level, so the caller's `Some(` match skips them. They reach the field as an
    `Option`, which is exactly why they look innocent.

    Only a zero counts: `x.or(Some(default_branch))` is a real fallback value
    and none of this check's business.
    """
    call = _tail_call(e)
    if not call:
        return False
    name, arg = call
    arg = " ".join(arg.split())
    if name not in ("or", "or_else"):
        return False
    if name == "or_else":
        arg = re.sub(r"^\|[^|]*\|\s*", "", arg).strip()
    m = re.match(r"^Some\s*\((.*)\)$", arg, re.S)
    if not m or _matching(arg, arg.index("(")) != len(arg) - 1:
        return False
    return zero_kind(m.group(1).strip()) is not None


# A `Type {` that is a declaration or a destructuring pattern, not a literal.
_NOT_A_LITERAL = re.compile(r"(?:\b(?:struct|enum|union|trait|impl|let)\b|=>)\s*$")


def _let_bindings(src: str):
    """`[(offset, name, rhs)]` for every `let` in the file, so a body field
    initialised from a local (`AddTimeOption { .., user_name }`) can still be
    read. That spelling is not hypothetical: it is how `times/mod.rs` holds the
    value that used to be `Some(String::new())`."""
    out = []
    for m in re.finditer(r"\blet\s+(?:mut\s+)?(\w+)\s*(?::[^=;]*)?=\s*", src):
        rest = src[m.end() :]
        end = len(_split_top(rest, ";")[0])
        out.append((m.start(), m.group(1), rest[:end]))
    return out


def read_text(src: str) -> str:
    """The scrubbing pipeline applied to a string rather than a file, so the
    self-test's fixtures go through exactly what real source goes through."""
    return strip_cfg_test(scrub(src, keep_quotes=True))


def _body_locals(src: str, omit):
    """`[(offset, name, Type)]` for locals visibly bound to a request-body type.

    A sparse PATCH body is usually built by assignment rather than in one
    literal -- `let mut patch = IssuePatch::default(); ... patch.title =
    Some(t);` -- so the literal scan alone would not see it. The receiver's type
    has to be RESOLVED rather than guessed: matching on the field name alone
    would flag `out.name`, `ctx.path` and `self.branch`, none of which is a
    request body, and a gate with false positives is one somebody deletes.
    """
    names = "|".join(sorted(omit, key=len, reverse=True))
    pat = re.compile(
        r"\blet\s+(?:mut\s+)?(\w+)\s*(?::\s*(%s)\s*)?=\s*(%s)\s*(?:\{|::\s*default\s*\()"
        % (names, names)
    )
    return [(m.start(), m.group(1), m.group(3) or m.group(2)) for m in pat.finditer(src)]


def _nearest(bindings, name, before):
    prior = [b for b in bindings if b[1] == name and b[0] < before]
    return max(prior) if prior else None


def scan_text(src: str, omit):
    """`[(lineno, "Type.field")]` for one already-scrubbed file body."""
    names = "|".join(sorted(omit, key=len, reverse=True))
    if not names:
        return []
    literal = re.compile(r"\b(%s)\s*\{" % names)
    hits, lets = [], None
    for m in literal.finditer(src):
        ty = m.group(1)
        if _NOT_A_LITERAL.search(src[: m.start()].rstrip()):
            continue
        close = _matching(src, m.end() - 1)
        if close < 0:
            continue
        lineno = src.count("\n", 0, m.start()) + 1
        for part in _split_top(src[m.end() : close]):
            part = " ".join(part.split())
            if not part or part.startswith(".."):
                continue
            fm = re.match(r"^(\w+)\s*:\s*(.*)$", part)
            field, expr = (fm.group(1), fm.group(2)) if fm else (part, part)
            cat = omit[ty].get(field)
            if cat is None:
                continue
            if re.fullmatch(r"[A-Za-z_]\w*", expr):
                # Initialised from a local, as in
                # `AddTimeOption { created: None, time, user_name }`. Not
                # hypothetical: that is exactly how times/mod.rs now holds the
                # value that used to be built with `String::new()`, so a scan
                # that only reads inline expressions would miss a regression
                # there. Nearest preceding binding of the name wins.
                if lets is None:
                    lets = _let_bindings(src)
                prior = [b for b in lets if b[1] == expr and b[0] < m.start()]
                if not prior:
                    continue
                expr = max(prior)[2]
            expr = " ".join(expr.split())
            inner = re.match(r"^Some\s*\((.*)\)$", expr, re.S)
            # The paren opened by `Some(` must close at the very end, so
            # `Some(a) + f(b)` is not read as `Some(a) + f(b)` inside a `Some`.
            if not inner or _matching(expr, expr.index("(")) != len(expr) - 1:
                # Not a `Some(...)`, but an Option-typed expression can still
                # manufacture one from nothing.
                if option_fallback(expr):
                    hits.append((lineno, "%s.%s" % (ty, field)))
                continue
            kind = zero_kind(inner.group(1))
            if kind == "fallback" or (kind == "literal" and cat in ("string", "other")):
                hits.append((lineno, "%s.%s" % (ty, field)))

    # `patch.title = Some(...)` on a local whose type resolves to a body model.
    locals_ = _body_locals(src, omit)
    if locals_:
        for m in re.finditer(r"\b(\w+)\s*\.\s*(\w+)\s*=\s*Some\s*\(", src):
            bind = _nearest(locals_, m.group(1), m.start())
            if not bind:
                continue
            cat = omit.get(bind[2], {}).get(m.group(2))
            if cat is None:
                continue
            rhs = _split_top(src[m.end() - len("Some("):], ";")[0].strip()
            inner = re.match(r"^Some\s*\((.*)\)$", " ".join(rhs.split()), re.S)
            if not inner:
                continue
            kind = zero_kind(inner.group(1))
            if kind == "fallback" or (kind == "literal" and cat in ("string", "other")):
                hits.append(
                    (src.count("\n", 0, m.start()) + 1, "%s.%s" % (bind[2], m.group(2)))
                )
    return hits


def check_zero_body():
    """Count zero values written into request-body fields whose `Option` exists
    to omit them."""
    omit = request_body_fields()
    if not omit:
        return []
    hits = []
    for path in sources():
        # keep_quotes so `unwrap_or("")` is distinguishable from
        # `unwrap_or("main")`. Comments are still blanked, so a `///` example or
        # a quoted server message can never be counted as code.
        for lineno, what in scan_text(read(path, keep_quotes=True), omit):
            hits.append((rel(path), lineno, what))
    return hits


CHECKS = {
    "panic": check_panic,
    "exit": check_exit,
    "support": check_support,
    "zero": check_zero_body,
}


# --- self-test -------------------------------------------------------------
#
# `zero` is the only check here whose correctness is not obvious by reading it:
# the other three are a regex over scrubbed text, this one classifies Rust
# expressions. A scan that is wrong in EITHER direction makes the gate
# worthless -- a miss is a gate that never fires, and a false positive is a gate
# that the first person it inconveniences deletes. So the classifier is pinned
# by fixtures, and `zero-check` runs them before it counts anything.
#
# Every FLAG case below is a spelling that was actually found in this tree when
# the bug was fixed, and every SKIP case is something the tree actually contains
# today that must not count.

_ZERO_FIXTURES = [
    # (should_flag, field category, the expression inside `Some(...)`, why)
    (True, "string", "args.fork_name.clone().unwrap_or_default()", "repo fork: the 500"),
    (True, "string", "x.unwrap_or_else(String::new)", "unwrap_or_else spelling"),
    (True, "string", "x.unwrap_or_else(|| String::new())", "closure spelling"),
    (True, "string", 'x.unwrap_or("")', "empty-string fallback"),
    (True, "string", "String::new()", "admin/repo.rs: the bare literal"),
    (True, "string", '"".to_owned()', "same, other spelling"),
    (True, "string", "if seeded { s } else { String::new() }", "repo/create.rs"),
    (True, "string", "match x { None => String::new(), Some(v) => v }", "times/mod.rs"),
    (True, "collection", "a.rule.clone().unwrap_or_default()", "a fallback is the bug for any type"),
    (True, "scalar", "n.unwrap_or_default()", "likewise"),
    (True, "other", "Default::default()", "a nested model's own zero is still a zero"),
    (True, "string", "{ let t = s; t.unwrap_or_default() }", "block tail position"),
    (True, "other", "VisibilityMode::default()", "a named type's zero is still a zero"),
    # Must NOT flag: none of these substitutes a zero for "the user said nothing".
    (False, "string", "args.full_name.clone().unwrap_or_else(|| current.full_name.clone())", "org.rs read-modify-write"),
    (False, "string", "a.title.clone().unwrap_or_else(|| a.tag.clone())", "release.rs: name falls back to the tag"),
    (False, "string", 'args.message.clone().unwrap_or_else(|| format!("edit {}", t))', "wiki commit message"),
    (False, "scalar", "i32::try_from(limit).unwrap_or(i32::MAX)", "query clamp"),
    (False, "collection", "Vec::new()", "an explicit empty list is ordinary on a create"),
    (False, "scalar", "0", "admin/quota.rs: an explicit zero limit is documented and deliberate"),
    (False, "scalar", "false", "an explicit false is ordinary"),
    (False, "string", "current.description.clone()", "a real value"),
    (False, "collection", "xs.iter().map(|x| x.clone().unwrap_or_default()).collect()", "the zero is per element, not the field"),
    (False, "string", "if a { b } else { c }", "both arms are real values"),
    (False, "string", "s.unwrap_or_else(|| other.clone())", "a meaningful fallback"),
    (False, "string", "x.unwrap_or_default().len()", "the zero is not what the field receives"),
    (False, "string", "f(x.unwrap_or_default())", "same: not in result position"),
    (False, "string", 'x.as_deref().unwrap_or("main").to_owned()', "a real default branch name"),
    (False, "string", "x.default()", "a method called `default` on a value is not `T::default()`"),
]

# A file whose only mention of the pattern is PROSE. Three times in one session a
# text search matched a doc comment *about* a defect and reported it as the
# defect, so this is pinned rather than assumed. The `#[cfg(test)]` fixture at
# the bottom is the second half of the same point: a test may construct a
# deliberately-wrong body to assert the shape of the request.
_PROSE_FIXTURE = r"""
//! Do not write `CreateForkOption { name: Some(a.fork_name.unwrap_or_default()) }`:
//! that turns "unset" into `Some("")` and the server answers `500 name is empty`.
/// ```
/// let body = CreateForkOption { name: Some(String::new()) };  // wrong
/// ```
/* Block form too: CreateForkOption { name: Some(String::new()) } */
fn f(args: &Args) -> CreateForkOption {
    // Historically this said `name: Some(args.fork_name.clone().unwrap_or_default())`.
    CreateForkOption { name: args.fork_name.clone(), organization: None }
}
#[cfg(test)]
mod tests {
    fn fixture() -> CreateForkOption {
        CreateForkOption { name: Some(String::new()), organization: None }
    }
}
"""

_PROSE_CORRECT = "CreateForkOption { name: args.fork_name.clone(), organization: None }"
_PROSE_PLANTED = (
    "CreateForkOption { name: Some(args.fork_name.clone().unwrap_or_default()), organization: None }"
)



# A sparse PATCH body built by assignment, which is how `cmd/issue/shared.rs`'s
# hand-written patch types are actually used. Two of these four must fire and
# two must not: the receiver's TYPE decides, not the field name, because
# `out.title` and `ctx.body` are not request bodies at all.
_ASSIGN_FIXTURE = r"""
// Prose: `patch.title = Some(t.unwrap_or_default())` must not count.
fn a(t: Option<String>) -> IssuePatch {
    let mut patch = IssuePatch::default();
    patch.title = Some(t.clone().unwrap_or_default());   // FIRES
    patch
}
fn b(args: &Args) -> CreatePullRequestOption {
    let mut body = CreatePullRequestOption { title: args.title.clone(), ..Default::default() };
    body.body = Some(args.body.clone().unwrap_or_else(String::new));   // FIRES
    body
}
fn c(out: &mut Whatever, ctx: &mut Other, s: Option<String>) {
    out.title = Some(s.clone().unwrap_or_default());   // not a body
    ctx.body = Some(String::new());                    // not a body
}
fn d(args: &Args, current: &Repo) -> CreatePullRequestOption {
    CreatePullRequestOption {
        body: Some(args.body.clone().unwrap_or_else(|| current.body.clone())),   // meaningful
        ..Default::default()
    }
}
// `.or(Some(zero))` reaches the field as an Option, so it is not spelled `Some(` at the top
// level and the ordinary match skips it — while doing exactly what the check exists to stop.
// Found by planting it against the finished gate, which is the only way a hole like this shows.
fn e(args: &Args, source: &Repo) -> CreateForkOption {
    CreateForkOption {
        name: args.fork_name.clone().or(Some(String::new())),   // FIRES
        organization: args.org.clone().or_else(|| Some(String::new())),   // FIRES
    }
}
fn f(args: &Args, source: &Repo) -> CreateForkOption {
    CreateForkOption {
        // A real value, not a zero. Must not fire.
        name: args.fork_name.clone().or(Some(source.name.clone())),
        organization: args.org.clone(),
    }
}
"""


def selftest() -> int:
    bad = []
    for want, cat, expr, why in _ZERO_FIXTURES:
        kind = zero_kind(expr)
        got = kind == "fallback" or (kind == "literal" and cat in ("string", "other"))
        if got != want:
            bad.append(
                "%s: %r -> %r (%s)" % ("MISSED" if want else "FALSE POSITIVE", expr, kind, why)
            )

    omit = {"CreateForkOption": {"name": "string", "organization": "string"}}
    # Prose must contribute nothing: comments are scrubbed and the `#[cfg(test)]`
    # fixture is stripped, so the only thing left is the real, correct literal.
    found = scan_text(read_text(_PROSE_FIXTURE), omit)
    if found:
        bad.append("prose fixture: counted %r — a comment or a cfg(test) fixture was read as code" % found)
    # ...and the same file with the defect actually present must be counted, so
    # the line above is proof of scrubbing and not proof of a dead scan.
    planted = _PROSE_FIXTURE.replace(_PROSE_CORRECT, _PROSE_PLANTED)
    if not scan_text(read_text(planted), omit):
        bad.append("a defect planted in the prose fixture was NOT counted")

    # The assignment form, and the type-resolution that keeps it honest.
    assign_omit = {
        "IssuePatch": {"title": "string", "body": "string"},
        "CreatePullRequestOption": {"title": "string", "body": "string"},
        "CreateForkOption": {"name": "string", "organization": "string"},
    }
    got = sorted(what for _, what in scan_text(read_text(_ASSIGN_FIXTURE), assign_omit))
    want = [
        # `.or(Some(zero))` / `.or_else(|| Some(zero))`: an Option reaching the field already
        # wrapped, so it is never spelled `Some(` at the top level.
        "CreateForkOption.name",
        "CreateForkOption.organization",
        "CreatePullRequestOption.body",
        "IssuePatch.title",
    ]
    if got != want:
        bad.append("assignment fixture: got %r, want %r" % (got, want))

    # The field set must really come from the code. If codegen stops
    # emitting `skip_serializing_if`, this gate measures nothing, and it should
    # say so rather than print a comfortable 0.
    omit_real = request_body_fields()
    missing = [t for t in ("IssuePatch", "LabelPatch", "CreateForkOption") if t not in omit_real]
    if missing:
        bad.append(
            "%s carry `skip_serializing_if` but are not in the derived set; the "
            "hand-written patch bodies in cmd/issue/shared.rs are covered on purpose" % missing
        )
    if len(omit_real) < 50:
        bad.append(
            "only %d request-body types found in the workspace: the check would "
            "silently measure nothing" % len(omit_real)
        )

    for line in bad:
        sys.stderr.write("selftest: %s\n" % line)
    if bad:
        sys.stderr.write("selftest: %d failure(s); the zero-value scan is not trustworthy\n" % len(bad))
        return 1
    print(
        "selftest: ok (%d expression fixtures, prose/cfg(test)/assignment fixtures, "
        "%d request-body types)"
        % (len(_ZERO_FIXTURES), len(omit_real))
    )
    return 0


def main(argv):
    if len(argv) > 1 and argv[1] == "selftest":
        return selftest()
    if len(argv) < 2 or argv[1] not in CHECKS:
        sys.stderr.write("usage: scan.py {%s|selftest} [--list]\n" % "|".join(CHECKS))
        return 2
    hits = CHECKS[argv[1]]()
    if "--list" in argv[2:]:
        for path, lineno, what in hits:
            print("%s:%d: %s" % (path, lineno, what))
    else:
        print(len(hits))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
