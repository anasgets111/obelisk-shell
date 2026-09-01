# `exclusive` is three answers, not a boolean

§ 6.1 types `exclusive` as `boolean`: "Reserves physical screen area for bar if true". That is two
answers for a protocol field with three, and the missing one is the only one a wallpaper can use.

`zwlr_layer_surface_v1::set_exclusive_zone` takes a signed number with three distinct meanings. A
positive zone reserves that much along the anchored edge. `0` reserves nothing **and still positions
the surface inside the area every other surface reserved**. `-1` reserves nothing and ignores what
everyone else reserved, so the surface covers the output.

The boolean reached the first two and nothing reached the third.

## What made it visible

`dev-config`'s wallpaper is a `panel` on `Background`, anchored to all four edges, `exclusive =
false`. Running the shell live: at startup the wallpaper resolved against the whole 1920x1200
output. The moment the bar mapped and claimed its 39px, niri reconfigured the wallpaper to 1920x1161
and it sat *below* the bar rather than behind it. Changing `theme.bar_height` and watching it reload
moved the wallpaper again, to 1140. A wallpaper is the one surface whose size must not depend on
what any other surface reserved.

Nothing in the config could fix that, and the obvious edit would not have either. `true` reads as
`Reserve`, and `exclusive_zone_for` answers `0` for a surface anchored to all four edges, because
there is no single edge to reserve against and the protocol's own wording only defines the strip
cases. So on exactly the surface that needed a third answer, `true` and `false` were the same
request.

## Decision 1: `exclusive` accepts `boolean` or `"Ignore"`

```lua
exclusive = true       -- reserve along the anchored edge, sized from the surface
exclusive = false      -- reserve nothing, stay inside what others reserved   (default)
exclusive = "Ignore"   -- reserve nothing, ignore what others reserved, cover the output
```

Parsed into `node::Exclusive { Reserve, Respect, Ignore }`, one variant per protocol case, and
`apply_exclusive_zone` maps them to the derived zone, `0` and `-1`.

**Additive, not a migration.** `true` and `false` keep exactly the meanings they had, so no config
written before this changes behaviour. The one edit in this repo is `dev-config`'s wallpaper, which
is the surface that was wrong.

**`boolean / string` rather than a pure enum**, because that is already this IDL's shape for a
scalar with one special case: § 6.1 types `width`/`height` as `integer / string` and
`parse_size_mode` takes a number, `"Fill"`, or `"NN%"`. Spelling it `"Auto" | "None" | "Ignore"`
would read more uniformly and would break every existing config to buy that.

**Named for what each does**, not for the number it sends. `Reserve` and `Respect` are the two
non-ignoring answers and the pair is what makes `0` legible: a surface reserving nothing is still
respecting everyone else, which is precisely the thing the wallpaper did not want and the boolean
could not distinguish.

## Decision 2: the deferred placeholder stays `Respect`

`exclusive` is not a structural property (`is_structural_property` covers only `layer`, `anchor`,
`monitor` and `namespace` on a `panel`), so a `Signal` in it resolves normally at layout time. But it
is read twice: `socket::surface_specs` reads the raw map before any getter has run, where
`is_deferred_signal` leaves the parser at its placeholder, and `App::apply_resolved_state` re-reads
the authoritative value off the resolved tree.

That placeholder must be `Respect`, and the reason is now worth stating rather than inheriting. It is
the only one of the three that is invisible for the frame it lasts. `Ignore` would paint a wallpaper
over the bar until the resolved pass corrected it. `Reserve` would shove every window aside. Doing
nothing is the only answer that looks like nothing.

## Decision 3: an unknown string fails the pass

`exclusive = "ignore"` and `exclusive = "None"` are refused by name rather than read as `Respect`.
Both are shapes a config author reaches for, and a silent fallback here is the quiet miss
`NODE_PROPERTIES` exists to prevent one level up: before that list, `aling_v = "Center"` was accepted
and read by nobody. A value typo inside a known key deserves the same treatment as a key typo.

## What this does not decide

**A numeric zone.** Quickshell's `WlrLayershell` carries `exclusionMode` *and* an integer
`exclusiveZone`, and setting the integer flips the mode to `Normal` implicitly. Nothing here has
asked to reserve a custom amount, and the implicit mode flip is the half of that design worth not
copying. `exclusive_zone_for`'s derivation stays the only way a positive zone is produced.

**Renaming to match Quickshell.** Their `ExclusionMode` is `Auto` / `Normal` / `Ignore`, and the
mapping is exact: `Auto` is `Reserve`, `Normal` with a zone of 0 is `Respect`, `Ignore` is `Ignore`.
Their names describe how the number was arrived at; these describe what the surface asked for. Only
`Ignore` is shared, deliberately, because its meaning is the protocol's rather than either
implementation's.

**The IDL version.** Cargo.toml's rule says minor for an IDL field added or changed, and this does
not bump it. The rule has not started yet: nothing has been pushed to origin, so 0.1.0 is a
placeholder rather than a released number and there is nobody downstream for a minor to inform.
ADR-0069 added `scroll` to § 5.1 under the same conditions and left it alone. The first push is what
turns the rule on; from then a change like this one is a minor.
