# HCL conformance

HCL texts, each with the verdict of HCL v2.25.0 (`github.com/hashicorp/hcl/v2`), and a
test that `config_hcl::read` and `config_hcl::write` agree with them.
`crates/config-hcl` runs the test as its `conformance` test. The `[[test]]` entry in
`crates/config-hcl/Cargo.toml` is part of this oracle: to remove it is to weaken the
oracle.

| File | Holds |
| --- | --- |
| `texts/<name>.hcl` | One HCL text, exact bytes. `.gitattributes` keeps Git from changing a line end. |
| `main.go`, `go.mod`, `go.sum` | The program that writes `verdicts.txt`. `go.mod` pins the HCL version. |
| `verdicts.txt` | Made by the program. A line for each text: `<name> refused`, `<name> accepted`, or `<name> accepted <code>...`. The first line names the HCL version. |
| `differences.txt` | A line for each text where `read` differs from HCL on purpose: `<name> <outcome> <decision>`. The outcome is `ok` or one diagnostic code. |
| `verdicts.rs` | The test. |

## What the test checks

- Each text has one verdict, and each name in `differences.txt` has a text.
- `read` gives a Document for each accepted text with no code, and an error for each
  refused text. For an accepted text with codes, `read` gives errors, and each error
  has one of those codes.
- For a text in `differences.txt`, `read` gives the outcome there, and that outcome is
  not what the verdict asks. So a difference that stops must be removed.
- For each text that reads, `write` gives the bytes of a text that is accepted with no
  code and is not in `differences.txt`.

## The codes

The program walks the syntax tree of each accepted text and lists the code `read`
gives for each form outside data. A node that is not in this table stops the program
with its type and position. Add it to the table.

| HCL syntax | Code |
| --- | --- |
| `null` | `hcl.null` |
| `${` or `%{` in a string or a heredoc | `hcl.template` |
| An operator, except `-` before a number | `hcl.operator` |
| `a ? b : c` | `hcl.conditional` |
| `[for ...]` or `{for ...}` | `hcl.for` |
| `a[0]`, `f().b` | `hcl.index` |
| `a[*].b`, `a.*.b` | `hcl.splat` |
| `(a)` | `hcl.parentheses` |
| `p::f()` | `hcl.namespace` |
| `f(a...)` | `hcl.expansion` |
| An object key that is a number with a fraction, an exponent, or more than 154 digits | `hcl.number-key` |
An object key that is an expression, such as `{ f() = 1 }`, is not in the table:
`read` has no form for it yet (#506). HCL refuses some texts only when it evaluates
them, such as `{ a.b = 1 }`. The program only parses, so their verdict is "accepted".

## Add a text

1. Put the exact bytes in `texts/<name>.hcl`. A name is lower-case words joined by
   `-`.
2. In this directory, run `go run .` with Go 1.25 or later.
3. Run `cargo test -p config-hcl --test conformance`.
4. When `read` differs from HCL on purpose, add a line to `differences.txt` with the
   decision in `docs/decisions.md`. Otherwise fix `read`, or file an issue and leave
   the text out until the fix.

When the test says that `write` gives new bytes, add them as a new text. Never change
a text: add one.
