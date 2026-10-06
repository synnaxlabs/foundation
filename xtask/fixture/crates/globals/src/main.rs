//! A binary whose comments and literals name `static` and disallowed_macros, which the
//! check allows. It refuses the last `static`, which shows that each literal ends
//! where it should.

// clippy::disallowed_macros, static mut
/* static /* nested static */ static */
const QUOTE: char = '"';
const TEXT: &'static str = "static mut \" static";
const RAW: &str = r#"static " static"#;
const BYTES: &[u8] = br"static";
const ESCAPED: char = '\'';
const fn is_static<'a>(text: &'a str) -> &'a str {
    text
}

static AFTER: u8 = 0;

fn main() {}
