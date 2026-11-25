//! Probe rendering and compiler-error site blaming.

use super::discover::variable_name;
use super::model::{ArmGuard, RUNTIME_MOD, Site, SiteKind};

/// Instrumented text plus the mapping back to the original, used to blame a site for a
/// compile error in the rewritten file.
#[derive(Debug, PartialEq)]
pub struct Rendered {
    pub text: String,
    /// `(output offset, original offset, length)` of each stretch copied from the original,
    /// in output order.
    segments: Vec<(usize, usize, usize)>,
    /// `(output offset, length, site)` of each piece of text a probe added, in output order.
    inserted: Vec<(usize, usize, u32)>,
}

impl Rendered {
    /// Where a stretch of the original that no probe touches is in the instrumented
    /// text. `None` when a probe is in it or next to its ends.
    pub fn copied(&self, from: usize, to: usize) -> Option<(usize, usize)> {
        self.segments
            .iter()
            .find(|(_, start, len)| *start <= from && to <= start + len)
            .map(|(out, start, _)| (out + from - start, out + to - start))
    }

    /// Maps a byte offset in the instrumented text to the original. Offsets inside inserted
    /// text map to the point of insertion.
    /// The site whose probe text contains the byte at `offset` of the instrumented text.
    pub fn inserted_by(&self, offset: usize) -> Option<u32> {
        let idx = self.inserted.partition_point(|(out, _, _)| *out <= offset);
        let (out, len, site) = self.inserted[idx.checked_sub(1)?];
        (offset - out < len).then_some(site)
    }

    pub fn to_original(&self, offset: usize) -> usize {
        let idx = self.segments.partition_point(|(out, _, _)| *out <= offset);
        let Some((out, orig, len)) = idx.checked_sub(1).map(|i| self.segments[i]) else {
            return 0;
        };
        // Past the end of the copied stretch is inserted text, which sits at its end.
        orig + (offset - out).min(len)
    }
}

struct Edit {
    pos: usize,
    end: usize,
    /// 0: closes a site, 1: replaces an operator, 2: opens a site.
    phase: u8,
    /// Closers: innermost first. Openers: outermost first.
    order: i64,
    text: String,
    /// The site the text belongs to.
    site: u32,
}

