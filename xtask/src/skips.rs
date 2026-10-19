//! The skips check. The lists of each OS in `ci.yaml` are the one record of what that
//! OS does not run (LINUX CI), so a test skips an OS by an attribute or a `cfg` only in
//! a case of `docs/claude/testing.md`, "No `#[ignore]`", and its reason says so.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use cfg_expr::targets::{TargetInfo, get_builtin_target_by_triple};
use cfg_expr::{Expression, Predicate};
use quote::ToTokens;
use syn::ext::IdentExt;
use syn::parse::ParseStream;
use syn::punctuated::Punctuated;
use syn::visit::Visit;
use syn::{Attribute, Expr, Ident, Item, Lit, Meta, Token};

use crate::{field, files, workflow};

/// A runner of CI that runs the tests of the workspace: its target, its OS as a reason
/// names it, and the name of its entry in the matrix of the job `check`.
struct Runner {
    triple: &'static str,
    os: &'static str,
    entry: &'static str,
}

/// The triple of the runner of `arm.yaml`, which has no lists.
const ARM: &str = "aarch64-unknown-linux-gnu";

/// The target of each runner of `Runners`, in the order of its bits.
fn targets() -> impl Iterator<Item = &'static TargetInfo> {
    (RUNNERS.iter().map(|runner| runner.triple))
        .chain([ARM])
        .map(|triple| {
            get_builtin_target_by_triple(triple)
                .expect("invariant: rustc knows the target of each runner")
        })
}

/// The runners of the lists of LINUX CI, one for each OS, in the order in which a
/// reason names their OSes.
const RUNNERS: [Runner; 3] = [
    Runner {
        triple: "x86_64-unknown-linux-gnu",
        os: "Linux",
        entry: "check",
    },
    Runner {
        triple: "aarch64-apple-darwin",
        os: "macOS",
        entry: "macos",
    },
    Runner {
        triple: "x86_64-pc-windows-msvc",
        os: "Windows",
        entry: "windows",
    },
];

/// A set of runners: one bit for each of `RUNNERS` by its index, and the next bit for
/// the runner of `arm.yaml`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Runners(u8);

impl Runners {
    const ALL: Self = Self((1 << (RUNNERS.len() + 1)) - 1);
    /// The runners of the job `check`.
    const CHECK: Self = Self((1 << RUNNERS.len()) - 1);
    const NONE: Self = Self(0);

    /// The runners on which `predicate`, the predicate of a `cfg`, holds in `cargo test
    /// --all-features`, and whether it names a target: `test`, `debug_assertions`, and
    /// each `feature` hold, and each other name that is not one of a target does not.
    fn holding(predicate: &Meta) -> Result<(Self, bool), String> {
        let expression =
            Expression::parse(&text(predicate)?).map_err(|e| e.to_string())?;
        let keyed = expression
            .predicates()
            .any(|predicate| matches!(predicate, Predicate::Target(_)));
        let mut runners = Self::NONE;
        for (i, target) in targets().enumerate() {
            let holds = expression.eval(|predicate| match predicate {
                Predicate::Target(name) => Some(name.matches(target)),
                Predicate::Test
                | Predicate::DebugAssertions
                | Predicate::Feature(_) => Some(true),
                Predicate::ProcMacro
                | Predicate::Flag(_)
                | Predicate::KeyValue { .. } => Some(false),
                Predicate::TargetFeature(_) => None,
            });
            match holds {
                Some(true) => runners.0 |= 1 << i,
                Some(false) => {}
                None => {
                    return Err(format!(
                        "a `cfg` predicate with no known value on `{}`",
                        target.triple.as_str()
                    ));
                }
            }
        }
        Ok((runners, keyed))
    }

    /// The runners whose target has the `target_arch` `arch`.
    fn arch(arch: &str) -> Self {
        let mut runners = Self::NONE;
        for (i, target) in targets().enumerate() {
            if target.arch.as_str() == arch {
                runners.0 |= 1 << i;
            }
        }
        runners
    }

