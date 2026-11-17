//! A surface item id as people read it: copied from the game's `sunrise-mod-surface` (tests/test_launcher_copies.py).

/// What an item is, as people read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Hook,
    /// One argument of a hook.
    Argument,
    Trait,
    Method,
    AssociatedItem,
    Function,
    Macro,
    PartKind,
    Capability,
    Type,
    Variant,
    Field,
}

/// An id's kind and the name a mod writes for it; `None` for an id of no kind a mod's code can use.
pub fn item(id: &str) -> Option<(ItemKind, String)> {
    let (kind, rest) = id.split_once(':')?;
    Some(match kind {
        "hook" => match rest.split_once('.') {
            Some((hook, argument)) => (ItemKind::Argument, format!("{hook}.{argument}")),
            None => (ItemKind::Hook, rest.to_owned()),
        },
        "trait" => (ItemKind::Trait, rest.to_owned()),
        "method" | "handle" => (ItemKind::Method, rest.to_owned()),
        "assoc" => (ItemKind::AssociatedItem, rest.to_owned()),
        "fn" => (ItemKind::Function, rest.to_owned()),
        "macro" => (ItemKind::Macro, rest.to_owned()),
        "part" => (ItemKind::PartKind, rest.to_owned()),
        "capability" => (ItemKind::Capability, rest.to_owned()),
        "type" => match rest.rsplit_once('.') {
            // By Rust's naming: a variant is capitalised, a field is not (or is a position).
            Some((owner, member)) if member.starts_with(|c: char| c.is_ascii_uppercase()) => {
                (ItemKind::Variant, format!("{owner}::{member}"))
            }
            Some((owner, member)) => (ItemKind::Field, format!("{owner}.{member}")),
            None => (ItemKind::Type, rest.to_owned()),
        },
        _ => return None,
    })
}

/// The identifier a mod writes for an id: the last path segment, or the field or variant name.
pub fn leaf(id: &str) -> &str {
    let Some((kind, rest)) = id.split_once(':') else {
        return id;
    };
    match kind {
        "capability" | "part" | "behaviour" => rest,
        "hook" => rest.rsplit_once('.').map_or(rest, |(_, member)| member),
        "type" => match rest.rsplit_once('.') {
            Some((_, member)) => member,
            None => rest.rsplit("::").next().unwrap_or(rest),
        },
        _ => rest.rsplit("::").next().unwrap_or(rest),
    }
}
