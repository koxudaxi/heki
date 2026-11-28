//! The types the standard library's stubs declare for call results and attribute reads
//! ([`heki_kb::StubTable`]), as receiver types: a row's type text with `Self` bound to the
//! receiver and `~N` to the receiver's N-th type argument. The hand catalog's result
//! (`return_type`, its argument cases, `[[attributes]]`) answers first wherever it has one; this
//! table answers what it leaves open. An overload is chosen only by whether a subscript's index
//! is a slice; otherwise the result is the join of the overloads' results, of no type when one
//! of them says nothing (`Any`, a function's own type variable) or past [`MAX_UNION`] classes.
//!
//! [`MAX_UNION`]: crate::types::MAX_UNION

use crate::resolver::{split_top_level, subscript};
use crate::types::{Args, Elem, Generic, Obj, Ty, generic, generic_args};
use heki_kb::{StubCall, StubKind, StubTable};

/// Whether a subscript's index is a slice, which chooses among the overloads of `__getitem__`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Index {
    Item,
    Slice,
}

/// What a row's `Self` and type parameters stand for.
struct Binding<'r> {
    self_ty: Option<&'r Ty>,
    params: Vec<Elem>,
}

impl Binding<'_> {
    fn decode(&self, text: &str) -> Option<Ty> {
        let members = split_top_level(text, '|');
        if members.len() > 1 {
            let mut objs = Vec::new();
            for member in members {
                let t = self.decode(member)?;
                if t.members().is_empty() {
                    return None;
                }
                objs.extend(t.members().iter().cloned());
            }
            return Ty::of_members(objs);
        }
        match text {
            "Self" => return self.self_ty.cloned(),
            "?" | "..." | "()" => return None,
            _ => {}
        }
        if let Some(n) = text.strip_prefix('~') {
            return match self.params.get(n.parse::<usize>().ok()?)? {
                Elem::Of(t) => Some(t.clone()),
                Elem::Empty | Elem::Unknown => None,
            };
        }
        let Some((head, inner)) = subscript(text) else {
            return Some(Ty::inst(text));
        };
        if head == "type" {
            return Some(Ty::Class(self.decode(inner)?.obj()?.class.clone()));
        }
        let args = split_top_level(inner, ',');
        let args = match generic(head) {
            Some(kind) => generic_args(
                kind,
                &args,
                |a| Elem::of(self.decode(a)),
                |result| Args::returns(self.decode(result)),
            ),
            // Arguments are tracked only for the generic containers, as for an annotation
            // (`ProjectDecls::declared_ty`).
            None => Args::None,
        };
        Some(Ty::container(head, args))
    }

    /// The parameters of the MRO entry `base` (`builtins.dict[~0, builtins.int]`), in terms of
    /// these.
    fn of_base(&self, base: &str) -> Vec<Elem> {
        subscript(base).map_or_else(Vec::new, |(_, inner)| {
            (split_top_level(inner, ',').into_iter())
                .map(|a| Elem::of(self.decode(a)))
                .collect()
        })
    }
}

/// The type arguments of an instance of a generic class by the position of the class's type
/// parameters (`dict[K, V]`: key, value).
fn params(o: &Obj) -> Vec<Elem> {
    match (&o.args, generic(&o.class)) {
        (Args::Items(pair), Some(Generic::Pairs)) => match &**pair {
            Elem::Of(Ty::Inst(Obj {
                args: Args::Tuple(kv),
                ..
            })) if kv.len() == 2 => kv.clone(),
            _ => Vec::new(),
        },
        (Args::Items(item) | Args::Result(item), _) => vec![(**item).clone()],
        (Args::Map(key, _), Some(Generic::Counter)) => vec![(**key).clone()],
        (Args::Map(key, value), _) => vec![(**key).clone(), (**value).clone()],
        (Args::Tuple(items), _) => vec![items.iter().fold(Elem::Empty, |acc, e| acc.join(e))],
        _ => Vec::new(),
    }
}

/// What a call `call` gives: the join of the results its overloads give for a subscript of
/// kind `index` (every overload when the call is no subscript).
fn result(call: StubCall, binding: &Binding<'_>, index: Option<Index>) -> Option<Ty> {
    let mut joined: Option<Ty> = None;
    for alt in call.results() {
        let (chosen_for, text) = match alt.split_once(':') {
            Some(("i", text)) => (Some(Index::Item), text),
            Some(("s", text)) => (Some(Index::Slice), text),
            _ => (None, alt),
        };
        if index.is_some() && chosen_for.is_some() && chosen_for != index {
            continue;
        }
        let t = binding.decode(text)?;
        joined = Some(match joined {
            Some(j) => j.join(&t)?,
            None => t,
        });
    }
    joined
}