    fn and(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    fn or(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// The OSes of the runners, in the order of `RUNNERS`. The runner of `ARM` is
    /// Linux.
    fn oses(self) -> Vec<&'static str> {
        let arm = self.0 >> RUNNERS.len() & 1 == 1;
        RUNNERS
            .iter()
            .enumerate()
            .filter(|(i, runner)| self.0 >> i & 1 == 1 || arm && runner.os == "Linux")
            .map(|(_, runner)| runner.os)
            .collect()
    }
}

/// Checks each test in the module tree of each target of the workspace at `root`
/// against the lists of LINUX CI in `.github/workflows/ci.yaml`, as LINUX CI states. It
/// refuses an `ignore` that skips an OS on its runner with no `only <OS>` reason, each
/// gate, a `cfg` that names a target and leaves a test out of an OS, that no `gates`
/// list item and no `Only <OS>` or `Only <arch>` doc gives, and each list item that
/// names no gate. A `cfg_attr` that applies `test` on some runners is a gate on the
/// others. In the body of a macro call, it reads only the functions, modules, and brace
/// groups in the tokens of the call, and the `cfg` of each branch of `cfg_if!`, and no
/// `ignore`.
///
/// It also refuses a file that does not parse, a `mod` with no file, a `path` in a
/// `cfg_attr`, a `cfg` predicate that is not one of Rust or whose value on a runner is
/// not known, an `ignore` reason that is not a string literal, and a `ci.yaml` that
/// `workflow` refuses, that has no `gates` list for an entry of the job `check`, or
/// whose `gates` value is not a block list or `[]`.
pub(crate) fn check(root: &Path) -> Result<(), Vec<String>> {
    let problems = problems(root).map_err(|e| vec![e])?;
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

fn problems(root: &Path) -> Result<Vec<String>, String> {
    let mut lists = lists(root)?;
    let mut modules = roots(root)?;
    let mut problems = BTreeSet::new();
    let mut catalog = Catalog::default();
    while let Some(module) = modules.pop() {
        let shown = module
            .file
            .strip_prefix(root)
            .unwrap_or(&module.file)
            .display()
            .to_string();
        let listed = shown.replace(std::path::MAIN_SEPARATOR, "/");
        let text = std::fs::read_to_string(&module.file)
            .map_err(|e| format!("{}: {e}", module.file.display()))?;
        let file = match syn::parse_file(&text) {
            Ok(file) => file,
            Err(e) => {
                problems.insert(format!("`{shown}`: {e}"));
                continue;
            }
        };
        let tests = if alone(module.entry, &file) {
            Tests::Each
        } else {
            module.tests
        };
        let mut items = Items {
            shown: &shown,
            listed: &listed,
            file: &module.file,
            above: &module.above,
            dir: module.dir,
            inline: 0,
            item: Vec::new(),
            on: module.on,
            target: module.target,
            module: module.path,
            gates: module.gates,
            tests,
            modules: &mut modules,
            catalog: &mut catalog,
            problems: &mut problems,
        };
        items.enter(&file.attrs, None, |items, _| items.visit_file(&file));
    }
    catalog.refuse(&mut lists, &mut problems);
    for (runner, list) in RUNNERS.iter().zip(lists) {
        for item in list {
            problems.insert(unlisted(&item, runner.os));
        }
    }
    Ok(problems.into_iter().collect())
}

/// The root module of each target of the workspace at `root`.
fn roots(root: &Path) -> Result<Vec<Module>, String> {
    let metadata = crate::metadata(root)?;
    let mut modules = Vec::new();
    for (package, value) in field::list(&metadata, "packages")?.iter().enumerate() {
        for (target, value) in field::list(value, "targets")?.iter().enumerate() {
            let file = files::normalize(Path::new(field::text(value, "src_path")?));
            let dir = file
                .parent()
                .ok_or_else(|| format!("{} has no directory", file.display()))?
                .to_path_buf();
            let tests = if field::flag(value, "test")? {
                Tests::Marked
            } else {
                Tests::None
            };
            modules.push(Module {
                file,
                above: Vec::new(),
                dir,
                on: Runners::ALL,
                target: (package, target),
                path: Vec::new(),
                gates: Vec::new(),
                tests,
                entry: entry(value)?,
            });
        }
    }
    Ok(modules)
}

/// Reports whether `cargo test` runs `target`, a target of cargo metadata, as a test or
/// bench target.
pub(crate) fn entry(target: &serde_json::Value) -> Result<bool, String> {
    let kinds = field::list(target, "kind")?;
    Ok(field::flag(target, "test")?
        && kinds.iter().any(|k| k == "test" || k == "bench"))
}

/// Reports whether a target that `cargo test` runs as a test or bench target when
/// `entry` holds, with the root file `file`, is a program with no test harness.
pub(crate) fn alone(entry: bool, file: &syn::File) -> bool {
    entry && file.items.iter().any(main)
}

/// Reports whether `item`, an item of the root file of a target, makes the target a
/// program with no test harness.
fn main(item: &Item) -> bool {
    match item {
        Item::Fn(function) => function.sig.ident == "main",
        Item::Macro(call) => call
            .mac
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "criterion_main"),
        _ => false,
    }
}

/// Which functions of a module tree are tests.
#[derive(Clone, Copy)]
enum Tests {
    /// None: `cargo test` does not run the target.
    None,
    /// Those with a `test` attribute.
    Marked,
    /// Each one: the target has no test harness.
    Each,
}

/// A target of the workspace: the index of its package, and its index in the package.
type Target = (usize, usize);

/// A file of a module, the directory of the modules that it declares, the runners on
/// which it builds, its target, its path in the target, the gates around it, and which
/// of its functions are tests.
struct Module {
    file: PathBuf,
    /// The files of the modules that hold the module.
    above: Vec<PathBuf>,
    dir: PathBuf,
    on: Runners,
    target: Target,
    path: Vec<String>,
    gates: Vec<usize>,
    tests: Tests,
    /// The file is the root of a test or bench target, so a `main` in it makes each
    /// function a test.
    entry: bool,
}

/// An item whose `cfg` names a target and leaves out runners on which its parent
/// builds.
struct Gate {
    /// The item as a `gates` list names it: its file, and its path in the file.
    named: String,
    /// The runners that its `cfg` leaves out.
    out: Runners,
    /// The runners on which it builds.
    on: Runners,
    /// The first line of its doc.
    doc: Option<String>,
}

/// The gates and the tests of the workspace.
#[derive(Default)]
struct Catalog {
    gates: Vec<Gate>,
    /// Each definition of each path of a test in its target.
    tests: BTreeMap<(Target, Vec<String>), Vec<Definition>>,
}

/// A definition of a test: the runners on which it is a test, and the gates around it.
struct Definition {
    on: Runners,
    gates: Vec<usize>,
}

impl Catalog {
    /// Refuses each gate that leaves a test out of an OS on which no definition of its
    /// path builds, with no item of the `gates` list of that OS and no doc that gives
    /// the case. It takes each item that it finds out of `lists`.
    fn refuse(&self, lists: &mut [BTreeSet<String>], problems: &mut BTreeSet<String>) {
        let mut out = BTreeSet::new();
        for items in self.tests.values() {
            let on = items.iter().fold(Runners::NONE, |on, item| on.or(item.on));
            for item in items {
                for &gate in &item.gates {
                    let left = self.gates[gate].out.without(on);
                    out.extend(
                        (0..RUNNERS.len())
                            .filter(|i| left.0 >> i & 1 == 1)
                            .map(|i| (gate, i)),
                    );
                }
            }
        }
        // Gates can share a name, so an item is taken out only after each of them.
        let mut used = BTreeSet::new();
        for (gate, i) in out {
            let gate = &self.gates[gate];
            if lists[i].contains(&gate.named) {
                used.insert((i, gate.named.as_str()));
            } else if !gate.doc.as_deref().is_some_and(|d| covers(d, gate.on)) {
                problems.insert(gated(&gate.named, RUNNERS[i].os));
            }
        }
        for (i, named) in used {
            lists[i].remove(named);
        }
    }
}

/// What the attributes of an item do: the runners on which it builds, those on which
/// each `cfg` that names a target builds it, those on which a `test` attribute applies,
/// if it has one, and each `ignore`, with the runners on which it applies and its
/// reason.
struct Effect {
    builds: Runners,
    keyed: Runners,
    tested: Option<Runners>,
    ignores: Vec<Ignore>,
}

struct Ignore {
    on: Runners,
    /// A `cfg_attr` around it names a target.
    keyed: bool,
    reason: Option<String>,
}

/// Walks the items of one file.
struct Items<'a> {
    shown: &'a str,
    /// The file as a `gates` item names it, with `/` on each OS.
    listed: &'a str,
    file: &'a Path,
    /// The files of the modules that hold the file.
    above: &'a [PathBuf],
    /// The directory of the modules that the item declares, with each inline module.
    dir: PathBuf,
    /// How many inline modules hold the item.
    inline: usize,
    /// The path of the item in the file.
    item: Vec<String>,
    /// The runners on which the item builds.
    on: Runners,
    target: Target,
    /// The path of the file in its target.
    module: Vec<String>,
    /// The gates around the item.
    gates: Vec<usize>,
    tests: Tests,
    modules: &'a mut Vec<Module>,
    catalog: &'a mut Catalog,
    problems: &'a mut BTreeSet<String>,
}

