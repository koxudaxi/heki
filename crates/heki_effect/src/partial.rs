//! `functools.partial` objects a function makes: what calling one calls, and with which
//! arguments (`partial(f, a, k=b)(c)` calls `f(a, c, k=b)`). The call targets and the receiver
//! typing both read calls of such objects this way.

use heki_ir::VarId;

/// What calling a `functools.partial` object calls ([`Partial::of`]).
pub(crate) struct Partial {
    pub(crate) callable: VarId,
    /// Each nested `functools.partial` call: the object it makes and the callable it holds.
    pub(crate) calls: Vec<(VarId, VarId)>,
    /// The positional arguments it passes first, up to the first one `*` unpacks.
    pub(crate) args: Vec<VarId>,
    /// Its keywords (`**` unpacking is a `None` keyword).
    pub(crate) kwargs: Vec<(Option<String>, VarId)>,
    /// It unpacks positional arguments with `*`: the positions after `args` are not known.
    pub(crate) open: bool,
}

/// A call of a `functools.partial` object, as a call of the callable it holds
/// ([`Partial::called_with`]).
pub(crate) struct PartialCall {
    pub(crate) callable: VarId,
    /// [`Partial::calls`].
    pub(crate) calls: Vec<(VarId, VarId)>,
    pub(crate) args: Vec<VarId>,
    pub(crate) kwargs: Vec<(Option<String>, VarId)>,
    /// Where `*` unpacking starts among `args`.
    pub(crate) star_from: Option<u8>,
}

/// The parts of a `functools.partial(f, ...)` call: `f`, the positional arguments after it up
/// to the first one `*` unpacks, the keywords, and whether one is unpacked.
#[allow(clippy::type_complexity)]
pub(crate) fn parts<'a>(
    args: &'a [VarId],
    kwargs: &'a [(Option<String>, VarId)],
    star_from: Option<u8>,
) -> Option<(VarId, &'a [VarId], &'a [(Option<String>, VarId)], bool)> {
    let known = star_from.map_or(args.len(), |s| usize::from(s).min(args.len()));
    let (&inner, held) = args[..known].split_first()?;
    Some((inner, held, kwargs, known < args.len()))
}

impl Partial {
    /// What calling `var` calls when it holds a `functools.partial` object, through copies
    /// (`copied`) and at most 16 nested partials (the inner one's arguments go first; past the
    /// bound, the call is not resolved). `made` gives the [`parts`] of the call that makes the
    /// value of a variable, when it is a call of `functools.partial`.
    #[allow(clippy::type_complexity)]
    pub(crate) fn of<'a>(
        var: VarId,
        copied: impl Fn(VarId) -> VarId,
        made: impl Fn(VarId) -> Option<(VarId, &'a [VarId], &'a [(Option<String>, VarId)], bool)>,
    ) -> Option<Partial> {
        let mut found: Option<Partial> = None;
        let mut cur = var;
        for _ in 0..16 {
            let object = copied(cur);
            let Some((inner, held, kwargs, open)) = made(object) else {
                break;
            };
            let p = found.get_or_insert_with(|| Partial {
                callable: inner,
                calls: Vec::new(),
                args: Vec::new(),
                kwargs: Vec::new(),
                open: false,
            });
            p.callable = inner;
            p.calls.push((object, inner));
            // The outer partials' arguments come after what this one unpacks with `*`.
            if open {
                p.args.clear();
            }
            p.args.splice(0..0, held.iter().copied());
            // An outer partial's `**` unpacking may override any keyword this one holds.
            let overridden = p.kwargs.iter().any(|(k, _)| k.is_none());
            // A keyword an outer partial writes again replaces this one's.
            let replaced: Vec<_> = kwargs
                .iter()
                .filter(|(k, _)| !overridden || k.is_none())
                .filter(|(k, _)| k.is_none() || !p.kwargs.iter().any(|(o, _)| o == k))
                .cloned()
                .collect();
            p.kwargs.splice(0..0, replaced);
            p.open |= open;
            cur = inner;
        }
        found
    }

    /// What a call passing `args`, `kwargs` (`*` unpacking from `star_from`) to the object
    /// passes the callable it holds: the positional arguments it holds, then the call's up to
    /// the first one `*` unpacks; its keywords, which a keyword the call writes replaces, then
    /// the call's. `None` when the call unpacks a mapping with `**` while the object holds
    /// keywords: the mapping may replace any of them.
    pub(crate) fn called_with(
        self,
        args: &[VarId],
        kwargs: &[(Option<String>, VarId)],
        star_from: Option<u8>,
    ) -> Option<PartialCall> {
        let unpacks_mapping = kwargs.iter().any(|(k, _)| k.is_none());
        if unpacks_mapping && !self.kwargs.is_empty() {
            return None;
        }
        let written = |k: &Option<String>| kwargs.iter().any(|(w, _)| w == k);
        let all_kwargs = self
            .kwargs
            .iter()
            .filter(|(k, _)| k.is_none() || !written(k))
            .chain(kwargs)
            .cloned()
            .collect();
        let known = star_from.map_or(args.len(), |s| usize::from(s).min(args.len()));
        let held = self.args.len();
        let (all_args, star_from) = match self.open {
            true => (self.args, Some(held)),
            false => {
                let after = args[..known].iter().copied();
                let star_from = star_from.map(|_| held + known);
                (self.args.into_iter().chain(after).collect(), star_from)
            }
        };
        Some(PartialCall {
            callable: self.callable,
            calls: self.calls,
            args: all_args,
            kwargs: all_kwargs,
            star_from: star_from.map(|s| u8::try_from(s).unwrap_or(u8::MAX)),
        })
    }
}
