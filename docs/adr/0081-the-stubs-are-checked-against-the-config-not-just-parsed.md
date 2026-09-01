# The stubs are checked against the config, not just parsed

`just check` runs `lua-language-server --check` over `dev-config/oblisk` and `share/starter`.
Amends `lua-meta/nodes.lua`'s header, which argued that spelling `Signal` in every union would
drown the useful types.

## The stubs had no consumer

`lua-meta` had three guards. `just lua` runs `luac -p`, which proves a file parses.
`renderer/src/lua/nodes.rs`'s `meta_stub_tests` prove the *roster* agrees: every node kind is
declared, every kind's `---@field` names match `accepted_properties`, and every property some parser
reads is accepted by some kind. `supervisor/src/stubs.rs` proves the generated half matches the
payload types it was generated from.

All three check names. None of them ever used a declared *type* for anything. The types were
written for a reader, and nothing read them.

That is the gap this closes. `.luarc.json` already existed in `dev-config/oblisk` and already
pointed at `lua-meta`, so the language server was doing this check in an editor and throwing the
answer away.

## Which made a wrong type free to write

The header said every property takes a `Signal` whether or not the union says so, and named the
union-spelling as a readability trade. Measured, with a generated probe binding a `Signal` to all
192 kind/property pairs:

| | |
| --- | --- |
| properties that reject a `Signal` the engine accepts | 28 |
| of those, callbacks where the union is true but useless | 7 |
| fixed here | 21 |

`radius`, `spacing`, `font_size`, `align_h`, `align_v`, `clip`, `elide`, `fit`, `size`,
`text_align`, `direction`, `exclusive`, `keyboard_interactivity`, `app_id`, `parent`, `grab`,
`gravity`, `placeholder`, `mask_character`, and a `popup`'s `width`/`height` all now spell
`|Signal`.

The trade was not readability against pedantry. `lua-meta` is what the language server reads, so an
omitted union member is a red squiggle under working config code, and the cost was paid by whoever
wrote that code rather than by whoever read the stub.

## What still names no Signal, and why

Two sets, both correct as they are.

`id`, `hover`, `scroll`, and a `panel`'s `layer`/`anchor`/`monitor`/`namespace` are structural:
`node::resolve_properties` copies them through raw rather than resolving, because they are
identities and an identity does not resolve. The engine really does refuse a `Signal` there.

`on_click`, `on_change`, `on_submit`, `on_close`, `on_dismiss`, `itemfn` and `key` resolve like any
other property and are then refused for not being a function. Declaring `fun(...)|Signal` would be
true and would only make completion worse.

## Why the hand-written half is not generated

The obvious follow-up is to generate `nodes.lua` and `surfaces.lua` too, and stop having two kinds
of stub. Measured, that does not do what it sounds like it does.

The *names* are already data: `NODE_KINDS`, `COMMON_PROPERTIES`, `BOX_PROPERTIES` and
`NODE_PROPERTIES` in `renderer/src/lua/nodes.rs` are exactly the roster a generator would iterate.
The types and the prose are not. They live in 49 `parse_*` functions and 45 `properties.get("...")`
call sites across ten files, each with its own defaulting and coercion.

So a generator would need a table of `(property, Lua type, description)` in Rust, and that table
would be a hand-written claim in a `.rs` file instead of a hand-written claim in a `.lua` file. The
prose moves; nothing becomes derived. The only spelling that would genuinely derive is a per-kind
props struct the parsers read fields off, the way a capability payload is a `Serialize` struct.
That is a rewrite of the parse layer, and it trades this crate's property-by-property error
messages (`radius: expected a number, got String("x")`) for serde's.

What was missing was not generation, it was a check. `every_type_the_stubs_declare_is_accepted_by_the_engine`
builds `kind { property = <sample of the declared type> }` and runs a real `Scene::apply`, which is
what calls all 49 parsers. All 408 declared type members, no skips: a type with no sample fails the
test rather than passing quietly. Verified by injecting `font_size? Color|integer|Signal`, which it
names.

That closes the direction generation would have closed, without moving a word of prose.

## Two directions, two tools

The engine test and `just types` check opposite claims and neither subsumes the other.

| | catches | how |
| --- | --- | --- |
| `every_type_the_stubs_declare_is_accepted_by_the_engine` | the stub promises what the engine refuses | feeds each declared type to `Scene::apply` |
| `just types` | the stub omits what the engine accepts | checks real config code against the declarations |

The engine test cannot do the second direction. Trying every *undeclared* sample and asserting a
refusal collapses on aliases: `width` is `Length|Signal` and accepts a bare `integer`, `content` is
`string` and accepts a `Color` sample because a hex colour is a string. Both would report as
findings and both are correct code. Structural checking against real config is the right tool for
that half, which is why both exist.

## What the check is worth

`dev-config` came back with six diagnostics, all in `components/icon_button.lua`, and none of them
a stub bug: three locals hold either a colour string or a `Signal`, chosen by a runtime
`type(x) == "userdata"` test, and there are no user-defined type guards in LuaCATS. Four `---@type`
and five `---@cast` annotations, in the one place in the config that needs them.

The check earns its place on the 174 generated payload fields rather than on the node properties.
`b.percentt` in a `:map` callback is now a build failure naming the field, where before it was a
`nil` that a config rendered as a blank pill.

Optional in `check`, because `lua-language-server` is not a build dependency of this workspace and
there is no CI to install it into. Absent means skipped with a line saying so, never a silent pass.

## `lua-meta` is checked as itself, not only as a library

The first version checked `dev-config` and `share/starter` and loaded `lua-meta` as their
`workspace.library`. A library's own diagnostics are suppressed, so the stubs were the one thing the
stub checker could not see.

That hid a real fault the day it was written. `---@return T a, b` declares *two* returns, so a comma
in a single return's prose makes the next word a type: `---@return Signal Read-only, like `map`.`
declared a return of type `like`. Checking the config said nothing, correctly, because the config
was fine.

So `lua-meta` is now checked as its own workspace too, with no library of its own: these files
declare everything they reference, which is what makes that possible. Single-return prose is written
`---@return T # ...`, where `#` is LuaCATS' explicit "the rest is a comment" marker and the whole
class goes away.