impl Items<'_> {
    /// Visits an item with `attributes`, named `name`, on the runners on which it
    /// builds. A `cfg` that names a target can leave a test out of each runner, so the
    /// items under such a gate are visited on none.
    fn enter(
        &mut self,
        attributes: &[Attribute],
        name: Option<String>,
        visit: impl FnOnce(&mut Self, &Effect),
    ) {
        let named = name.is_some();
        self.item.extend(name);
        match effect(attributes) {
            Ok(effect) => {
                let on = self.on.and(effect.builds);
                let gated = self.gate(self.on.without(effect.keyed), on, attributes);
                if on != Runners::NONE || gated || self.on == Runners::NONE {
                    let outer = std::mem::replace(&mut self.on, on);
                    visit(self, &effect);
                    self.on = outer;
                }
                if gated {
                    self.gates.pop();
                }
            }
            Err(e) => self.problem(&e),
        }
        if named {
            self.item.pop();
        }
    }

    /// Adds a gate around the item, with `attributes`, that leaves it out of `out` and
    /// builds it on `on`, unless `out` is empty. It reports whether it added one.
    fn gate(&mut self, out: Runners, on: Runners, attributes: &[Attribute]) -> bool {
        if out == Runners::NONE {
            return false;
        }
        let item = self.item.join("::");
        self.gates.push(self.catalog.gates.len());
        self.catalog.gates.push(Gate {
            named: [self.listed, &item].join(" ").trim_end().to_string(),
            out,
            on,
            doc: doc(attributes),
        });
        true
    }

    fn problem(&mut self, problem: &str) {
        let item = self.item.join("::");
        self.problems
            .insert(format!("`{}`: `{item}`: {problem}", self.shown));
    }

    /// Visits the function with `attributes`, named `name`: it records the function
    /// when it is a test, and refuses each of its `ignore` attributes that skips an OS
    /// outside a case. A test that its `ignore` attributes skip on each OS on which it
    /// builds has no OS for a reason to name, unless a plain `#[ignore]` skips it on
    /// every OS.
    fn function(
        &mut self,
        attributes: &[Attribute],
        name: &Ident,
        visit: impl FnOnce(&mut Self),
    ) {
        self.enter(attributes, Some(name.unraw().to_string()), |s, effect| {
            s.record(attributes, effect.tested);
            s.test(&effect.ignores);
            visit(s);
        });
    }

    /// Records the function with `attributes` as a test, when it is one, on the runners
    /// of `tested`, those on which a `test` attribute applies to it. A `cfg_attr` that
    /// applies it on only some runners is a gate on the others.
    fn record(&mut self, attributes: &[Attribute], tested: Option<Runners>) {
        let tested = match (self.tests, tested) {
            (Tests::None, _) | (Tests::Marked, None) => return,
            (Tests::Marked, Some(tested)) => tested,
            (Tests::Each, _) => Runners::ALL,
        };
        let on = self.on.and(tested);
        let gated = self.gate(self.on.without(tested), on, attributes);
        let path = [self.module.as_slice(), &self.item].concat();
        self.catalog
            .tests
            .entry((self.target, path))
            .or_default()
            .push(Definition {
                on,
                gates: self.gates.clone(),
            });
        if gated {
            self.gates.pop();
        }
    }

    /// Visits `items`, the body of a macro call with `outer`, the attributes of the
    /// call, or of a module in it, with none.
    fn body(&mut self, outer: &[Attribute], items: &[Body]) {
        for item in items {
            match item {
                Body::Function(attributes, name) => {
                    let attributes = [outer, attributes].concat();
                    let name = name.unraw().to_string();
                    self.enter(&attributes, Some(name), |s, effect| {
                        s.record(&attributes, effect.tested);
                    });
                }
                Body::Module(attributes, name, items) => {
                    let attributes = [outer, attributes].concat();
                    let name = name.unraw().to_string();
                    self.enter(&attributes, Some(name), |s, _| s.body(&[], items));
                }
                Body::Group(attributes, items) => {
                    self.body(&[outer, attributes].concat(), items);
                }
            }
        }
    }

    fn test(&mut self, ignores: &[Ignore]) {
        if ignores
            .iter()
            .any(|ignore| !ignore.keyed && ignore.on == Runners::ALL)
        {
            return;
        }
        let skipped = ignores
            .iter()
            .fold(Runners::NONE, |skipped, ignore| skipped.or(ignore.on));
        let runs = self.on.without(skipped);
        for ignore in ignores {
            let given =
                (ignore.reason.as_deref()).is_some_and(|r| gives(r, &runs.oses()));
            if ignore.on.and(self.on).and(Runners::CHECK) != Runners::NONE && !given {
                self.problems
                    .insert(refused(self.shown, &self.item.join("::")));
            }
        }
    }

    /// The directory from which a `#[path]` of the item names its file.
    fn base(&self) -> &Path {
        match self.inline {
            0 => self.file.parent().unwrap_or(&self.dir),
            _ => &self.dir,
        }
    }

    /// The file of the module `module` declares with no body, and the directory of the
    /// modules of that file.
    fn declared(&self, module: &syn::ItemMod) -> Result<(PathBuf, PathBuf), String> {
        let name = module.ident.unraw().to_string();
        if let Some(path) = path(&module.attrs)? {
            let file = files::normalize(&self.base().join(path));
            let dir = file.parent().unwrap_or(self.base()).to_path_buf();
            return Ok((file, dir));
        }
        let dir = self.dir.join(&name);
        let flat = self.dir.join(format!("{name}.rs"));
        if flat.is_file() {
            Ok((flat, dir))
        } else if dir.join("mod.rs").is_file() {
            Ok((dir.join("mod.rs"), dir))
        } else {
            Err(format!("no file for the module `{name}`"))
        }
    }
}

