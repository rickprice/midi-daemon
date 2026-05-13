use mlua::prelude::*;

/// A MIDI message passed between Rust and Lua as a table.
/// Fields: type, channel, note, velocity, controller, value, data (raw bytes)
pub fn midi_bytes_to_lua(lua: &Lua, bytes: &[u8]) -> LuaResult<LuaTable> {
    let msg = lua.create_table()?;

    match bytes {
        // Note On
        [status, note, vel] if (0x90..=0x9F).contains(status) && *vel > 0 => {
            msg.set("type", "note_on")?;
            msg.set("channel", (status & 0x0F) + 1)?;
            msg.set("note", *note)?;
            msg.set("velocity", *vel)?;
        }
        // Note Off (or Note On with vel=0)
        [status, note, vel]
            if (0x80..=0x8F).contains(status)
                || ((0x90..=0x9F).contains(status) && *vel == 0) =>
        {
            msg.set("type", "note_off")?;
            msg.set("channel", (status & 0x0F) + 1)?;
            msg.set("note", *note)?;
            msg.set("velocity", *vel)?;
        }
        // Control Change
        [status, cc, val] if (0xB0..=0xBF).contains(status) => {
            msg.set("type", "cc")?;
            msg.set("channel", (status & 0x0F) + 1)?;
            msg.set("controller", *cc)?;
            msg.set("value", *val)?;
        }
        // Program Change
        [status, prog] if (0xC0..=0xCF).contains(status) => {
            msg.set("type", "program_change")?;
            msg.set("channel", (status & 0x0F) + 1)?;
            msg.set("program", *prog)?;
        }
        // Pitch Bend
        [status, lsb, msb] if (0xE0..=0xEF).contains(status) => {
            let value = (((*msb as i16) << 7) | (*lsb as i16)) - 8192;
            msg.set("type", "pitch_bend")?;
            msg.set("channel", (status & 0x0F) + 1)?;
            msg.set("value", value)?;
        }
        // MIDI Clock
        [0xF8] => {
            msg.set("type", "clock")?;
        }
        // Start
        [0xFA] => {
            msg.set("type", "start")?;
        }
        // Stop
        [0xFC] => {
            msg.set("type", "stop")?;
        }
        // Continue
        [0xFB] => {
            msg.set("type", "continue")?;
        }
        // Fallback: raw bytes
        _ => {
            msg.set("type", "raw")?;
            let data = lua.create_table()?;
            for (i, b) in bytes.iter().enumerate() {
                data.set(i + 1, *b)?;
            }
            msg.set("data", data)?;
        }
    }

    Ok(msg)
}

