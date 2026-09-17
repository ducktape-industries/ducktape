#!/usr/bin/env bash
# The binary embeds no wasm (AGENTS.md, "No Embedded Wasm"): an
# `include_bytes!`/`include_str!` of a `.wasm` is allowed only in a test — a
# file under a `tests/` directory or named `tests.rs`, or an item a
# `#[cfg(test)]` governs. Pure text, no toolchain.
#
# The scanner is item-aware because the attribute is. `#[cfg(test)]` governs
# ONE item: a `mod tests { … }` block (test until its brace closes) or a single
# `const`/`use`/`fn` (test until that item ends). A file does not become a test
# file because one constant in it is shrunk under `cfg(test)` —
# `crates/services/broker/src/lib.rs:78` does exactly that and carries 4,500
# lines of production code after it.
#
# Invocations may span lines, so an include is tracked from its macro name
# until its parentheses balance, and the `.wasm` may appear on any line in
# between.
#
# Structure is counted on the line with its string and char literals removed,
# because a brace inside a literal skews the depth in BOTH directions: a `{` in
# a test module holds the region open over the production code below it (a
# false green, the thing this gate exists to prevent), and a `}` in production
# closes a region early (a false refusal). Fixture 7 is the first of those.
#
# The remaining ceiling is a raw string with embedded quotes (`r#"…"#`), which
# the literal stripper reads as ordinary quotes. That is where a tripwire stops
# and a parser begins, and a parser means `syn` — a toolchain this gate exists
# to not need.
#
# `--self-test` alone runs the fixtures below and scans nothing.
set -euo pipefail

