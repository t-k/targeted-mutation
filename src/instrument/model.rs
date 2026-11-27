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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BitValueType {
    Unsigned(u32),
    Signed(u32),
    Bool,
    Char,
}

impl BitValueType {
    pub(crate) fn width(self) -> u32 {
        match self {
            BitValueType::Unsigned(w) | BitValueType::Signed(w) => w,
            BitValueType::Bool => 1,
            BitValueType::Char => 21,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LocalBitType {
    Value(BitValueType),
    SliceOf(BitValueType),
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
        /// Static primitive types of local bindings referenced by `|` / `^` operands.
        local_types: Vec<(String, LocalBitType)>,
        /// Primitive type names shadowed by type aliases/items in this file.
        shadowed_prims: Vec<String>,
        /// Runtime cast-source type observation probes `(slot, start, end)` inside `|` / `^`.
        cast_probes: Vec<(u32, usize, usize)>,
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
    /// `receiver.starts_with(pattern)` or `.ends_with(pattern)`: relaxed to `.contains(pattern)`.
    StrAffix {
        starts: bool,
        receiver: String,
        pattern: String,
        /// Byte range of `.starts_with(` (from receiver end to pattern start).
        call: (usize, usize),
        /// Byte range of the closing `)` (from pattern end to closing paren end).
        close: (usize, usize),
        /// Byte range of the method name.
        method: (usize, usize),
    },
    /// `receiver.replace(from, to)` <-> `receiver.replacen(from, to, 1)`.
    StrReplace {
        /// `true` for `.replace(from, to)`, `false` for `.replacen(from, to, 1)`.
        replace_all: bool,
        receiver: String,
        from: String,
        to: String,
        /// Byte range from receiver end to `from` start.
        call: (usize, usize),
        /// Byte range from `from` end to `to` start.
        comma: (usize, usize),
        /// Byte range from `to` end to closing `)` end (includes `, 1)` for `replacen`).
        close: (usize, usize),
        /// Byte range of the method name.
        method: (usize, usize),
    },
    /// `receiver.clamp(low, high)`: drop lower bound (`.min(high)`) or upper bound (`.max(low)`).
    Clamp {
        receiver: String,
        low: String,
        high: String,
        /// Byte range from receiver end to `low` start.
        call: (usize, usize),
        /// Byte range from `low` end to `high` start.
        comma: (usize, usize),
        /// Byte range from `high` end to closing `)` end.
        close: (usize, usize),
    },
    /// `receiver.floor()` / `.ceil()` / `.round()` / `.trunc()` on floats.
    Rounding {
        op: RoundingOp,
        receiver: String,
        /// Byte range of `.floor()`.
        call: (usize, usize),
        /// Byte range of the method name.
        method: (usize, usize),
    },
    /// `receiver.all(pred)` <-> `receiver.any(pred)` on an iterator with a closure predicate.
    Quantifier {
        all: bool,
        receiver: String,
        predicate: String,
        /// Byte range from receiver end to predicate start.
        call: (usize, usize),
        /// Byte range from predicate end to closing `)` end.
        close: (usize, usize),
        /// Byte range of the method name.
        method: (usize, usize),
    },
    /// `receiver.position(pred)` <-> `receiver.rposition(pred)` on a double-ended exact-size
    /// iterator with a closure predicate.
    Position {
        /// `true` for `.position(pred)`, `false` for `.rposition(pred)`.
        forward: bool,
        receiver: String,
        predicate: String,
        /// Byte range from receiver end to predicate start.
        call: (usize, usize),
        /// Byte range from predicate end to closing `)` end.
        close: (usize, usize),
        /// Byte range of the method name.
        method: (usize, usize),
    },
    /// `receiver.saturating_{add,sub,mul}(arg)` <-> `receiver.wrapping_{add,sub,mul}(arg)`.
    SaturatingArith {
        op: SaturatingOp,
        receiver: String,
        argument: String,
        /// Byte range from receiver end to argument start.
        call: (usize, usize),
        /// Byte range from argument end to closing `)` end.
        close: (usize, usize),
        /// Byte range of the method name.
        method: (usize, usize),
    },
    /// `receiver.trim()` / `.trim_start()` / `.trim_end()` on strings.
    StrTrim {
        op: TrimOp,
        receiver: String,
        /// Byte range of `.trim()`.
        call: (usize, usize),
        /// Byte range of the method name.
        method: (usize, usize),
    },
    /// `receiver.eq_ignore_ascii_case(arg)` -> strict equality (`==`).
    StrCaseEq {
        receiver: String,
        argument: String,
        /// Byte range from receiver end to argument start.
        call: (usize, usize),
        /// Byte range from argument end to closing `)` end.
        close: (usize, usize),
        /// Byte range of the method name.
        method: (usize, usize),
    },
    /// `receiver.abs()` -> `receiver` on signed integers and floats.
    Abs {
        receiver: String,
        /// Byte range of `.abs()`.
        call: (usize, usize),
        /// Byte range of the method name.
        method: (usize, usize),
    },
    /// `iter.take(n)` -> `iter.take(usize::MAX)` or `iter.skip(n)` -> `iter.skip(0)`.
    IterLimit {
        take: bool,
        receiver: String,
        count: String,
        /// Byte range from receiver end to count start.
        call: (usize, usize),
        /// Byte range from count end to closing `)` end.
        close: (usize, usize),
        /// Byte range of the method name.
        method: (usize, usize),
    },
    /// `x.to_be_bytes()` <-> `x.to_le_bytes()`, `x.to_be()` <-> `x.to_le()` on multi-byte ints.
    EndianSwap {
        op: EndianOp,
        receiver: String,
        /// Byte range of `.to_be_bytes()` / `.to_be()`.
        call: (usize, usize),
        /// Byte range of the method name.
        method: (usize, usize),
    },
    /// `x.leading_zeros()` <-> `x.trailing_zeros()`, `leading_ones` <-> `trailing_ones`.
    BitCount {
        op: BitCountOp,
        receiver: String,
        /// Byte range of `.leading_zeros()`.
        call: (usize, usize),
        /// Byte range of the method name.
        method: (usize, usize),
    },
    /// Unlabeled, value-less `break` <-> `continue` directly inside a loop.
    LoopCtrl { is_break: bool },
    /// `[lit1, lit2, ...].contains(&x)`: drop one literal element at a time.
    LiteralSet {
        receiver: String,
        argument: String,
        /// Byte range from receiver end to argument start (`.contains(`).
        call: (usize, usize),
        /// Byte range from argument end to closing `)` end.
        close: (usize, usize),
        /// `(element text, removal start, removal end)` for each element.
        elements: Vec<(String, usize, usize)>,
    },
    /// `(lhs..rhs).contains(arg)` <-> `(lhs..=rhs).contains(arg)` on primitive ordered types.
    RangeContains {
        inclusive: bool,
        lhs: String,
        rhs: String,
        argument: String,
        /// Byte range from site start to `lhs` start (leading `(`).
        open: (usize, usize),
        /// Byte range of `..` / `..=` and surrounding whitespace.
        gap: (usize, usize),
        /// Byte range from `rhs` end to `arg` start (`).contains(`).
        call: (usize, usize),
        /// Byte range from `arg` end to closing `)` end.
        close: (usize, usize),
    },
    /// `lhs..rhs` or `lhs..=rhs` on integers (or inside an indexing expression): the end
    /// moves by one.
    Range {
        inclusive: bool,
        lhs: String,
        rhs: String,
        gap: (usize, usize),
        /// Present when the range is probed by switching an enclosing indexing expression
        /// (`&xs[a..b]`, `&mut xs[a..b]`, or `xs[a..b].read_method(...)`).
        index: Option<RangeIndex>,
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

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundingOp {
    Floor,
    Ceil,
    Round,
    Trunc,
}

impl RoundingOp {
    pub fn code(self) -> u8 {
        match self {
            RoundingOp::Floor => 0,
            RoundingOp::Ceil => 1,
            RoundingOp::Round => 2,
            RoundingOp::Trunc => 3,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            RoundingOp::Floor => "floor",
            RoundingOp::Ceil => "ceil",
            RoundingOp::Round => "round",
            RoundingOp::Trunc => "trunc",
        }
    }

    pub fn alt1(self) -> RoundingOp {
        match self {
            RoundingOp::Floor => RoundingOp::Trunc,
            RoundingOp::Ceil => RoundingOp::Floor,
            RoundingOp::Round => RoundingOp::Floor,
            RoundingOp::Trunc => RoundingOp::Floor,
        }
    }

    pub fn alt2(self) -> RoundingOp {
        match self {
            RoundingOp::Floor => RoundingOp::Ceil,
            RoundingOp::Ceil => RoundingOp::Trunc,
            RoundingOp::Round => RoundingOp::Trunc,
            RoundingOp::Trunc => RoundingOp::Round,
        }
    }

    /// Concrete input description distinguishing `self` from `other`.
    pub fn differs_for(self, other: RoundingOp) -> &'static str {
        match (self, other) {
            (RoundingOp::Floor, RoundingOp::Trunc) | (RoundingOp::Trunc, RoundingOp::Floor) => {
                "negative and not a whole number (e.g. -0.5)"
            }
            (RoundingOp::Ceil, RoundingOp::Trunc) | (RoundingOp::Trunc, RoundingOp::Ceil) => {
                "positive and not a whole number (e.g. 0.5)"
            }
            (RoundingOp::Floor, RoundingOp::Round) | (RoundingOp::Round, RoundingOp::Floor) => {
                "a number that rounds up (e.g. 1.5 or -1.4)"
            }
            (RoundingOp::Ceil, RoundingOp::Round) | (RoundingOp::Round, RoundingOp::Ceil) => {
                "a number that rounds down (e.g. 1.4 or -1.6)"
            }
            (RoundingOp::Round, RoundingOp::Trunc) | (RoundingOp::Trunc, RoundingOp::Round) => {
                "a number that rounds away from zero (e.g. 1.5 or -1.6)"
            }
            _ => "not a whole number (e.g. 0.5)",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SaturatingOp {
    SatAdd,
    SatSub,
    SatMul,
    WrapAdd,
    WrapSub,
    WrapMul,
}

impl SaturatingOp {
    pub fn code(self) -> u8 {
        match self {
            SaturatingOp::SatAdd => 0,
            SaturatingOp::SatSub => 1,
            SaturatingOp::SatMul => 2,
            SaturatingOp::WrapAdd => 3,
            SaturatingOp::WrapSub => 4,
            SaturatingOp::WrapMul => 5,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            SaturatingOp::SatAdd => "saturating_add",
            SaturatingOp::SatSub => "saturating_sub",
            SaturatingOp::SatMul => "saturating_mul",
            SaturatingOp::WrapAdd => "wrapping_add",
            SaturatingOp::WrapSub => "wrapping_sub",
            SaturatingOp::WrapMul => "wrapping_mul",
        }
    }

    pub fn swapped(self) -> Self {
        match self {
            SaturatingOp::SatAdd => SaturatingOp::WrapAdd,
            SaturatingOp::SatSub => SaturatingOp::WrapSub,
            SaturatingOp::SatMul => SaturatingOp::WrapMul,
            SaturatingOp::WrapAdd => SaturatingOp::SatAdd,
            SaturatingOp::WrapSub => SaturatingOp::SatSub,
            SaturatingOp::WrapMul => SaturatingOp::SatMul,
        }
    }

    pub fn overflow_phrase(self) -> &'static str {
        match self {
            SaturatingOp::SatAdd | SaturatingOp::WrapAdd => "whose sum overflows",
            SaturatingOp::SatSub | SaturatingOp::WrapSub => {
                "whose difference underflows or overflows"
            }
            SaturatingOp::SatMul | SaturatingOp::WrapMul => "whose product overflows",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrimOp {
    Trim,
    TrimStart,
    TrimEnd,
}

impl TrimOp {
    pub fn code(self) -> u8 {
        match self {
            TrimOp::Trim => 0,
            TrimOp::TrimStart => 1,
            TrimOp::TrimEnd => 2,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            TrimOp::Trim => "trim",
            TrimOp::TrimStart => "trim_start",
            TrimOp::TrimEnd => "trim_end",
        }
    }

    pub fn alt1(self) -> Self {
        match self {
            TrimOp::Trim => TrimOp::TrimStart,
            TrimOp::TrimStart | TrimOp::TrimEnd => TrimOp::Trim,
        }
    }

    pub fn alt2(self) -> Option<Self> {
        match self {
            TrimOp::Trim => Some(TrimOp::TrimEnd),
            TrimOp::TrimStart | TrimOp::TrimEnd => None,
        }
    }

    pub fn whitespace_phrase(self, alt: TrimOp) -> &'static str {
        match (self, alt) {
            (TrimOp::Trim, TrimOp::TrimStart) | (TrimOp::TrimStart, TrimOp::Trim) => {
                "trailing whitespace"
            }
            (TrimOp::Trim, TrimOp::TrimEnd) | (TrimOp::TrimEnd, TrimOp::Trim) => {
                "leading whitespace"
            }
            _ => "leading or trailing whitespace",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndianOp {
    ToBeBytes,
    ToLeBytes,
    ToBe,
    ToLe,
}

impl EndianOp {
    pub fn code(self) -> u8 {
        match self {
            EndianOp::ToBeBytes => 0,
            EndianOp::ToLeBytes => 1,
            EndianOp::ToBe => 2,
            EndianOp::ToLe => 3,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            EndianOp::ToBeBytes => "to_be_bytes",
            EndianOp::ToLeBytes => "to_le_bytes",
            EndianOp::ToBe => "to_be",
            EndianOp::ToLe => "to_le",
        }
    }

    pub fn swapped(self) -> Self {
        match self {
            EndianOp::ToBeBytes => EndianOp::ToLeBytes,
            EndianOp::ToLeBytes => EndianOp::ToBeBytes,
            EndianOp::ToBe => EndianOp::ToLe,
            EndianOp::ToLe => EndianOp::ToBe,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BitCountOp {
    LeadingZeros,
    TrailingZeros,
    LeadingOnes,
    TrailingOnes,
}

impl BitCountOp {
    pub fn code(self) -> u8 {
        match self {
            BitCountOp::LeadingZeros => 0,
            BitCountOp::TrailingZeros => 1,
            BitCountOp::LeadingOnes => 2,
            BitCountOp::TrailingOnes => 3,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            BitCountOp::LeadingZeros => "leading_zeros",
            BitCountOp::TrailingZeros => "trailing_zeros",
            BitCountOp::LeadingOnes => "leading_ones",
            BitCountOp::TrailingOnes => "trailing_ones",
        }
    }

    pub fn swapped(self) -> Self {
        match self {
            BitCountOp::LeadingZeros => BitCountOp::TrailingZeros,
            BitCountOp::TrailingZeros => BitCountOp::LeadingZeros,
            BitCountOp::LeadingOnes => BitCountOp::TrailingOnes,
            BitCountOp::TrailingOnes => BitCountOp::LeadingOnes,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RangeIndexMode {
    /// `&xs[a..b]`
    Ref,
    /// `&mut xs[a..b]`
    RefMut,
    /// `xs[a..b]` used as the receiver of a known `&self` read-only method.
    Deref,
    /// `xs.get(a..b)`
    Get,
    /// `xs.get_mut(a..b)`
    GetMut,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RangeIndex {
    pub mode: RangeIndexMode,
    /// Source text of the plain-place base expression (e.g. `xs`, `self.source`).
    pub base: String,
    /// Byte range from outer expression start to range start (`&xs[`, `&mut xs[`, or `xs[`).
    pub head: (usize, usize),
    /// Byte range from range end to closing `]` end.
    pub close_bracket: (usize, usize),
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
