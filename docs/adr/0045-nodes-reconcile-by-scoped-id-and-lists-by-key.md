# Nodes reconcile by scoped `id`, and `list` items by `key`

> "Match the remaining fresh children to the remaining retained children" is ambiguous, and the
> first implementation read it the wrong way, so this states the rule exactly. **An `id` means "this
> is the same node, and only the same node", in both directions.** A fresh child carrying an `id`
> matches only a retained child with that same `id`; if none exists it is new and must not draw from
> the positional pool. A fresh child carrying no `id` matches only a retained child carrying no
> `id`. Everything unclaimed is retired child-first.
>
> Read the other way, "remaining" lets an identified-but-unmatched fresh child adopt whatever
> retained node is next in line, and lets an anonymous fresh child inherit a retained node that
> declared an explicit `id`. Both were measured: retained `[a, b, c]` against fresh `[b, c, d]`
> produced `[3, 4, 2]` with nothing retired, so `d` inherited `a`'s node and subtree while `a` was
> never released. That makes declaring an `id` a weaker guarantee than declaring none, which
> inverts the point of the feature.
>
> `list` and its `key` are still unbuilt, and not because they were skipped. `list` is registered as
> a Lua constructor in `NODE_KINDS` but rejected by `layout::scene::ensure_supported_kind`, so a
> config using it never reaches reconciliation at all. `key` has nothing to attach to until `list`
> exists as a real node kind, which is its own slice.

ADR-0023 § 4 matches a freshly evaluated node to its retained counterpart by position among its
parent's children. That is correct only while the child order never changes. Insert one node above
an existing sibling and every node below it shifts by one, so each reconciles against the wrong
retained node: leases transfer to the wrong subtree, and once ADR-0044's named state exists, state
follows the wrong node too.

Quickshell hit this and answered it twice. `reload.hpp` gives every `Reloadable` an optional
`reloadableId`, and `getChildByReloadId` stops descending the moment it meets another `Reloadable`
because "that's a separate reload scope". `ScriptModel` answers the list version, comparing items by
a named property rather than by index, because a raw expression used as a model "will destroy all
created delegates, and re-create the entire list" on any change.

Both are the same question at two scales: what makes this node the same node as last time.

## Decision 1: any node may carry an `id`, scoped to its parent

`id` becomes a base property in § 5.1, available on every node kind rather than required only on
top-level surfaces. It is a hint for reconciliation and nothing else: it does not have to be unique
across the tree, is not addressable from Lua, and has no effect on layout or paint.

Scoping is per parent, not global. Two sibling nodes may not share an `id`; a node in a different
parent may reuse it freely. This is what makes a reusable component composable, since a table
returned by a `require`d module can carry internal ids and still be instantiated twice
(ADR-0047).

A duplicate `id` among siblings is a `LayoutError` routed to rescue, not a warning. Quickshell warns
and ignores duplicates in `Variants`; Oblisk already validates the whole tree on every resolve, so
rejecting is both cheaper and deterministic.

## Decision 2: identified children pair first, then the rest pair by position

Within one parent, match every fresh child that has an `id` to the retained child with the same
`id`. Then match the remaining fresh children to the remaining retained children by their order
among themselves, which is exactly ADR-0023's existing rule applied to a smaller set.

This degrades to today's behavior when a config uses no ids at all, so nothing regresses and no
config is forced to annotate anything. It also means ids can be applied to exactly the nodes that
need stability, which is the small number holding a lease or named state, rather than to every node
in the tree.

## Decision 3: `list` takes a `key` function, and a duplicate key is an error

`key` is a Lua function from a `source` element to a string, called on the element rather than on
the node `itemfn` builds, so a key is computable without building anything. Items reconcile by key.

```lua
list {
    source = oblisk.tray.items,
    key = function(item) return item.id end,
    itemfn = function(item) return text { content = item.title } end,
}
```

One shape, not two. Quickshell's `objectProp` is a property name, which avoids running script per
item but cannot key a list of plain strings. A function covers both, and thirty calls per resolve is
not a cost worth a second spelling for.

Without `key`, items match by index and every item below an insertion rebuilds. That is a documented
cost rather than a rejected configuration, since a static list of five buttons has no reason to
carry keys.

Duplicate keys are an error. `ScriptModel`'s docs say behavior with duplicates is undefined; a config
error reported through rescue beats undefined behavior in a shell that is meant to stay up.

## Consequences

`list` is still deferred (ADR-0023 item 1, restated by ADR-0044), and this ADR does not un-defer it.
It settles the one thing that has to be right on the first attempt, because a `list` that ships
without keyed reconciliation teaches configs to rely on index stability.

Top-level surface ids are unchanged. They are already required, already unique, and already key
`Scene`'s `HashMap`. Surfaces are the root scope, so decision 1's per-parent rule starts one level
down.

`oblisk-idl-api-specs.md` § 5.1 gains `id`, and § 5.2's `list` gains `key`.
