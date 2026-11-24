//! Data types describing discovered mutation sites.

use serde::Serialize;

use crate::plan::CmpOp;

/// Name of the module injected into the crate root.
pub const RUNTIME_MOD: &str = "__tmt_rt";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LiteralSide {
    None,
    Rhs(i128),
    Lhs(i128),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SiteKind {
    Cmp {
        op: CmpOp,
        lhs: String,
        rhs: String,
        literal: LiteralSide,
        /// Byte range of the operator and the whitespace around it.
        gap: (usize, usize),
    },
    /// `lhs && rhs` or `lhs || rhs`.
    Logic {
        and: bool,
        lhs: String,
        rhs: String,
        gap: (usize, usize),
    },
    /// `lhs + rhs` and friends. Only primitive numbers are mutated; for other operand
    /// types the probe performs the original operation and reports nothing.
    Arith {
        op: ArithOp,
        /// `lhs op= rhs` rather than `lhs op rhs`.
        assign: bool,
        lhs: String,
        rhs: String,
        gap: (usize, usize),
    },
    /// `!operand`. Only `bool` is mutated (the `!` is dropped).
    Not {
        /// Byte range of the `!` and whatever separates it from the operand.
        bang: (usize, usize),
        operand: String,
        /// Deleting `!` would glue the preceding and following tokens (`return!x`).
        needs_space: bool,
    },
    /// The result of a function whose exits reach the probe. The site's range is the
    /// inside of the body's braces and its text is the function signature.
    Return {
        name: String,
        /// `None` for a function returning `bool`; for one returning `Option<T>`, the
        /// return type as written.
        option: Option<String>,
        /// Byte range of the body when it is a single expression: the function's result
        /// is then exactly that expression's value.
        sole_expr: Option<(usize, usize)>,
    },
    /// `receiver.max(argument)` or `.min(..)`: the other of the two.
    MinMax {
        max: bool,
        receiver: String,
        argument: String,
        /// Byte ranges of `.max(` and of the closing `)`.
        call: (usize, usize),
        close: (usize, usize),
    },
    /// `receiver.first()` or `.last()` of a slice, a `Vec`, an array or a `BTreeSet`: the
    /// other end. The two are the same element while there is at most one.
    SliceEnd {
        first: bool,
        receiver: String,
        /// Byte range of `.first()`.
        call: (usize, usize),
        /// Byte range of the method's name.
        method: (usize, usize),
    },
    /// `lhs..rhs` or `lhs..=rhs` on integers: the end moves by one.
    Range {
        inclusive: bool,
        lhs: String,
        rhs: String,
        gap: (usize, usize),
    },
    /// An arm of a `match` that ends in a catch-all arm: without this arm the value falls
    /// through to the arms below.
    Arm {
        pattern: String,
        /// Where to put the probe: the start of the `=>`, or the range of an existing guard.
        guard: ArmGuard,
        removal: (usize, usize),
    },
    /// A call or assignment statement that can be deleted.
    Stmt,
    /// `-operand`: the minus is dropped.
    Neg {
        /// Byte range of the `-` and whatever separates it from the operand.
        minus: (usize, usize),
        operand: String,
        /// The operand is a literal: its type is whatever the context asks for.
        literal: bool,
        /// Deleting `-` would glue the preceding and following tokens (`return-1`).
        needs_space: bool,
    },
    /// The guard of a match arm: replaced by `true` or `false`.
    Guard {
        /// Whether `false` is a mutant of its own. It is not where the arm can be removed,
        /// which is the same thing.
        can_be_false: bool,
    },
    /// A field of a struct literal with a base (`..base`): the field is left to the base.
    Field {
        name: String,
        /// Written without its name (`S { name, ..base }`).
        shorthand: bool,
        /// The expression used by a probe, where the base can safely be read again.
        from_base: Option<String>,
        /// The field and its trailing comma, including any attributes.
        removal: (usize, usize),
    },
    /// The whole body of a function, replaced by a value of its return type. The site's
    /// range is the inside of the body's braces and its text is the function signature.
    Body {
        name: String,
        value: String,
        /// The body when it is one expression that only computes its value (no call,
        /// no macro, nothing assigned, no early exit): replacing the body by the value
        /// that expression always had is then the same program for the tests.
        sole_expr: Option<(usize, usize)>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArmGuard {
    /// No guard: one is added before the `=>` at this offset.
    None(usize),
    /// `if <expr>` with the expression at this byte range.
    Existing(usize, usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

impl ArithOp {
    /// How tightly the operator binds: a higher number before a lower one.
    pub fn binding(self) -> u8 {
        match self {
            ArithOp::Mul | ArithOp::Div | ArithOp::Rem => 5,
            ArithOp::Add | ArithOp::Sub => 4,
            ArithOp::Shl | ArithOp::Shr => 3,
            ArithOp::BitAnd => 2,
            ArithOp::BitXor => 1,
            ArithOp::BitOr => 0,
        }
    }

    pub const ALL: [ArithOp; 10] = [
        ArithOp::Add,
        ArithOp::Sub,
        ArithOp::Mul,
        ArithOp::Div,
        ArithOp::Rem,
        ArithOp::BitAnd,
        ArithOp::BitOr,
        ArithOp::BitXor,
        ArithOp::Shl,
        ArithOp::Shr,
    ];

    pub fn symbol(self) -> &'static str {
        match self {
            ArithOp::Add => "+",
            ArithOp::Sub => "-",
            ArithOp::Mul => "*",
            ArithOp::Div => "/",
            ArithOp::Rem => "%",
            ArithOp::BitAnd => "&",
            ArithOp::BitOr => "|",
            ArithOp::BitXor => "^",
            ArithOp::Shl => "<<",
            ArithOp::Shr => ">>",
        }
    }

    /// The operator the mutant uses instead.
    pub fn swapped(self) -> ArithOp {
        match self {
            ArithOp::Add => ArithOp::Sub,
            ArithOp::Sub => ArithOp::Add,
            ArithOp::Mul => ArithOp::Div,
            ArithOp::Div => ArithOp::Mul,
            ArithOp::Rem => ArithOp::Div,
            ArithOp::BitAnd => ArithOp::BitOr,
            ArithOp::BitOr => ArithOp::BitAnd,
            ArithOp::BitXor => ArithOp::BitAnd,
            ArithOp::Shl => ArithOp::Shr,
            ArithOp::Shr => ArithOp::Shl,
        }
    }

    /// The second operator a mutant uses instead. The shifts have none.
    pub fn swapped2(self) -> Option<ArithOp> {
        Some(match self {
            ArithOp::Add => ArithOp::Mul,
            ArithOp::Sub => ArithOp::Div,
            ArithOp::Mul => ArithOp::Add,
            ArithOp::Div => ArithOp::Rem,
            ArithOp::Rem => ArithOp::Add,
            ArithOp::BitAnd | ArithOp::BitOr => ArithOp::BitXor,
            ArithOp::BitXor => ArithOp::BitOr,
            ArithOp::Shl | ArithOp::Shr => return None,
        })
    }

    /// The second replacement where the operator is written as an assignment or not.
    /// Bits collected with `|=` from zero are the same bits with `^=`, and `&=` with
    /// `^=` is rarely a slip of the pen: as in cargo-mutants, those two are left out.
    pub fn second(self, assign: bool) -> Option<ArithOp> {
        if assign && matches!(self, ArithOp::BitAnd | ArithOp::BitOr) {
            return None;
        }
        self.swapped2()
    }

    /// Bit operators exist for integers only, the others for floats too.
    pub fn is_bitwise(self) -> bool {
        matches!(
            self,
            ArithOp::BitAnd | ArithOp::BitOr | ArithOp::BitXor | ArithOp::Shl | ArithOp::Shr
        )
    }

    /// The name the runtime uses for this operator in its probe methods and traits.
    fn stem(self) -> (&'static str, &'static str) {
        match self {
            ArithOp::Add => ("add", "Add"),
            ArithOp::Sub => ("sub", "Sub"),
            ArithOp::Mul => ("mul", "Mul"),
            ArithOp::Div => ("div", "Div"),
            ArithOp::Rem => ("rem", "Rem"),
            ArithOp::BitAnd => ("bitand", "BitAnd"),
            ArithOp::BitOr => ("bitor", "BitOr"),
            ArithOp::BitXor => ("bitxor", "BitXor"),
            ArithOp::Shl => ("shl", "Shl"),
            ArithOp::Shr => ("shr", "Shr"),
        }
    }

    /// Name of the probe method in the runtime.
    pub(crate) fn method(self, assign: bool) -> String {
        format!(
            "tmt_{}{}",
            self.stem().0,
            if assign { "_assign" } else { "" }
        )
    }

    /// Trait with the probe for every type that is not a primitive number.
    pub(crate) fn fallback_trait(self, assign: bool) -> String {
        format!("Any{}{}", self.stem().1, if assign { "Assign" } else { "" })
    }

    /// Trait with the probe for primitive numbers.
    pub(super) fn prim_trait(self, assign: bool) -> &'static str {
        match (self.is_bitwise(), assign) {
            (false, false) => "ArithPrim",
            (true, false) => "BitPrim",
            (false, true) => "ArithAssignPrim",
            (true, true) => "BitAssignPrim",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Site {
    pub id: u32,
    pub kind: SiteKind,
    /// 1-based line and column of the start of the site.
    pub line: usize,
    pub column: usize,
    /// Byte range in the original source.
    pub start: usize,
    pub end: usize,
    pub text: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TestModule {
    pub name: String,
    /// The `#[path = "..."]` of the declaration, relative to the declaring file's directory.
    pub path: Option<String>,
}

#[derive(Debug, Default)]
pub struct Discovery {
    pub sites: Vec<Site>,
    /// Sites that cannot be switched while the program runs: inside a `const fn`, the
    /// body of a function that returns `impl Iterator`, a compound assignment to a place
    /// that is more than a variable. Their mutants are built one by one
    /// (`source_mutants`). They have no number.
    pub rebuild_sites: Vec<Site>,
    /// Functions that return nothing and consist of one statement that is a site: an
    /// empty body is the mutant that deletes the statement, so it is not made twice.
    pub single_statement_bodies: Vec<String>,
    /// Out-of-line modules that only exist under `cfg(test)` (`#[cfg(test)] mod tests;`).
    pub test_modules: Vec<TestModule>,
}

/// The number of a site that has none.
pub const NO_ID: u32 = u32::MAX;
