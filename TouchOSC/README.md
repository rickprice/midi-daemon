# TouchOSC layouts

`ComplexSetup.tosc` and `VolumePanMuteControl.tosc` are built from the
`.yaml` files in this directory plus the shared scripts in `lib/`, using
[`touch-osc-utilities-rs`](https://github.com/rickprice/touch-osc-utilities-rs)'s
`tosc` CLI. **Never hand-edit the `.tosc` files directly** — that's exactly
how they get silently corrupted. See that repo's `CLAUDE.md` for why.

`lib/subscribe.lua` is the subscribe/heartbeat/disconnect script shared by
every `VolumePanMuteControl*` strip group and the `Metronome` beat/BPM
group (6 copies, byte-identical, before this split). `lib/beat_flash.lua`
is the beat-flash color script shared by the `beat` button and `BPMLabel`
(2 copies). Both `.yaml` files reference them via `{file: lib/....lua}`,
inlined by `tosc build`.

**`tosc build` doesn't persist these file references** — the resulting
`.tosc` has the script text inlined per node like any other TouchOSC file,
and a fresh `tosc dump` of it won't show `{file: ...}` again. So if you
edit this layout in the TouchOSC app itself rather than through this
workflow, the duplication comes back; re-split it the same way described
here when that happens.

## Regenerating after a `lib/*.lua` edit

```sh
cd touch-osc-utilities-rs && cargo build --release   # once
TOSC=../touch-osc-utilities-rs/target/release/tosc

$TOSC build ComplexSetup.yaml -o ComplexSetup.tosc
$TOSC build VolumePanMuteControl.yaml -o VolumePanMuteControl.tosc
$TOSC validate ComplexSetup.tosc
$TOSC validate VolumePanMuteControl.tosc
```

## Regenerating after an edit in the TouchOSC app

```sh
$TOSC dump ComplexSetup.tosc -o ComplexSetup.yaml
```

Then re-apply the `{file: lib/subscribe.lua}` / `{file: lib/beat_flash.lua}`
substitutions to whichever `script` properties now hold inline text that
matches `lib/subscribe.lua` / `lib/beat_flash.lua` byte-for-byte, and
`tosc build` as above. Verify with `tosc dump` on the rebuilt file —
diffing it against the dump you started from (minus the `script` value
representation) should show no other changes.