impl<'ast> Visit<'ast> for Items<'_> {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.enter(&i.attrs, Some(i.ident.unraw().to_string()), |s, _| {
            if i.content.is_some() {
                let dir = match path(&i.attrs) {
                    Ok(Some(path)) => files::normalize(&s.base().join(path)),
                    Ok(None) => s.dir.join(i.ident.unraw().to_string()),
                    Err(e) => return s.problem(&e),
                };
                let outer = std::mem::replace(&mut s.dir, dir);
                s.inline += 1;
                syn::visit::visit_item_mod(s, i);
                s.inline -= 1;
                s.dir = outer;
                return;
            }
            match s.declared(i) {
                Ok((file, _)) if file == s.file || s.above.contains(&file) => {
                    s.problem("circular modules");
                }
                Ok((file, dir)) => s.modules.push(Module {
                    file,
                    above: [s.above, &[s.file.to_path_buf()]].concat(),
                    dir,
                    on: s.on,
                    target: s.target,
                    path: [s.module.as_slice(), &s.item].concat(),
                    gates: s.gates.clone(),
                    tests: s.tests,
                    entry: false,
                }),
                Err(e) => s.problem(&e),
            }
        });
    }

    /// The items of an `impl` take its type, and its trait as `<type as trait>`, in
    /// their path, so that none shares the path of a free function or of an item of
    /// another `impl`.
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let name = match &i.trait_ {
            Some((path, _)) => {
                format!("<{} as {}>", spelled(&i.self_ty), spelled(path))
            }
            None => spelled(&i.self_ty),
        };
        self.enter(&i.attrs, Some(name), |s, _| {
            syn::visit::visit_item_impl(s, i);
        });
    }

    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        let name = i.ident.unraw().to_string();
        self.enter(&i.attrs, Some(name), |s, _| {
            syn::visit::visit_item_trait(s, i);
        });
    }

    /// A `cfg` of the call is a gate of each function and module in its body, in a
    /// brace group too, by its name. A `macro_rules!` holds no item until a call
    /// expands it.
    fn visit_item_macro(&mut self, i: &'ast syn::ItemMacro) {
        if i.mac.path.is_ident("macro_rules") {
            return;
        }
        match i.mac.parse_body_with(body) {
            Ok(items) => self.body(&i.attrs, &items),
            Err(e) => self.problem(&e.to_string()),
        }
    }

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.function(&i.attrs, &i.sig.ident, |s| syn::visit::visit_item_fn(s, i));
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.function(&i.attrs, &i.sig.ident, |s| {
            syn::visit::visit_impl_item_fn(s, i);
        });
    }

    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.function(&i.attrs, &i.sig.ident, |s| {
            syn::visit::visit_trait_item_fn(s, i);
        });
    }
}
/// The text of `tokens`, with a space only between two words, in a string literal,
/// and after a `,` or a `;` that no closing bracket follows.
fn spelled(tokens: &impl ToTokens) -> String {
    let spaced = tokens.to_token_stream().to_string();
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut text = String::with_capacity(spaced.len());
    let (mut quoted, mut escaped) = (false, false);
    let mut chars = spaced.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            (quoted, escaped) = (escaped || c != '"', !escaped && c == '\\');
        } else if c == '"' {
            quoted = true;
        } else if c == ' ' {
            let last = text.chars().last().unwrap_or(' ');
            let next = chars.peek().copied().unwrap_or(' ');
            let closing = matches!(next, ')' | ']' | '>');
            if !(word(last) && word(next) || matches!(last, ',' | ';') && !closing) {
                continue;
            }
        }
        text.push(c);
    }
    text
}

/// A function, a module, or a brace group in the body of a macro call: its attributes,
/// those inside a module too, its name, and its items.
enum Body {
    Function(Vec<Attribute>, Ident),
    Module(Vec<Attribute>, Ident, Vec<Body>),
    Group(Vec<Attribute>, Vec<Body>),
}