/// Convert a Lua table back to raw MIDI bytes for sending.
pub fn lua_to_midi_bytes(msg: &LuaTable) -> LuaResult<Vec<u8>> {
    let msg_type: String = msg.get("type")?;

    match msg_type.as_str() {
        "note_on" => {
            let ch: u8 = msg.get::<u8>("channel")?.saturating_sub(1) & 0x0F;
            let note: u8 = msg.get("note")?;
            let vel: u8 = msg.get("velocity")?;
            Ok(vec![0x90 | ch, note, vel])
        }
        "note_off" => {
            let ch: u8 = msg.get::<u8>("channel")?.saturating_sub(1) & 0x0F;
            let note: u8 = msg.get("note")?;
            let vel: u8 = msg.get::<Option<u8>>("velocity")?.unwrap_or(0);
            Ok(vec![0x80 | ch, note, vel])
        }
        "cc" => {
            let ch: u8 = msg.get::<u8>("channel")?.saturating_sub(1) & 0x0F;
            let cc: u8 = msg.get("controller")?;
            let val: u8 = msg.get("value")?;
            Ok(vec![0xB0 | ch, cc, val])
        }
        "program_change" => {
            let ch: u8 = msg.get::<u8>("channel")?.saturating_sub(1) & 0x0F;
            let prog: u8 = msg.get("program")?;
            Ok(vec![0xC0 | ch, prog])
        }
        "pitch_bend" => {
            let ch: u8 = msg.get::<u8>("channel")?.saturating_sub(1) & 0x0F;
            let value: i16 = msg.get("value")?;
            let v = (value + 8192).clamp(0, 16383) as u16;
            let lsb = (v & 0x7F) as u8;
            let msb = ((v >> 7) & 0x7F) as u8;
            Ok(vec![0xE0 | ch, lsb, msb])
        }
        "clock" => Ok(vec![0xF8]),
        "start" => Ok(vec![0xFA]),
        "stop" => Ok(vec![0xFC]),
        "continue" => Ok(vec![0xFB]),
        "raw" => {
            let data: LuaTable = msg.get("data")?;
            let mut bytes = Vec::new();
            for i in 1..=data.len()? {
                bytes.push(data.get::<u8>(i)?);
            }
            Ok(bytes)
        }
        other => Err(LuaError::RuntimeError(format!(
            "Unknown MIDI message type: {}",
            other
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua() -> Lua {
        Lua::new()
    }

    fn str_field(t: &LuaTable, k: &str) -> String {
        t.get::<String>(k).unwrap()
    }
    fn u8_field(t: &LuaTable, k: &str) -> u8 {
        t.get::<u8>(k).unwrap()
    }
    fn i16_field(t: &LuaTable, k: &str) -> i16 {
        t.get::<i16>(k).unwrap()
    }

    // ── midi_bytes_to_lua ────────────────────────────────────────────────────

    #[test]
    fn parse_note_on() {
        let lua = lua();
        let msg = midi_bytes_to_lua(&lua, &[0x90, 60, 100]).unwrap();
        assert_eq!(str_field(&msg, "type"), "note_on");
        assert_eq!(u8_field(&msg, "channel"), 1);
        assert_eq!(u8_field(&msg, "note"), 60);
        assert_eq!(u8_field(&msg, "velocity"), 100);
    }

    #[test]
    fn parse_note_on_channel_16() {
        let lua = lua();
        let msg = midi_bytes_to_lua(&lua, &[0x9F, 48, 64]).unwrap();
        assert_eq!(str_field(&msg, "type"), "note_on");
        assert_eq!(u8_field(&msg, "channel"), 16);
        assert_eq!(u8_field(&msg, "note"), 48);
    }

    #[test]
    fn parse_note_off_via_8x_status() {
        let lua = lua();
        let msg = midi_bytes_to_lua(&lua, &[0x83, 60, 0]).unwrap();
        assert_eq!(str_field(&msg, "type"), "note_off");
        assert_eq!(u8_field(&msg, "channel"), 4);
        assert_eq!(u8_field(&msg, "note"), 60);
        assert_eq!(u8_field(&msg, "velocity"), 0);
    }

    #[test]
    fn parse_note_off_via_note_on_vel0() {
        // Note On with velocity 0 must be treated as Note Off
        let lua = lua();
        let msg = midi_bytes_to_lua(&lua, &[0x90, 60, 0]).unwrap();
        assert_eq!(str_field(&msg, "type"), "note_off");
        assert_eq!(u8_field(&msg, "channel"), 1);
        assert_eq!(u8_field(&msg, "note"), 60);
    }

    #[test]
    fn parse_cc() {
        let lua = lua();
        let msg = midi_bytes_to_lua(&lua, &[0xB2, 7, 100]).unwrap();
        assert_eq!(str_field(&msg, "type"), "cc");
        assert_eq!(u8_field(&msg, "channel"), 3);
        assert_eq!(u8_field(&msg, "controller"), 7);
        assert_eq!(u8_field(&msg, "value"), 100);
    }

    #[test]
    fn parse_cc_channel_bounds() {
        let lua = lua();
        let msg = midi_bytes_to_lua(&lua, &[0xBF, 64, 127]).unwrap();
        assert_eq!(str_field(&msg, "type"), "cc");
        assert_eq!(u8_field(&msg, "channel"), 16);
    }

    #[test]
    fn parse_program_change() {
        let lua = lua();
        let msg = midi_bytes_to_lua(&lua, &[0xC0, 42]).unwrap();
        assert_eq!(str_field(&msg, "type"), "program_change");
        assert_eq!(u8_field(&msg, "channel"), 1);
        assert_eq!(u8_field(&msg, "program"), 42);
    }

    #[test]
    fn parse_pitch_bend_center() {
        // 14-bit center = 8192 (0x2000): lsb=0x00, msb=0x40 → decoded value = 0
        let lua = lua();
        let msg = midi_bytes_to_lua(&lua, &[0xE0, 0x00, 0x40]).unwrap();
        assert_eq!(str_field(&msg, "type"), "pitch_bend");
        assert_eq!(u8_field(&msg, "channel"), 1);
        assert_eq!(i16_field(&msg, "value"), 0);
    }

    #[test]
    fn parse_pitch_bend_min() {
        // 14-bit 0: lsb=0x00, msb=0x00 → decoded value = -8192
        let lua = lua();
        let msg = midi_bytes_to_lua(&lua, &[0xE0, 0x00, 0x00]).unwrap();
        assert_eq!(i16_field(&msg, "value"), -8192);
    }

    #[test]
    fn parse_pitch_bend_max() {
        // 14-bit max = 16383 (0x3FFF): lsb=0x7F, msb=0x7F → decoded value = 8191
        let lua = lua();
        let msg = midi_bytes_to_lua(&lua, &[0xE0, 0x7F, 0x7F]).unwrap();
        assert_eq!(i16_field(&msg, "value"), 8191);
    }

    #[test]
    fn parse_clock() {
        let lua = lua();
        let msg = midi_bytes_to_lua(&lua, &[0xF8]).unwrap();
        assert_eq!(str_field(&msg, "type"), "clock");
    }

    #[test]
    fn parse_start() {
        let lua = lua();
        let msg = midi_bytes_to_lua(&lua, &[0xFA]).unwrap();
        assert_eq!(str_field(&msg, "type"), "start");
    }

    #[test]
    fn parse_stop() {
        let lua = lua();
        let msg = midi_bytes_to_lua(&lua, &[0xFC]).unwrap();
        assert_eq!(str_field(&msg, "type"), "stop");
    }

    #[test]
    fn parse_continue() {
        let lua = lua();
        let msg = midi_bytes_to_lua(&lua, &[0xFB]).unwrap();
        assert_eq!(str_field(&msg, "type"), "continue");
    }

    #[test]
    fn parse_raw_sysex_fallback() {
        let lua = lua();
        let msg = midi_bytes_to_lua(&lua, &[0xF0, 0x41, 0xF7]).unwrap();
        assert_eq!(str_field(&msg, "type"), "raw");
        let data: LuaTable = msg.get("data").unwrap();
        assert_eq!(data.get::<u8>(1).unwrap(), 0xF0);
        assert_eq!(data.get::<u8>(2).unwrap(), 0x41);
        assert_eq!(data.get::<u8>(3).unwrap(), 0xF7);
    }

    #[test]
    fn parse_raw_empty() {
        let lua = lua();
        // Empty bytes have no pattern match, fall through to raw
        let msg = midi_bytes_to_lua(&lua, &[]).unwrap();
        assert_eq!(str_field(&msg, "type"), "raw");
        let data: LuaTable = msg.get("data").unwrap();
        assert_eq!(data.len().unwrap(), 0);
    }

    // ── lua_to_midi_bytes ────────────────────────────────────────────────────

    #[test]
    fn encode_note_on() {
        let lua = lua();
        let t = lua.create_table().unwrap();
        t.set("type", "note_on").unwrap();
        t.set("channel", 1u8).unwrap();
        t.set("note", 60u8).unwrap();
        t.set("velocity", 100u8).unwrap();
        assert_eq!(lua_to_midi_bytes(&t).unwrap(), vec![0x90, 60, 100]);
    }

    #[test]
    fn encode_note_on_channel_16() {
        let lua = lua();
        let t = lua.create_table().unwrap();
        t.set("type", "note_on").unwrap();
        t.set("channel", 16u8).unwrap();
        t.set("note", 48u8).unwrap();
        t.set("velocity", 64u8).unwrap();
        assert_eq!(lua_to_midi_bytes(&t).unwrap(), vec![0x9F, 48, 64]);
    }

    #[test]
    fn encode_note_off() {
        let lua = lua();
        let t = lua.create_table().unwrap();
        t.set("type", "note_off").unwrap();
        t.set("channel", 1u8).unwrap();
        t.set("note", 60u8).unwrap();
        t.set("velocity", 0u8).unwrap();
        assert_eq!(lua_to_midi_bytes(&t).unwrap(), vec![0x80, 60, 0]);
    }

    #[test]
    fn encode_note_off_optional_velocity() {
        // velocity field absent → defaults to 0
        let lua = lua();
        let t = lua.create_table().unwrap();
        t.set("type", "note_off").unwrap();
        t.set("channel", 1u8).unwrap();
        t.set("note", 60u8).unwrap();
        let bytes = lua_to_midi_bytes(&t).unwrap();
        assert_eq!(bytes[0], 0x80);
        assert_eq!(bytes[2], 0);
    }

    #[test]
    fn encode_cc() {
        let lua = lua();
        let t = lua.create_table().unwrap();
        t.set("type", "cc").unwrap();
        t.set("channel", 2u8).unwrap();
        t.set("controller", 7u8).unwrap();
        t.set("value", 100u8).unwrap();
        assert_eq!(lua_to_midi_bytes(&t).unwrap(), vec![0xB1, 7, 100]);
    }

    #[test]
    fn encode_program_change() {
        let lua = lua();
        let t = lua.create_table().unwrap();
        t.set("type", "program_change").unwrap();
        t.set("channel", 1u8).unwrap();
        t.set("program", 42u8).unwrap();
        assert_eq!(lua_to_midi_bytes(&t).unwrap(), vec![0xC0, 42]);
    }

    #[test]
    fn encode_pitch_bend_center() {
        let lua = lua();
        let t = lua.create_table().unwrap();
        t.set("type", "pitch_bend").unwrap();
        t.set("channel", 1u8).unwrap();
        t.set("value", 0i16).unwrap();
        assert_eq!(lua_to_midi_bytes(&t).unwrap(), vec![0xE0, 0x00, 0x40]);
    }

    #[test]
    fn encode_pitch_bend_min() {
        let lua = lua();
        let t = lua.create_table().unwrap();
        t.set("type", "pitch_bend").unwrap();
        t.set("channel", 1u8).unwrap();
        t.set("value", -8192i16).unwrap();
        assert_eq!(lua_to_midi_bytes(&t).unwrap(), vec![0xE0, 0x00, 0x00]);
    }

    #[test]
    fn encode_pitch_bend_max() {
        let lua = lua();
        let t = lua.create_table().unwrap();
        t.set("type", "pitch_bend").unwrap();
        t.set("channel", 1u8).unwrap();
        t.set("value", 8191i16).unwrap();
        assert_eq!(lua_to_midi_bytes(&t).unwrap(), vec![0xE0, 0x7F, 0x7F]);
    }

    #[test]
    fn encode_clock() {
        let lua = lua();
        let t = lua.create_table().unwrap();
        t.set("type", "clock").unwrap();
        assert_eq!(lua_to_midi_bytes(&t).unwrap(), vec![0xF8]);
    }

    #[test]
    fn encode_start() {
        let lua = lua();
        let t = lua.create_table().unwrap();
        t.set("type", "start").unwrap();
        assert_eq!(lua_to_midi_bytes(&t).unwrap(), vec![0xFA]);
    }

    #[test]
    fn encode_stop() {
        let lua = lua();
        let t = lua.create_table().unwrap();
        t.set("type", "stop").unwrap();
        assert_eq!(lua_to_midi_bytes(&t).unwrap(), vec![0xFC]);
    }

    #[test]
    fn encode_continue() {
        let lua = lua();
        let t = lua.create_table().unwrap();
        t.set("type", "continue").unwrap();
        assert_eq!(lua_to_midi_bytes(&t).unwrap(), vec![0xFB]);
    }

    #[test]
    fn encode_raw() {
        let lua = lua();
        let data = lua.create_table().unwrap();
        data.set(1, 0xF0u8).unwrap();
        data.set(2, 0x41u8).unwrap();
        data.set(3, 0xF7u8).unwrap();
        let t = lua.create_table().unwrap();
        t.set("type", "raw").unwrap();
        t.set("data", data).unwrap();
        assert_eq!(lua_to_midi_bytes(&t).unwrap(), vec![0xF0, 0x41, 0xF7]);
    }

    #[test]
    fn encode_unknown_type_is_error() {
        let lua = lua();
        let t = lua.create_table().unwrap();
        t.set("type", "sysex").unwrap();
        assert!(lua_to_midi_bytes(&t).is_err());
    }

    // ── Round-trip tests ─────────────────────────────────────────────────────

    fn roundtrip(input: &[u8]) -> Vec<u8> {
        let lua = lua();
        let table = midi_bytes_to_lua(&lua, input).unwrap();
        lua_to_midi_bytes(&table).unwrap()
    }

    #[test]
    fn roundtrip_note_on() {
        assert_eq!(roundtrip(&[0x90, 60, 100]), vec![0x90, 60, 100]);
    }

    #[test]
    fn roundtrip_note_on_all_channels() {
        for ch in 0u8..16 {
            let input = [0x90 | ch, 64, 80];
            assert_eq!(roundtrip(&input), input.to_vec(), "channel {}", ch + 1);
        }
    }

    #[test]
    fn roundtrip_note_off() {
        assert_eq!(roundtrip(&[0x80, 60, 0]), vec![0x80, 60, 0]);
    }

    #[test]
    fn roundtrip_cc() {
        assert_eq!(roundtrip(&[0xB3, 7, 100]), vec![0xB3, 7, 100]);
    }

    #[test]
    fn roundtrip_program_change() {
        assert_eq!(roundtrip(&[0xC5, 42]), vec![0xC5, 42]);
    }

    #[test]
    fn roundtrip_pitch_bend_center() {
        assert_eq!(roundtrip(&[0xE0, 0x00, 0x40]), vec![0xE0, 0x00, 0x40]);
    }

    #[test]
    fn roundtrip_pitch_bend_min() {
        assert_eq!(roundtrip(&[0xE0, 0x00, 0x00]), vec![0xE0, 0x00, 0x00]);
    }

    #[test]
    fn roundtrip_pitch_bend_max() {
        assert_eq!(roundtrip(&[0xE0, 0x7F, 0x7F]), vec![0xE0, 0x7F, 0x7F]);
    }

    #[test]
    fn roundtrip_clock() {
        assert_eq!(roundtrip(&[0xF8]), vec![0xF8]);
    }

    #[test]
    fn roundtrip_start_stop_continue() {
        assert_eq!(roundtrip(&[0xFA]), vec![0xFA]);
        assert_eq!(roundtrip(&[0xFC]), vec![0xFC]);
        assert_eq!(roundtrip(&[0xFB]), vec![0xFB]);
    }
}