# Print every production `.wasm` include in one file as `path:line: text`.
# Exits 1 when it printed something, 0 when the file is clean.
scan_file() {
  awk -v file="$1" '
    BEGIN { depth = 0; pending = 0; test_depth = -1; test_item = 0; inc = 0; found = 0 }
    {
      line = $0
      sub(/\/\/.*/, "", line)

      # read the macro and its path off the text, literals intact
      starts_include = (line ~ /include_(bytes|str)!/)
      names_wasm     = (line ~ /\.wasm"/)

      # then drop the literals: a brace, paren or semicolon inside one is not
      # structure, and counting it as structure is what moves a test region
      # over production code
      code = line
      gsub(/"([^"\\]|\\.)*"/, "", code)
      gsub(/\047([^\047\\]|\\.)\047/, "", code)   # \047 is the char-literal quote

      nopen  = gsub(/\{/, "{", code)
      nclose = gsub(/\}/, "}", code)

      # `#[cfg(not(test))]` is production and must not match.
      if (code ~ /#\[cfg\(test\)\]/) pending = 1

      # what the attributes on this line govern
      rest = code
      gsub(/#\[[^]]*\]/, "", rest)

      is_test = (test_depth >= 0) || test_item || pending

      if (!inc && starts_include) {
        inc = 1; inc_line = NR; inc_text = $0; inc_wasm = 0; inc_test = is_test; paren = 0
      }
      if (inc) {
        if (names_wasm) inc_wasm = 1
        paren += gsub(/\(/, "(", code) - gsub(/\)/, ")", code)
        if (paren <= 0) {
          if (inc_wasm && !inc_test) { print file ":" inc_line ": " inc_text; found = 1 }
          inc = 0
        }
      }

      before = depth
      depth += nopen - nclose
      if (test_depth >= 0 && depth <= test_depth) test_depth = -1
      if (test_item && rest ~ /;/) test_item = 0
      if (pending && rest ~ /[^[:space:]]/) {
        if (nopen > nclose) test_depth = before
        else if (nopen == nclose && nopen > 0) { }
        else if (rest !~ /;/) test_item = 1
        pending = 0
      }
    }
    END { exit (found ? 1 : 0) }
  ' "$1"
}

# ---------------------------------------------------------------------------
# The gate proves itself before it judges the tree. Each fixture names the
# verdict it must get; a scanner that cannot tell these seven apart is the
# false-green this check exists to prevent.
# ---------------------------------------------------------------------------
self_test() {
  local dir failures=0
  dir=$(mktemp -d)
  trap 'rm -rf "$dir"' RETURN

  # 1. a test-only constant, then production code (the broker shape)
  cat >"$dir/after-test-const.rs" <<'FIXTURE'
#[cfg(not(test))]
const IDLE: u64 = 120;
#[cfg(test)]
const IDLE: u64 = 200;

pub static GUEST: &[u8] = include_bytes!("../modules/chat/component.wasm");
FIXTURE

  # 2. a production include spread over several lines
  cat >"$dir/multiline.rs" <<'FIXTURE'
pub static GUEST: &[u8] = include_bytes!(
    "../modules/chat/component.wasm"
);
FIXTURE

  # 3. the ordinary production include — the case that always failed
  cat >"$dir/same-line.rs" <<'FIXTURE'
pub static GUEST: &[u8] = include_bytes!("component.wasm");
FIXTURE

  # 4. a real test module, with production code after it
  cat >"$dir/test-mod.rs" <<'FIXTURE'
pub fn run() {}

#[cfg(test)]
mod tests {
    const FIXTURE: &[u8] = include_bytes!("fixtures/hello.component.wasm");

    #[test]
    fn it_loads() {
        assert!(!FIXTURE.is_empty());
    }
}

pub fn also_production() {}
FIXTURE

  # 5. a test-only constant whose include spans lines
  cat >"$dir/test-const-multiline.rs" <<'FIXTURE'
#[cfg(test)]
const FIXTURE: &[u8] = include_bytes!(
    "fixtures/hello.component.wasm"
);

pub fn run() {}
FIXTURE

  # 6. production code that includes something that is not a guest
  cat >"$dir/not-a-guest.rs" <<'FIXTURE'
pub const HELP: &str = include_str!("help.txt");

#[cfg(test)]
mod tests {
    #[test]
    fn it_reads() {
        assert!(!super::HELP.is_empty());
    }
}
FIXTURE

  # 7. a test module whose string literal holds an unbalanced brace, then a
  #    production include. Counting that brace as structure holds the test
  #    region open over the include and greens it.
  cat >"$dir/brace-in-string.rs" <<'FIXTURE'
#[cfg(test)]
mod tests {
    const OPEN: &str = "{";

    #[test]
    fn it_reads() {
        assert_eq!(OPEN, "{");
    }
}

pub static GUEST: &[u8] = include_bytes!("../modules/chat/component.wasm");
FIXTURE

  local name want got
  while read -r name want; do
    got=refuses
    scan_file "$dir/$name" >/dev/null 2>&1 && got=accepts
    if [ "$got" != "$want" ]; then
      echo "wasm-embed-check self-test: $name should be $want, the scanner $got it" >&2
      failures=$((failures + 1))
    fi
  done <<'CASES'
after-test-const.rs refuses
multiline.rs refuses
same-line.rs refuses
test-mod.rs accepts
test-const-multiline.rs accepts
not-a-guest.rs accepts
brace-in-string.rs refuses
CASES

  [ "$failures" = 0 ] || { echo "wasm-embed-check: the scanner itself is broken, $failures of 7 fixtures misjudged" >&2; exit 1; }
}

self_test
if [ "${1:-}" = --self-test ]; then
  echo "wasm-embed-check: 7 of 7 fixtures judged correctly"
  exit 0
fi

bad=0
while read -r f; do
  scan_file "$f" || bad=1
done < <(git ls-files '*.rs' | grep -v -e '/tests/' -e '/tests\.rs$')

[ "$bad" = 0 ] || {
  echo "a non-test source embeds a .wasm — the binary is not the module set (AGENTS.md)"
  exit 1
}
echo "wasm-embed-check: no non-test include of a .wasm"