/// The functions and modules of the body of a macro call, as `proptest!` takes them,
/// and those in each brace group, with the attributes before it. A group after `if
/// #[cfg(<p>)]` or `else`, as `cfg_if!` takes them, builds only where no `<p>` before
/// it in its chain holds. Each other token is passed.
fn body(input: ParseStream<'_>) -> syn::Result<Vec<Body>> {
    let mut items = Vec::new();
    // The predicates of the chain that an `else` at the next token continues.
    let mut chain = Vec::new();
    while !input.is_empty() {
        let continued = input.parse::<Option<Token![else]>>()?.is_some();
        if input.peek(Token![if]) && input.peek2(Token![#]) {
            input.parse::<Token![if]>()?;
            let attribute = cfg(input)?;
            if !continued {
                chain.clear();
            }
            let content;
            syn::braced!(content in input);
            items.push(Body::Group(
                branch(input, &chain, Some(&attribute))?,
                body(&content)?,
            ));
            chain.push(attribute);
            continue;
        }
        if continued && input.peek(syn::token::Brace) {
            let content;
            syn::braced!(content in input);
            items.push(Body::Group(branch(input, &chain, None)?, body(&content)?));
            chain.clear();
            continue;
        }
        chain.clear();
        let attributes = if input.peek(Token![#]) && input.peek2(syn::token::Bracket) {
            input.call(Attribute::parse_outer)?
        } else {
            Vec::new()
        };
        input.parse::<syn::Visibility>()?;
        input.parse::<Option<Token![const]>>()?;
        input.parse::<Option<Token![async]>>()?;
        input.parse::<Option<Token![unsafe]>>()?;
        input.parse::<Option<syn::Abi>>()?;
        if rules(input)? {
            continue;
        }
        if input.peek(Token![fn]) && input.peek2(Ident::peek_any) {
            input.parse::<Token![fn]>()?;
            items.push(Body::Function(attributes, input.call(Ident::parse_any)?));
        } else if input.peek(Token![mod])
            && input.peek2(Ident::peek_any)
            && input.peek3(syn::token::Brace)
        {
            input.parse::<Token![mod]>()?;
            let name = input.call(Ident::parse_any)?;
            let content;
            syn::braced!(content in input);
            let attributes =
                [attributes, content.call(Attribute::parse_inner)?].concat();
            items.push(Body::Module(attributes, name, body(&content)?));
        } else {
            // The attributes of another item gate its first brace group: the body of
            // an `impl` or the input of a macro call. The item ends at it or at a `;`.
            while !attributes.is_empty() && !ends(input) {
                skip(input)?;
            }
            if input.peek(syn::token::Brace) {
                let content;
                syn::braced!(content in input);
                items.push(Body::Group(attributes, body(&content)?));
            } else if !input.is_empty() {
                // No function or module starts at the token after a prefix, so a skip
                // of it loses none.
                skip(input)?;
            }
        }
    }
    Ok(items)
}

/// Passes a `macro_rules!` at `input`, which holds no item until a call expands it,
/// and reports whether one was there.
fn rules(input: ParseStream<'_>) -> syn::Result<bool> {
    let rules = input.peek(Ident::peek_any)
        && input.peek2(Token![!])
        && input.fork().call(Ident::parse_any)? == "macro_rules";
    if rules {
        input.call(Ident::parse_any)?;
        input.parse::<Token![!]>()?;
        input.call(Ident::parse_any)?;
        skip(input)?;
    }
    Ok(rules)
}

/// Reports whether an item ends at `input`: at its end, a brace group, or a `;`.
fn ends(input: ParseStream<'_>) -> bool {
    input.is_empty() || input.peek(syn::token::Brace) || input.peek(Token![;])
}

/// Passes the token tree at `input`.
fn skip(input: ParseStream<'_>) -> syn::Result<()> {
    input.step(|cursor| {
        cursor
            .token_tree()
            .map(|(_, next)| ((), next))
            .ok_or_else(|| cursor.error("the end of the body"))
    })
}

/// The predicate of the attributes at the start of `input`, which must be one `cfg`.
fn cfg(input: ParseStream<'_>) -> syn::Result<Meta> {
    match input.call(Attribute::parse_outer)?.as_slice() {
        [attribute] if attribute.path().is_ident("cfg") => {
            attribute.meta.require_list()?.parse_args()
        }
        _ => Err(input.error("an `if` of a `cfg_if` with no one `cfg`")),
    }
}

/// The attributes of a branch at `input` of a `cfg_if` chain after `chain`: its own
/// predicate when it has one, and none of `chain`.
fn branch(
    input: ParseStream<'_>,
    chain: &[Meta],
    own: Option<&Meta>,
) -> syn::Result<Vec<Attribute>> {
    let text = |m: &Meta| text(m).map_err(|e| input.error(e));
    let mut predicates = Vec::new();
    if !chain.is_empty() {
        let before: Vec<_> = chain.iter().map(text).collect::<syn::Result<_>>()?;
        predicates.push(format!("not(any({}))", before.join(", ")));
    }
    predicates.extend(own.map(text).transpose()?);
    if predicates.is_empty() {
        return Ok(Vec::new());
    }
    syn::parse::Parser::parse_str(
        Attribute::parse_outer,
        &format!("#[cfg(all({}))]", predicates.join(", ")),
    )
}

/// The first line of the doc in `attributes`.
fn doc(attributes: &[Attribute]) -> Option<String> {
    attributes
        .iter()
        .find(|a| a.path().is_ident("doc"))
        .and_then(|a| match &a.meta {
            Meta::NameValue(pair) => string(&pair.value).ok(),
            _ => None,
        })
        .map(|doc| doc.lines().next().unwrap_or_default().trim().to_string())
}

/// The items of the `gates` list of each entry of the matrix of the job `check` in
/// `.github/workflows/ci.yaml`, in the order of `RUNNERS`, with no comment.
fn lists(root: &Path) -> Result<Vec<BTreeSet<String>>, String> {
    let path = root.join(".github/workflows/ci.yaml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let nodes: Vec<&str> = (workflow::lines(&text)?.into_iter())
        .filter_map(|line| line.code)
        .collect();
    let job = workflow::under(&workflow::under(&nodes, "jobs:")?, "  check:")?;
    let mut lists: Vec<Option<BTreeSet<String>>> = vec![None; RUNNERS.len()];
    let (mut entry, mut list) = (None, None);
    for line in job {
        let text = line.trim();
        if let Some(name) = text.strip_prefix("- name: ") {
            entry = RUNNERS.iter().position(|runner| runner.entry == name);
            list = None;
        } else if list.is_some_and(|key| workflow::indent(line) > key) {
            if let (Some(i), Some(item)) = (entry, text.strip_prefix("- ")) {
                lists[i].get_or_insert_default().insert(item.to_string());
            }
        } else if let (Some(i), Some(value)) = (entry, text.strip_prefix("gates:")) {
            lists[i].get_or_insert_default();
            list = match value {
                "" => Some(workflow::indent(line)),
                " []" => None,
                _ => {
                    return Err(format!(
                        "`ci.yaml`: a `gates` list that is no block: `{text}`"
                    ));
                }
            };
        } else {
            list = None;
        }
    }
    RUNNERS
        .iter()
        .zip(lists)
        .map(|(runner, list)| {
            list.ok_or_else(|| {
                format!(
                    "`ci.yaml` has no `gates` list in the entry `{}` of the job \
                     `check`",
                    runner.entry
                )
            })
        })
        .collect()
}

/// What `attributes` do. It reads into each `cfg_attr`.
fn effect(attributes: &[Attribute]) -> Result<Effect, String> {
    let mut effect = Effect {
        builds: Runners::ALL,
        keyed: Runners::ALL,
        tested: None,
        ignores: Vec::new(),
    };
    apply(
        attributes.iter().map(|a| &a.meta),
        Runners::ALL,
        false,
        &mut effect,
    )?;
    Ok(effect)
}

/// Adds to `effect` what each of `attributes` does on the runners of `on`, inside a
/// `cfg_attr` that names a target when `keyed`.
fn apply<'a>(
    attributes: impl IntoIterator<Item = &'a Meta>,
    on: Runners,
    keyed: bool,
    effect: &mut Effect,
) -> Result<(), String> {
    for meta in attributes {
        let path = meta.path();
        if path.is_ident("cfg_attr") {
            let (predicate, inner) = meta
                .require_list()
                .and_then(|list| list.parse_args_with(cfg_attr))
                .map_err(|e| e.to_string())?;
            if inner.iter().any(|meta| meta.path().is_ident("path")) {
                return Err("a `path` in a `cfg_attr`".to_string());
            }
            let (holds, named) = Runners::holding(&predicate)?;
            apply(&inner, on.and(holds), keyed || named, effect)?;
        } else if path.is_ident("cfg") {
            let predicate = meta
                .require_list()
                .and_then(syn::MetaList::parse_args::<Meta>)
                .map_err(|e| e.to_string())?;
            let (holds, named) = Runners::holding(&predicate)?;
            let fails = on.without(holds);
            effect.builds = effect.builds.without(fails);
            if keyed || named {
                effect.keyed = effect.keyed.without(fails);
            }
        } else if path.is_ident("ignore") {
            let reason = match meta {
                Meta::Path(_) => None,
                Meta::NameValue(pair) => Some(string(&pair.value)?),
                Meta::List(_) => {
                    return Err("`ignore` takes no list".to_string());
                }
            };
            effect.ignores.push(Ignore { on, keyed, reason });
        } else if path.segments.last().is_some_and(|s| s.ident == "test")
            && (keyed || on != Runners::NONE)
        {
            effect.tested = Some(effect.tested.unwrap_or(Runners::NONE).or(on));
        }
    }
    Ok(())
}

/// Parses the arguments of a `cfg_attr`: its predicate and its attributes.
fn cfg_attr(input: ParseStream<'_>) -> syn::Result<(Meta, Vec<Meta>)> {
    let predicate = input.parse()?;
    input.parse::<Token![,]>()?;
    let attributes = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
    Ok((predicate, attributes.into_iter().collect()))
}

/// The path of a `#[path]` among `attributes`.
fn path(attributes: &[Attribute]) -> Result<Option<String>, String> {
    attributes
        .iter()
        .find(|a| a.path().is_ident("path"))
        .map(|a| match &a.meta {
            Meta::NameValue(pair) => string(&pair.value),
            _ => Err("a `path` that is no string".to_string()),
        })
        .transpose()
}

/// The text of a `cfg` predicate, as `cfg-expr` reads it.
fn text(predicate: &Meta) -> Result<String, String> {
    let name = predicate
        .path()
        .get_ident()
        .ok_or("a `cfg` name that is no identifier")?
        .to_string();
    Ok(match predicate {
        Meta::Path(_) => name,
        Meta::NameValue(pair) => format!("{name} = \"{}\"", string(&pair.value)?),
        Meta::List(list) => {
            let nested = list
                .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                .map_err(|e| e.to_string())?;
            let nested: Vec<_> = nested.iter().map(text).collect::<Result<_, _>>()?;
            format!("{name}({})", nested.join(", "))
        }
    })
}

/// The value of a string literal.
fn string(expr: &Expr) -> Result<String, String> {
    match expr {
        Expr::Lit(syn::ExprLit {
            lit: Lit::Str(text),
            ..
        }) => Ok(text.value()),
        _ => Err("a value that is no string literal".to_string()),
    }
}

/// Reports whether `reason` is `only <OS>` or starts with `only <OS>:`, where `<OS>`
/// names `oses` and no more: their names joined by `and`, or Unix for Linux and macOS.
/// No reason gives an empty `oses`.
fn gives(reason: &str, oses: &[&str]) -> bool {
    if oses.is_empty() {
        return false;
    }
    let joined = oses.join(" and ");
    let unix = (oses == ["Linux", "macOS"]).then_some("Unix");
    [Some(joined.as_str()), unix]
        .into_iter()
        .flatten()
        .any(|name| {
            reason
                .strip_prefix("only ")
                .and_then(|r| r.strip_prefix(name))
                .is_some_and(|r| r.is_empty() || r.starts_with(':'))
        })
}

/// Reports whether `doc`, the first line of the doc of a gate that builds its item on
/// `on`, is `Only <OS>` or `Only <arch>`, or starts with either and `:`, where `<OS>`
/// names the OSes of `on` as a reason does, and `<arch>` is the `target_arch` of each
/// runner of `on`, that of `arm.yaml` too, and of no other.
fn covers(doc: &str, on: Runners) -> bool {
    let Some(rest) = doc.strip_prefix("Only ") else {
        return false;
    };
    let arch = rest.split_once(':').map_or(rest, |(arch, _)| arch);
    gives(&format!("only {rest}"), &on.oses())
        || (on != Runners::NONE && Runners::arch(arch) == on)
}

fn refused(shown: &str, item: &str) -> String {
    format!(
        "`{shown}`: `{item}` ignores a test on an OS outside the lists of LINUX CI. \
         Fix: remove the `ignore` and add `- <path> # #<issue>` to the `tests` \
         list of each OS in `ci.yaml` where the test fails, or, in a case of \
         `docs/claude/testing.md`, \"No `#[ignore]`\", make its reason `only <OS>` \
         or start it with `only <OS>:`, where <OS> names each OS that runs the test: \
         Linux, macOS, or Windows, two of them joined by `and` in that order, or Unix \
         for Linux and macOS."
    )
}

fn gated(named: &str, os: &str) -> String {
    format!(
        "`{named}`: a `cfg` leaves a test out of {os} outside the lists of LINUX CI. \
         Fix: add `- {named} # #<issue>` to the `gates` list of {os} in `ci.yaml`, \
         or, in a case of `docs/claude/testing.md`, \"No `#[ignore]`\", make the \
         first line of the doc of the item `Only <OS>` or `Only <arch>`, or start it \
         with either and `:`, where <OS> names each OS on which the item builds, as \
         the reason of an `ignore` does, and <arch> is the `target_arch` of each \
         runner on which it builds and of no other."
    )
}

fn unlisted(item: &str, os: &str) -> String {
    format!(
        "`ci.yaml`: the item `{item}` of the `gates` list of {os} names no `cfg` that \
         leaves a test out of {os}. Fix: remove it."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{NO_FOLDER, native};

    #[test]
    fn refuses_each_ignore_on_an_os_outside_its_case_and_names_what_it_cannot_read() {
        let at = |file: &str| native(&format!("crates/a/src/{file}"));
        let lib = at("lib.rs");
        let mut expected = vec![
            refused(&native("bench/b/src/lib.rs"), "b"),
            format!(
                "`{}`: `a`: not(unix, windows)\n^^^^^^^^^^^^^^^^^^ not() takes 1 \
                 predicate, found 2",
                at("arity.rs")
            ),
            format!("`{}`: `again`: circular modules", at("circle.rs")),
            refused(&at("everywhere.rs"), "a"),
            refused(&at("everywhere.rs"), "b"),
            refused(&at("everywhere.rs"), "c"),
            refused(&at("inline_dir/inner.rs"), "a"),
            refused(&lib, "a"),
            format!("`{lib}`: `chosen`: a `path` in a `cfg_attr`"),
            format!("`{lib}`: `missing`: no file for the module `missing`"),
            format!("`{}`: `b::a`: `ignore` takes no list", at("list.rs")),
            format!(
                "`{}`: `a`: a value that is no string literal",
                at("literal.rs")
            ),
            format!(
                "`{}`: cannot parse string into token stream",
                at("parse.rs")
            ),
            refused(&at("pathed_file.rs"), "a"),
        ];
        let reasons_rs = at("reasons.rs");
        expected.extend(["comma", "or", "slash"].map(|i| refused(&reasons_rs, i)));
        let items = [
            "a", "aa", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m",
            "n::X::o", "n::Y::p", "q", "r", "s", "t", "u", "v", "w", "x", "y", "z",
        ];
        let refused_rs = at("refused.rs");
        expected.extend(items.map(|i| refused(&refused_rs, i)));
        expected.extend([
            format!(
                "`{}`: `a`: a `cfg` predicate with no known value on \
                 `x86_64-pc-windows-msvc`",
                at("target.rs")
            ),
            refused(&at("type.rs"), "a"),
            refused(&at("unix.rs"), "b"),
            format!(
                "`{}`: `a`: nany(unix)\n    ^ expected one of `=`, `,`, `)` here",
                at("unknown.rs")
            ),
            refused(&at("windows.rs"), "a"),
            refused(&native("oracles/a/common.rs"), "ready_on_each_os"),
            refused("outside.rs", "on_each_os"),
        ]);
        assert_eq!(check(&crate::fixture().join("skips")), Err(expected));
    }

    #[test]
    fn refuses_each_gate_that_no_list_or_doc_gives_and_each_unused_list_item() {
        let lib = "crates/g/src/lib.rs";
        let shown = native(lib);
        let free = "crates/g/tests/free.rs";
        let mut expected = vec![
            gated(&format!("{lib} refused"), "Windows"),
            gated(&format!("{lib} misnamed"), "Windows"),
            gated(&format!("{lib} generated"), "Linux"),
            gated(&format!("{lib} generated"), "Windows"),
            gated(&format!("{lib} public"), "Windows"),
            gated(&format!("{lib} waited"), "Windows"),
            gated(&format!("{lib} inner"), "Windows"),
            gated(&format!("{lib} nowhere"), "Linux"),
            gated(&format!("{lib} nowhere"), "macOS"),
            gated(&format!("{lib} nowhere"), "Windows"),
            gated(&format!("{lib} misarmed"), "Linux"),
            gated(&format!("{lib} misarmed"), "Windows"),
            gated(&format!("{lib} apple"), "Linux"),
            gated(&format!("{lib} apple"), "Windows"),
            gated(&format!("{lib} intel"), "macOS"),
            gated(&format!("{lib} falsemac"), "Linux"),
            gated(&format!("{lib} falsemac"), "Windows"),
            gated(&format!("{lib} first"), "Windows"),
            gated(&format!("{lib} called"), "Windows"),
            gated(&format!("{free} implied"), "Windows"),
            gated(&format!("{lib} colon"), "Linux"),
            gated(&format!("{lib} colon"), "macOS"),
            gated(&format!("{lib} colon"), "Windows"),
            gated(&format!("{lib} hidden"), "Windows"),
            gated(&format!("{lib} shadowed"), "Linux"),
            gated(&format!("{lib} shadowed"), "macOS"),
            gated(&format!("{lib} shadowed"), "Windows"),
            gated(&format!("{lib} rest"), "Linux"),
            gated(&format!("{lib} rest"), "macOS"),
            gated(&format!("{lib} grouped"), "Windows"),
            gated(&format!("{lib} tail"), "Windows"),
            gated(&format!("{lib} inside"), "Windows"),
            gated(&format!("{lib} unbuilt"), "Linux"),
            gated(&format!("{lib} unbuilt"), "macOS"),
            gated(&format!("{lib} unbuilt"), "Windows"),
            format!("`{shown}`: ``: unexpected token, expected `]`"),
            format!("`{shown}`: ``: an `if` of a `cfg_if` with no one `cfg`"),
            gated("crates/g/src/module.rs inner", "Linux"),
            gated("crates/g/src/module.rs inner", "macOS"),
            gated("crates/g/src/twins.rs attributed", "Windows"),
            gated(&format!("{free} case"), "Windows"),
            gated(&format!("{free} Rig::gated"), "Windows"),
            gated(&format!("{free} Case::case"), "Windows"),
            gated(&format!("{free} <[u8] as Case>"), "Windows"),
            gated(&format!("{free} <Rig as std::fmt::Debug>"), "Windows"),
            gated(&format!("{free} <&mut Rig as Case>"), "Windows"),
            gated(&format!("{free} <[u8; 2] as Case>"), "Windows"),
            gated(&format!("{free} <(u8, Rig) as Case>"), "Windows"),
            gated(&format!("{free} <<Rig as Case>::Out as Case>"), "Windows"),
            gated(&format!("{free} <Cell<'static, Rig> as Case>"), "Windows"),
            gated(&format!("{free} <fn() as Other>"), "Windows"),
            gated(&format!("{free} konst"), "Windows"),
            gated(&format!("{free} risky"), "Windows"),
            gated(&format!("{free} foreign"), "Windows"),
            gated("crates/g/tests/filed.rs", "Windows"),
            gated("crates/g/benches/timed.rs case", "Windows"),
            "`ci.yaml`: the item `crates/g/src/lib.rs stale` of the `gates` list of \
             Windows names no `cfg` that leaves a test out of Windows. Fix: remove it."
                .into(),
        ];
        expected.sort();
        assert_eq!(check(&crate::fixture().join("gates")), Err(expected));
    }

    #[test]
    fn spells_a_type_with_a_space_only_where_its_tokens_need_one() {
        for (ty, expected) in [
            ("&'a mut Rig", "&'a mut Rig"),
            ("[u8; N]", "[u8; N]"),
            ("(u8,)", "(u8,)"),
            ("Cell<'static, Rig,>", "Cell<'static, Rig,>"),
            ("fn(u8) -> u8", "fn(u8)->u8"),
            ("<Rig as Case>::Out", "<Rig as Case>::Out"),
            ("dyn A + Send", "dyn A+Send"),
            (r#"Arr<"a  b\" c">"#, r#"Arr<"a  b\" c">"#),
            (r#"Arr<"a", B>"#, r#"Arr<"a", B>"#),
        ] {
            let parsed: syn::Type = syn::parse_str(ty).unwrap();
            assert_eq!(spelled(&parsed), expected, "{ty}");
        }
    }

    #[test]
    fn refuses_a_ci_yaml_with_no_gates_list_for_an_entry_or_no_job_check() {
        let gates = crate::fixture().join("gates");
        let refused = |dir: &str, problem: String| {
            assert_eq!(check(&gates.join(dir)), Err(vec![problem]));
        };
        refused(
            "missing",
            "`ci.yaml` has no `gates` list in the entry `windows` of the job `check`"
                .into(),
        );
        refused("nojob", "ci.yaml has no `check:`".into());
        refused(
            "flow",
            "`ci.yaml`: a `gates` list that is no block: `gates: [a]`".into(),
        );
        let absent = gates.join("absent").join(".github/workflows/ci.yaml");
        refused("absent", format!("{}: {NO_FOLDER}", absent.display()));
    }

    #[test]
    fn names_the_oses_that_run_a_test() {
        assert!(gives(
            "only Linux and Windows: a case",
            &["Linux", "Windows"]
        ));
        assert!(gives("only Unix", &["Linux", "macOS"]));
        assert!(gives("only Linux and macOS: a case", &["Linux", "macOS"]));
        assert!(!gives("only Linux and Windows: a case", &["Linux"]));
        assert!(!gives("only Linux: a case", &["Linux", "Windows"]));
        assert!(!gives("only Linuxes", &["Linux"]));
        assert!(!gives("only : a case", &[]));
    }
}