/// Rewrites `source` with probes for the sites that `enabled` accepts.
pub fn render(source: &str, sites: &[Site], enabled: &dyn Fn(u32) -> bool) -> Rendered {
    let rt = format!("crate::{RUNTIME_MOD}");
    let mut edits = vec![];
    for site in sites.iter().filter(|s| enabled(s.id)) {
        // Edits at one position are ordered by the length of their site, doubled to leave
        // room: a site that wraps a whole expression goes around a site of that very
        // expression (`if a > b` as a guard and as a comparison).
        let wraps = matches!(
            site.kind,
            SiteKind::Return { .. }
                | SiteKind::Body { .. }
                | SiteKind::Guard { .. }
                | SiteKind::Field { .. }
        );
        // tmt:ignore `2 +` orders the same: nothing longer starts or ends where a wrapping site does
        let len = 2 * (site.end - site.start) as i64 + wraps as i64;
        let (open, close) = match &site.kind {
            SiteKind::Cmp { op, gap, .. } => {
                let func = if op.is_ordering() { "ord" } else { "eq" };
                edits.push(Edit {
                    pos: gap.0,
                    end: gap.1,
                    phase: 1,
                    order: 0,
                    text: "), &(".into(),
                    site: site.id,
                });
                (
                    "(match (&(".to_string(),
                    format!(
                        ")) {{ (__tmt_l, __tmt_r) => {{ #[allow(unused_imports)] use {rt}::{{ValAny as _, ValNum as _}}; \
                         {rt}::{func}({id}u32, {code}u8, __tmt_l, __tmt_r, (&{rt}::W(__tmt_l)).tmt_val(), (&{rt}::W(__tmt_r)).tmt_val()) }} }})",
                        id = site.id,
                        code = op.code(),
                    ),
                )
            }
            // In the probes below the user's operands are evaluated where no name the probe
            // introduces is in scope: in the scrutinee of a `match`, in the initialiser of a
            // `let`, or in an arm that binds nothing. A scrutinee keeps its temporaries
            // until the end of the enclosing statement, as an arithmetic operator does.
            SiteKind::Logic { and, gap, .. } => {
                // The right operand stays in place, so it is evaluated exactly when the
                // (possibly mutated) left operand asks for it.
                let id = site.id;
                edits.push(Edit {
                    pos: gap.0,
                    end: gap.1,
                    phase: 1,
                    order: 0,
                    text: format!("); __tmt_c }} {{ 2u8 => {rt}::rhs({id}u32, {and}, "),
                    site: site.id,
                });
                (
                    // Each operand of `&&` / `||` drops its temporaries when it is done.
                    // The `let` ends the left operand's; the arm ends the right one's.
                    format!("(match {{ let __tmt_c: u8 = {rt}::lhs({id}u32, {and}, "),
                    "), __tmt_c => __tmt_c == 1u8 })".to_string(),
                )
            }
            SiteKind::Arith {
                op,
                assign,
                gap,
                cast_probes,
                ..
            } => {
                // A compound assignment hands the probe the place to update.
                let (holder, lhs_open, lhs_close) = if *assign {
                    ("Aa", "&mut (", ")")
                } else {
                    ("Ar", "", "")
                };
                for &(slot, start, end) in cast_probes {
                    let wider = 2 * (end - start) as i64 + 1;
                    edits.push(Edit {
                        pos: start,
                        end: start,
                        phase: 2,
                        order: -wider,
                        text: "(match (".into(),
                        site: site.id,
                    });
                    edits.push(Edit {
                        pos: end,
                        end,
                        phase: 0,
                        order: wider,
                        text: format!(
                            ") {{ __tmt_v => {{ #[allow(unused_imports)] use {rt}::{{CastAny as _, CastPrim as _}}; \
                             {rt}::record_cast({id}u32, {slot}u32, (&{rt}::W(&__tmt_v)).tmt_cast_bounds()); __tmt_v }} }})",
                            id = site.id,
                        ),
                        site: site.id,
                    });
                }
                edits.push(Edit {
                    pos: gap.0,
                    end: gap.1,
                    phase: 1,
                    order: 0,
                    text: format!("{lhs_close}, "),
                    site: site.id,
                });
                (
                    format!("(match ({lhs_open}"),
                    format!(
                        ") {{ (__tmt_a, __tmt_b) => {{ #[allow(unused_imports)] use {rt}::{{{any} as _, {prim} as _}}; \
                         (&{rt}::{holder}::new(__tmt_a, __tmt_b)).{method}({id}u32) }} }})",
                        any = op.fallback_trait(*assign),
                        prim = op.prim_trait(*assign),
                        method = op.method(*assign),
                        id = site.id,
                    ),
                )
            }
            SiteKind::Not { bang, .. } => {
                // Drop the `!`; the probe applies it (or not).
                edits.push(Edit {
                    pos: bang.0,
                    end: bang.1,
                    phase: 1,
                    order: 0,
                    text: String::new(),
                    site: site.id,
                });
                (
                    "(match (".to_string(),
                    format!(
                        ") {{ __tmt_a => {{ #[allow(unused_imports)] use {rt}::{{AnyNot as _, NotPrim as _}}; \
                         (&{rt}::Un::new(__tmt_a)).tmt_not({}u32) }} }})",
                        site.id
                    ),
                )
            }
            SiteKind::Return { option, .. } => match option {
                None => (
                    " let __tmt_v: bool = {".to_string(),
                    format!("}}; {rt}::ret({}u32, __tmt_v) ", site.id),
                ),
                Some(ty) => (
                    format!(" let __tmt_v: {ty} = {{"),
                    format!("}}; {rt}::ret_opt({}u32, __tmt_v) ", site.id),
                ),
            },
            SiteKind::MinMax {
                max, call, close, ..
            } => {
                // The receiver is borrowed, as a method call borrows it: a `max` that takes
                // `&self` leaves it where it is, and one that takes it by value copies it
                // out (or, for a type that cannot be copied, does not compile: no probe).
                edits.push(Edit {
                    pos: call.0,
                    end: call.1,
                    phase: 1,
                    order: 0,
                    text: "), ".into(),
                    site: site.id,
                });
                // The call's own `)` (and a trailing comma) is replaced by the probe.
                // The call is made as it was written, on the same operands; the probe only
                // decides which of the two it is.
                let (same, other) = if *max { ("max", "min") } else { ("min", "max") };
                edits.push(Edit {
                    pos: close.0,
                    end: close.1,
                    phase: 1,
                    order: 0,
                    text: format!(
                        ") {{ (__tmt_a, __tmt_b) => {{ #[allow(unused_imports)] use {rt}::{{AnyMinMax as _, MinMaxPrim as _}}; \
                         if (&{rt}::Mm(__tmt_a, &__tmt_b)).tmt_other({}u32) {{ (*__tmt_a).{other}(__tmt_b) }} else {{ (*__tmt_a).{same}(__tmt_b) }} }} }})",
                        site.id
                    ),
                    site: site.id,
                });
                ("(match (&(".to_string(), String::new())
            }
            SiteKind::SliceEnd {
                first,
                receiver,
                call,
                ..
            } => {
                // Both calls are made as they are written. Only where the runtime knows
                // the receiver's type does it ever ask for the other one; of anything else
                // it has nothing to say, whatever the call returns.
                let (same, other) = if *first {
                    ("first", "last")
                } else {
                    ("last", "first")
                };
                let id = site.id;
                let decide = format!(
                    "#[allow(unused_imports)] use {rt}::{{AnyEnd as _, SliceEnd as _}}; \
                     match (&{rt}::Ends(__TMT_BORROWED__)).tmt_other_end({id}u32, {first}) {{ \
                     {rt}::End::Other => {{ let __tmt_o = (__TMT_PLACE__).{other}(); \
                     if false {{ {rt}::same_type(&__tmt_o, &(__TMT_PLACE__).{same}()); }} __tmt_o }}, \
                     {rt}::End::Same(__tmt_s) => {{ \
                     let __tmt_r = (__TMT_PLACE__).{same}(); \
                     __tmt_s.tmt_end_seen({id}u32, &__tmt_r); __tmt_r }} }}"
                );
                // A variable is the same place each time it is named, so it is named
                // again for the call: the call then borrows it exactly as it did (a `&mut`
                // for as long as it is one). Any other receiver is evaluated once and
                // borrowed for the probe.
                let (open, close) = if let Some(name) = variable_name(receiver) {
                    // The receiver may have the name the probe would give.
                    let seen = if name == "__tmt_s" {
                        "__tmt_t"
                    } else {
                        "__tmt_s"
                    };
                    let decide = decide
                        .replace("__tmt_s", seen)
                        .replace("__TMT_PLACE__", &name);
                    let (head, tail) = decide
                        .split_once("__TMT_BORROWED__")
                        .expect("the receiver is borrowed once");
                    (format!("({{ {head}&("), format!("){tail} }})"))
                } else {
                    let decide = decide
                        .replace("__TMT_BORROWED__", "__tmt_a")
                        .replace("__TMT_PLACE__", "*__tmt_a");
                    (
                        "(match (&(".to_string(),
                        format!(")) {{ __tmt_a => {{ {decide} }} }})"),
                    )
                };
                edits.push(Edit {
                    pos: call.0,
                    end: call.1,
                    phase: 1,
                    order: 0,
                    text: close,
                    site: site.id,
                });
                (open, String::new())
            }
            SiteKind::StrAffix {
                starts,
                call,
                close,
                ..
            } => {
                let same = if *starts { "starts_with" } else { "ends_with" };
                edits.push(Edit {
                    pos: call.0,
                    end: call.1,
                    phase: 1,
                    order: 0,
                    text: "), ".into(),
                    site: site.id,
                });
                edits.push(Edit {
                    pos: close.0,
                    end: close.1,
                    phase: 1,
                    order: 0,
                    text: format!(
                        ") {{ (__tmt_a, __tmt_p) => {{ #[allow(unused_imports)] use {rt}::{{AnyStrAffix as _, StrAffixPrim as _}}; \
                         (&{rt}::Aff::new(__tmt_a, __tmt_p)).tmt_str_affix({}u32, {starts}, |__tmt_a, __tmt_p| (*__tmt_a).{same}(__tmt_p)) }} }})",
                        site.id
                    ),
                    site: site.id,
                });
                ("(match (&(".to_string(), String::new())
            }
            SiteKind::StrReplace {
                replace_all,
                call,
                comma,
                close,
                ..
            } => {
                let same_call = if *replace_all {
                    "(*__tmt_a).replace(__tmt_f, __tmt_t)"
                } else {
                    "(*__tmt_a).replacen(__tmt_f, __tmt_t, 1)"
                };
                edits.push(Edit {
                    pos: call.0,
                    end: call.1,
                    phase: 1,
                    order: 0,
                    text: "), ".into(),
                    site: site.id,
                });
                edits.push(Edit {
                    pos: comma.0,
                    end: comma.1,
                    phase: 1,
                    order: 0,
                    text: ", ".into(),
                    site: site.id,
                });
                edits.push(Edit {
                    pos: close.0,
                    end: close.1,
                    phase: 1,
                    order: 0,
                    text: format!(
                        ") {{ (__tmt_a, __tmt_f, __tmt_t) => {{ #[allow(unused_imports)] use {rt}::{{AnyStrReplace as _, StrReplacePrim as _}}; \
                         (&{rt}::Rep::new(__tmt_a, __tmt_f, __tmt_t)).tmt_str_replace({}u32, {replace_all}, |__tmt_a, __tmt_f, __tmt_t| {same_call}) }} }})",
                        site.id
                    ),
                    site: site.id,
                });
                ("(match (&(".to_string(), String::new())
            }
            SiteKind::Clamp {
                call, comma, close, ..
            } => {
                edits.push(Edit {
                    pos: call.0,
                    end: call.1,
                    phase: 1,
                    order: 0,
                    text: "), ".into(),
                    site: site.id,
                });
                edits.push(Edit {
                    pos: comma.0,
                    end: comma.1,
                    phase: 1,
                    order: 0,
                    text: ", ".into(),
                    site: site.id,
                });
                edits.push(Edit {
                    pos: close.0,
                    end: close.1,
                    phase: 1,
                    order: 0,
                    text: format!(
                        ") {{ (__tmt_a, __tmt_lo, __tmt_hi) => {{ #[allow(unused_imports)] use {rt}::{{AnyClamp as _, ClampPrim as _}}; \
                         (&{rt}::Clp::new(__tmt_a, __tmt_lo, __tmt_hi)).tmt_clamp({}u32, |__tmt_a, __tmt_lo, __tmt_hi| (*__tmt_a).clamp(__tmt_lo, __tmt_hi)) }} }})",
                        site.id
                    ),
                    site: site.id,
                });
                ("(match (&(".to_string(), String::new())
            }
            SiteKind::Rounding { op, call, .. } => {
                let same = op.name();
                let op_code = op.code();
                edits.push(Edit {
                    pos: call.0,
                    end: call.1,
                    phase: 1,
                    order: 0,
                    text: format!(
                        ")) {{ __tmt_a => {{ #[allow(unused_imports)] use {rt}::{{AnyRound as _, RoundPrim as _}}; \
                         (&{rt}::Rnd(__tmt_a)).tmt_round({}u32, {op_code}u8, || (*__tmt_a).{same}()) }} }})",
                        site.id
                    ),
                    site: site.id,
                });
                ("(match (&(".to_string(), String::new())
            }
            SiteKind::Quantifier {
                all, call, close, ..
            } => {
                let same = if *all { "all" } else { "any" };
                edits.push(Edit {
                    pos: call.0,
                    end: call.1,
                    phase: 1,
                    order: 0,
                    text: ").__tmt_quant_mut(), ".into(),
                    site: site.id,
                });
                edits.push(Edit {
                    pos: close.0,
                    end: close.1,
                    phase: 1,
                    order: 0,
                    text: format!(
                        ") {{ (__tmt_q, __tmt_f) => (&__tmt_q).tmt_quant({}u32, {all}, __tmt_f, |__tmt_i, __tmt_f| (*__tmt_i).{same}(__tmt_f)) }} }})",
                        site.id
                    ),
                    site: site.id,
                });
                (
                    format!(
                        "({{ #[allow(unused_imports)] use {rt}::{{AnyQuant as _, QuantPrim as _, TmtQuantExt as _}}; match (("
                    ),
                    String::new(),
                )
            }
            SiteKind::LiteralSet { .. } => continue,
            SiteKind::Range { inclusive, gap, .. } => {
                edits.push(Edit {
                    pos: gap.0,
                    end: gap.1,
                    phase: 1,
                    order: 0,
                    text: ", ".into(),
                    site: site.id,
                });
                let method = if *inclusive {
                    "tmt_range_inclusive"
                } else {
                    "tmt_range"
                };
                (
                    "(match (".to_string(),
                    format!(
                        ") {{ (__tmt_a, __tmt_b) => {{ #[allow(unused_imports)] use {rt}::{{AnyRange as _, RangePrim as _}}; \
                         (&{rt}::Ar::new(__tmt_a, __tmt_b)).{method}({}u32) }} }})",
                        site.id
                    ),
                )
            }
            SiteKind::Arm { guard, .. } => {
                // The probe goes into the arm's guard, where it runs only when the
                // pattern matched.
                match guard {
                    ArmGuard::None(arrow) => edits.push(Edit {
                        pos: *arrow,
                        end: *arrow,
                        phase: 2,
                        order: 0,
                        text: format!("if {rt}::arm({}u32) ", site.id),
                        site: site.id,
                    }),
                    ArmGuard::Existing(from, to) => {
                        let wider = 2 * (to - from + 1) as i64;
                        edits.push(Edit {
                            pos: *from,
                            end: *from,
                            phase: 2,
                            order: -wider,
                            text: format!("{rt}::arm({}u32) && (", site.id),
                            site: site.id,
                        });
                        edits.push(Edit {
                            pos: *to,
                            end: *to,
                            phase: 0,
                            order: wider,
                            text: ")".into(),
                            site: site.id,
                        });
                    }
                }
                (String::new(), String::new())
            }
            SiteKind::Stmt => (
                format!("if {rt}::live({}u32) {{ ", site.id),
                " }".to_string(),
            ),
            // A literal takes its type from where it stands, so it stays where it is.
            SiteKind::Neg {
                operand,
                literal: true,
                ..
            } => (
                format!("(if {rt}::live({}u32) {{ ", site.id),
                format!(" }} else {{ {operand} }})"),
            ),
            SiteKind::Neg { minus, .. } => {
                edits.push(Edit {
                    pos: minus.0,
                    end: minus.1,
                    phase: 1,
                    order: 0,
                    text: String::new(),
                    site: site.id,
                });
                (
                    "(match (".to_string(),
                    format!(
                        ") {{ __tmt_a => if {rt}::live({}u32) {{ -__tmt_a }} else {{ __tmt_a }} }})",
                        site.id
                    ),
                )
            }
            SiteKind::Guard { .. } => (format!("{rt}::ret({}u32, ", site.id), ")".to_string()),
            SiteKind::Field {
                name,
                shorthand,
                from_base: Some(from_base),
                ..
            } => (
                format!(
                    "{}if {rt}::live({}u32) {{ ",
                    if *shorthand {
                        format!("{name}: ")
                    } else {
                        String::new()
                    },
                    site.id
                ),
                format!(" }} else {{ {from_base} }}"),
            ),
            SiteKind::Field {
                from_base: None, ..
            } => continue,
            SiteKind::Body { value, .. } => (
                format!(" if !{rt}::live({}u32) {{ return {value}; }}", site.id),
                String::new(),
            ),
        };
        edits.push(Edit {
            pos: site.start,
            end: site.start,
            phase: 2,
            order: -len,
            text: open,
            site: site.id,
        });
        edits.push(Edit {
            pos: site.end,
            end: site.end,
            phase: 0,
            order: len,
            text: close,
            site: site.id,
        });
    }
    edits.sort_by_key(|e| (e.pos, e.phase, e.order));

    // tmt:ignore the capacity is a hint: any number gives the same text
    let capacity = source.len() + edits.iter().map(|e| e.text.len()).sum::<usize>();
    let mut text = String::with_capacity(capacity);
    let mut segments = vec![];
    let mut inserted = vec![];
    let mut cursor = 0;
    for edit in edits {
        // Also when nothing lies between two edits: text inserted right after a replaced
        // stretch then maps to where it was inserted, not to where the stretch began.
        if edit.pos >= cursor {
            segments.push((text.len(), cursor, edit.pos - cursor));
            text.push_str(&source[cursor..edit.pos]);
        }
        // A line that ends inside a replaced stretch still ends: `line!()` and the place
        // of a panic after it are what they were.
        let replaced = &source[cursor.max(edit.pos)..cursor.max(edit.end)];
        let lines = "\n".repeat(replaced.matches('\n').count());
        inserted.push((text.len(), edit.text.len() + lines.len(), edit.site));
        text.push_str(&edit.text);
        text.push_str(&lines);
        cursor = cursor.max(edit.end);
    }
    segments.push((text.len(), cursor, source.len() - cursor));
    text.push_str(&source[cursor..]);
    Rendered {
        text,
        segments,
        inserted,
    }
}

/// The sites to blame for a compile error at `offset` (original coordinates): the smallest
/// enabled sites that contain it.
pub fn blame(sites: &[Site], enabled: &dyn Fn(u32) -> bool, offset: usize) -> Vec<u32> {
    // A replaced body is blamed for what is wrong in its own text only (`inserted_by`):
    // by position it would answer for everything in the function.
    let containing = || {
        sites.iter().filter(|s| {
            enabled(s.id)
                && s.start <= offset
                && offset <= s.end
                && !matches!(s.kind, SiteKind::Body { .. })
        })
    };
    let Some(smallest) = containing().map(|s| s.end - s.start).min() else {
        return vec![];
    };
    containing()
        .filter(|s| s.end - s.start == smallest)
        .map(|s| s.id)
        .collect()
}