/// The binding of a member of `class` found at `base` along its MRO, called or read on `recv`:
/// the receiver's type arguments are the class's parameters only when it is an instance of
/// that class itself (a project subclass's are its own).
fn binding<'r>(stubs: &StubTable, class: &str, recv: &'r Ty, base: Option<&str>) -> Binding<'r> {
    let own = match recv {
        Ty::Inst(o) if stubs.canonical(&o.class) == stubs.canonical(class) => params(o),
        _ => Vec::new(),
    };
    let receiver = Binding {
        self_ty: Some(recv),
        params: own,
    };
    match base {
        Some(base) => Binding {
            params: receiver.of_base(base),
            self_ty: Some(recv),
        },
        None => receiver,
    }
}

/// What calling the library function `fqn` by name returns, or the class or static method
/// `fqn` names on its class (`datetime.datetime.now`), `Self` being an instance of that class.
/// An instance method is typed only with its receiver ([`method_result`]).
pub(crate) fn function_result(stubs: &StubTable, fqn: &str) -> Option<Ty> {
    if let Some(call) = stubs.function(fqn)
        && call.kind == StubKind::Function
    {
        let unbound = Binding {
            self_ty: None,
            params: Vec::new(),
        };
        return result(call, &unbound, None);
    }
    let (class, name) = fqn.rsplit_once('.')?;
    let member = stubs.method(class, name)?;
    if member.found.kind != StubKind::OnClass {
        return None;
    }
    let instance = Ty::inst(class);
    result(
        member.found,
        &binding(stubs, class, &instance, member.base),
        None,
    )
}

/// What calling the method `symbol` (`class.method`, the library method a call resolves to) on
/// `recv` returns; `on_instance` tells whether `recv` is an instance, which an instance method
/// needs.
pub(crate) fn method_result(
    stubs: &StubTable,
    symbol: &str,
    recv: &Ty,
    on_instance: bool,
    index: Option<Index>,
) -> Option<Ty> {
    let (class, name) = symbol.rsplit_once('.')?;
    let member = stubs.method(class, name)?;
    if !on_instance && member.found.kind != StubKind::OnClass {
        return None;
    }
    result(
        member.found,
        &binding(stubs, class, recv, member.base),
        index,
    )
}

/// What reading the attribute or property `attr` on `recv`, an instance of the library class
/// `class`, gives.
pub(crate) fn attribute_result(
    stubs: &StubTable,
    class: &str,
    attr: &str,
    recv: &Ty,
) -> Option<Ty> {
    let member = stubs.attribute(class, attr)?;
    binding(stubs, class, recv, member.base).decode(member.found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unbound() -> Binding<'static> {
        Binding {
            self_ty: None,
            params: Vec::new(),
        }
    }

    fn inst(class: &str) -> Elem {
        Elem::Of(Ty::inst(class))
    }

    #[test]
    fn a_union_with_a_class_object_has_no_type() {
        assert_eq!(unbound().decode("type[builtins.str]|builtins.int"), None);
        assert_eq!(
            unbound().decode("builtins.str|builtins.int"),
            Ty::of_members([
                Obj {
                    class: "builtins.int".into(),
                    args: Args::None,
                },
                Obj {
                    class: "builtins.str".into(),
                    args: Args::None,
                },
            ])
        );
    }

    #[test]
    fn type_arguments_are_the_parameters_by_position() {
        let pairs = Binding {
            self_ty: None,
            params: Vec::new(),
        }
        .decode("typing.ItemsView[builtins.str, builtins.int]")
        .unwrap();
        assert_eq!(
            params(pairs.obj().unwrap()),
            [inst("builtins.str"), inst("builtins.int")]
        );
        let odd_pairs = Obj {
            class: "typing.ItemsView".into(),
            args: Args::items(inst("builtins.str")),
        };
        assert!(params(&odd_pairs).is_empty());
        let counter = unbound()
            .decode("collections.Counter[builtins.str]")
            .unwrap();
        assert_eq!(params(counter.obj().unwrap()), [inst("builtins.str")]);
        let tuple = unbound()
            .decode("builtins.tuple[builtins.int, builtins.int]")
            .unwrap();
        assert_eq!(params(tuple.obj().unwrap()), [inst("builtins.int")]);
        let callable = unbound()
            .decode("collections.abc.Callable[builtins.int]")
            .unwrap();
        assert_eq!(callable.obj().unwrap().args, Args::None);
    }

    #[test]
    fn an_instance_method_needs_an_instance() {
        let kb = heki_kb::EffectiveEcosystemKb::load_builtin(&[]).unwrap();
        let text = Ty::inst("builtins.str");
        let strip =
            |on_instance| method_result(kb.stubs(), "builtins.str.strip", &text, on_instance, None);
        assert_eq!(strip(true), Some(Ty::inst("builtins.str")));
        assert_eq!(strip(false), None);
    }
}
