use bitbuffer::{BitRead, BitReadBuffer, BitReadStream, BitWrite, BitWriteStream, LittleEndian};
use main_error::MainError;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::{env, fs};
use tf_demo_parser::demo::data::ServerTick;
use tf_demo_parser::demo::header::Header;
use tf_demo_parser::demo::message::packetentities::{
    BaselineIndex, EntityId, PacketEntitiesMessage, PacketEntity, UpdateType,
};
use tf_demo_parser::demo::message::{Message, MessageType};
use tf_demo_parser::demo::packet::synctick::SyncTickPacket;
use tf_demo_parser::demo::packet::usercmd::UserCmd;
use tf_demo_parser::demo::packet::{Packet, PacketType};
use tf_demo_parser::demo::parser::{DemoHandler, Encode, Parse, RawPacketStream};
use tf_demo_parser::Demo;

struct HistoryPacket<'a> {
    tick: u32,
    packet: Packet<'a>,
    entities: Option<EntitySnapshot>,
    user_cmd: Option<UserCmd>,
}

type EntitySnapshot = BTreeMap<EntityId, Arc<PacketEntity>>;

fn merge_user_cmd(state: &mut Option<UserCmd>, update: &UserCmd) {
    let Some(current) = state.as_mut() else {
        *state = Some(update.clone());
        return;
    };
    macro_rules! merge {
        ($field:ident) => {
            if update.$field.is_some() {
                current.$field = update.$field.clone();
            }
        };
    }
    merge!(command_number);
    merge!(tick_count);
    merge!(buttons);
    merge!(impulse);
    merge!(weapon_select);
    merge!(mouse_dx);
    merge!(mouse_dy);
    for index in 0..3 {
        if update.view_angles[index].is_some() {
            current.view_angles[index] = update.view_angles[index];
        }
        if update.movement[index].is_some() {
            current.movement[index] = update.movement[index];
        }
    }
}

fn string_table_update<'a>(packet: &Packet<'a>) -> Option<Packet<'a>> {
    if matches!(packet, Packet::StringTables(_)) {
        return Some(packet.clone());
    }
    let Packet::Message(message_packet) = packet else {
        return None;
    };
    let mut update = message_packet.clone();
    update.messages.retain(|message| {
        matches!(
            message,
            Message::CreateStringTable(_) | Message::UpdateStringTable(_)
        )
    });
    (!update.messages.is_empty()).then_some(Packet::Message(update))
}

fn observe_entities(
    packet: &Packet<'_>,
    state: &tf_demo_parser::ParserState,
    snapshots: &mut BTreeMap<u32, EntitySnapshot>,
) -> Result<Option<EntitySnapshot>, MainError> {
    let (Packet::Message(packet) | Packet::Signon(packet)) = packet else {
        return Ok(None);
    };
    let server_tick = packet.messages.iter().find_map(|message| match message {
        Message::NetTick(message) => Some(u32::from(message.tick)),
        _ => None,
    });
    let mut result = None;
    for message in &packet.messages {
        let Message::PacketEntities(update) = message else {
            continue;
        };
        let tick = server_tick.ok_or("packet entities has no server tick")?;
        let mut entities = match update.delta {
            Some(delta) => snapshots
                .get(&u32::from(delta))
                .cloned()
                .ok_or("packet entities delta snapshot is missing")?,
            None => EntitySnapshot::new(),
        };
        for entity in &update.entities {
            match entity.update_type {
                UpdateType::Enter => {
                    let mut full = entity.clone();
                    full.props = entity.props(state).collect();
                    full.props.sort_by_key(|prop| prop.index);
                    full.delta = None;
                    entities.insert(entity.entity_index, Arc::new(full));
                }
                UpdateType::Preserve => {
                    if let Some(current) = entities.get_mut(&entity.entity_index) {
                        let current = Arc::make_mut(current);
                        current.apply_update(&entity.props);
                        current.props.sort_by_key(|prop| prop.index);
                        current.in_pvs = true;
                    }
                }
                UpdateType::Leave => {
                    if let Some(current) = entities.get_mut(&entity.entity_index) {
                        Arc::make_mut(current).in_pvs = false;
                    }
                }
                UpdateType::Delete => {
                    entities.remove(&entity.entity_index);
                }
            }
        }
        for removed in &update.removed_entities {
            entities.remove(removed);
        }
        snapshots.insert(tick, entities.clone());
        while snapshots.len() > 128 {
            let oldest = *snapshots
                .keys()
                .next()
                .expect("snapshot cache is not empty");
            snapshots.remove(&oldest);
        }
        result = Some(entities);
    }
    Ok(result)
}

fn set_server_tick(packet: &mut Packet<'_>, tick: ServerTick) {
    let (Packet::Message(packet) | Packet::Signon(packet)) = packet else {
        return;
    };
    for message in &mut packet.messages {
        if let Message::NetTick(message) = message {
            message.tick = tick;
        }
    }
}

fn continue_packet_sequence(packet: &mut Packet<'_>, next: &mut Option<u32>) {
    let (Packet::Message(message) | Packet::Signon(message)) = packet else {
        return;
    };
    let sequence = *next.get_or_insert(message.meta.sequence_in);
    message.meta.sequence_in = sequence;
    message.meta.sequence_out = sequence;
    *next = Some(sequence.wrapping_add(1));
}

fn replace_entity_snapshot(
    packet: &mut Packet<'_>,
    current: &EntitySnapshot,
    previous: &mut Option<EntitySnapshot>,
    previous_tick: Option<ServerTick>,
    current_tick: Option<ServerTick>,
) -> bool {
    let (Packet::Message(packet) | Packet::Signon(packet)) = packet else {
        return false;
    };
    for message in &mut packet.messages {
        let Message::PacketEntities(update) = message else {
            continue;
        };
        let source_updates = update
            .entities
            .iter()
            .map(|entity| {
                (
                    entity.entity_index,
                    (
                        entity.update_type,
                        entity
                            .props
                            .iter()
                            .map(|prop| prop.identifier)
                            .collect::<BTreeSet<_>>(),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let source_removed = update
            .removed_entities
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        // TF2 rejects a PacketEntities delta that names its own server tick.
        // A second network update can legitimately arrive on the same tick;
        // write that one as a complete snapshot instead.
        let full_reset =
            previous.is_none() || previous_tick.is_none() || previous_tick == current_tick;
        let mut entities = Vec::new();
        for current_entity in current.values().filter(|entity| entity.in_pvs) {
            let mut entity;
            if !full_reset {
                if let Some(old) = previous
                    .as_ref()
                    .and_then(|snapshot| snapshot.get(&current_entity.entity_index))
                {
                    if old.server_class == current_entity.server_class
                        && old.serial_number == current_entity.serial_number
                    {
                        let forced_props = source_updates
                            .get(&current_entity.entity_index)
                            .map(|(_, props)| props);
                        let props = current_entity
                            .props
                            .iter()
                            .filter(|prop| {
                                forced_props.is_some_and(|props| props.contains(&prop.identifier))
                                    || old
                                        .props
                                        .binary_search_by_key(&prop.index, |old_prop| {
                                            old_prop.index
                                        })
                                        .map_or(true, |index| old.props[index] != **prop)
                            })
                            .cloned()
                            .collect::<Vec<_>>();
                        if props.is_empty()
                            && !source_updates.contains_key(&current_entity.entity_index)
                        {
                            continue;
                        }
                        entity = (**current_entity).clone();
                        entity.props = props;
                        entity.update_type = UpdateType::Preserve;
                    } else {
                        entity = (**current_entity).clone();
                        entity.update_type = UpdateType::Enter;
                    }
                } else {
                    entity = (**current_entity).clone();
                    entity.update_type = UpdateType::Enter;
                }
            } else {
                entity = (**current_entity).clone();
                entity.update_type = UpdateType::Enter;
            }
            entity.in_pvs = true;
            entity.delta = previous_tick;
            entities.push(entity);
        }
        if !full_reset {
            if let Some(old) = previous.as_ref() {
                entities.extend(
                    old.iter()
                        .filter(|(entity, _)| {
                            !source_removed.contains(entity)
                                && current.get(entity).is_none_or(|entity| !entity.in_pvs)
                        })
                        .map(|(id, entity)| {
                            let mut entity = (**entity).clone();
                            entity.update_type = source_updates
                                .get(id)
                                .map_or(UpdateType::Leave, |(update_type, _)| *update_type);
                            entity.props.clear();
                            entity.delta = previous_tick;
                            entity
                        }),
                );
                entities.sort_by_key(|entity| entity.entity_index);
            }
        }
        let snapshot = previous.get_or_insert_with(BTreeMap::new);
        for entity in &entities {
            if let Some(current_entity) = current
                .get(&entity.entity_index)
                .filter(|entity| entity.in_pvs)
            {
                snapshot.insert(entity.entity_index, current_entity.clone());
            } else {
                snapshot.remove(&entity.entity_index);
            }
        }
        for entity in &source_removed {
            snapshot.remove(entity);
        }
        *update = PacketEntitiesMessage {
            entities,
            removed_entities: if full_reset {
                Vec::new()
            } else {
                source_removed
                    .into_iter()
                    .filter(|entity| {
                        previous
                            .as_ref()
                            .is_some_and(|old| old.contains_key(entity))
                    })
                    .collect()
            },
            max_entries: update.max_entries,
            delta: if full_reset { None } else { previous_tick },
            base_line: if full_reset {
                BaselineIndex::First
            } else {
                update.base_line
            },
            updated_base_line: false,
        };
        return true;
    }
    false
}

fn encode_packet(
    packet: &Packet<'_>,
    output: &mut Vec<u8>,
    handler: &DemoHandler<'_, tf_demo_parser::demo::parser::handler::NullHandler>,
) -> Result<(), MainError> {
    let mut encoded = Vec::new();
    {
        let mut stream = BitWriteStream::new(&mut encoded, LittleEndian);
        packet.encode(&mut stream, &handler.state_handler)?;
    }
    output.extend_from_slice(&encoded);
    Ok(())
}

fn validate_demo(input: &[u8]) -> Result<(), MainError> {
    let demo = Demo::new(input);
    let mut stream = demo.get_stream();
    let header = Header::read(&mut stream)?;
    let mut handler = DemoHandler::default();
    handler.handle_header(&header);
    let mut packet_number = 0usize;
    loop {
        let packet_start = stream.pos();
        let command = input[packet_start / 8];
        let packet = Packet::parse(&mut stream, &handler.state_handler).map_err(|error| {
            format!(
                "generated demo packet {packet_number} command {command} at bit {packet_start}: {error}"
            )
        })?;
        packet_number += 1;
        let stop = matches!(packet, Packet::Stop(_));
        handler.handle_packet(packet)?;
        if stop {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_cmd_checkpoint_materializes_previous_values() {
        let mut state = Some(UserCmd {
            command_number: Some(10),
            tick_count: Some(20),
            view_angles: [Some(1.0), Some(2.0), None],
            movement: [None; 3],
            buttons: Some(1),
            impulse: None,
            weapon_select: None,
            mouse_dx: None,
            mouse_dy: None,
        });
        merge_user_cmd(
            &mut state,
            &UserCmd {
                command_number: Some(11),
                tick_count: None,
                view_angles: [None, Some(3.0), None],
                movement: [None; 3],
                buttons: Some(0),
                impulse: None,
                weapon_select: None,
                mouse_dx: None,
                mouse_dy: None,
            },
        );
        let state = state.unwrap();
        assert_eq!(state.command_number, Some(11));
        assert_eq!(state.tick_count, Some(20));
        assert_eq!(state.view_angles, [Some(1.0), Some(3.0), None]);
        assert_eq!(state.buttons, Some(0));
    }

    #[test]
    fn packet_sequences_remain_monotonic_after_a_reverse_cut() {
        let mut next = Some(20_805);
        let mut first = Packet::Message(Default::default());
        let mut second = Packet::Message(Default::default());
        continue_packet_sequence(&mut first, &mut next);
        continue_packet_sequence(&mut second, &mut next);
        let Packet::Message(first) = first else {
            unreachable!()
        };
        let Packet::Message(second) = second else {
            unreachable!()
        };
        assert_eq!(
            (first.meta.sequence_in, first.meta.sequence_out),
            (20_805, 20_805)
        );
        assert_eq!(
            (second.meta.sequence_in, second.meta.sequence_out),
            (20_806, 20_806)
        );
    }

    #[test]
    fn raw_signon_history_continues_the_packet_sequence() {
        let mut raw = vec![0; 89];
        let mut next = 42;
        rewrite_raw_sequence(&mut raw, &mut next);
        assert_eq!(u32::from_le_bytes(raw[81..85].try_into().unwrap()), 42);
        assert_eq!(u32::from_le_bytes(raw[85..89].try_into().unwrap()), 42);
        assert_eq!(next, 43);
    }

    #[test]
    fn raw_tick_rewrite_keeps_unaligned_bits_intact() {
        let mut bytes = vec![0b1010_0101; 6];
        write_le_bits(&mut bytes, 3, 0x1234_5678).unwrap();
        let value = (0..32).fold(0u32, |value, bit| {
            value | (u32::from((bytes[(3 + bit) / 8] >> ((3 + bit) % 8)) & 1) << bit)
        });
        assert_eq!(value, 0x1234_5678);
        assert_eq!(bytes[0] & 0b111, 0b101);
    }
}

fn cut_source_montage(
    input: &[u8],
    output_path: &str,
    ranges: &[(u32, u32)],
) -> Result<(), MainError> {
    if ranges.is_empty() || ranges.iter().any(|(start, end)| start >= end) {
        return Err("each montage range must have a positive length".into());
    }

    let demo = Demo::new(input);
    let mut header_stream = demo.get_stream();
    let mut header = Header::read(&mut header_stream)?;
    if ranges.iter().any(|(_, end)| *end > header.ticks) {
        return Err("montage range exceeds demo duration".into());
    }
    let header_bytes = header_stream.pos() / 8;
    let signon_end_bits = header_stream.pos() + header.signon as usize * 8;
    let signon_end_bytes = header_bytes + header.signon as usize;
    if signon_end_bytes > input.len() {
        return Err("invalid demo sign-on boundary".into());
    }

    let tick_rate = header.ticks as f32 / header.duration;
    let total_ticks = ranges.iter().map(|(start, end)| end - start).sum::<u32>();
    let mut output_handler = DemoHandler::default();
    output_handler.handle_header(&header);
    let mut body = Vec::new();
    encode_packet(
        &Packet::SyncTick(SyncTickPacket { tick: 0u32.into() }),
        &mut body,
        &output_handler,
    )?;

    let mut cursor = 0u32;
    let mut frames = 0u32;
    let mut output_signon_written = false;
    let mut next_output_sequence = None;
    let mut extra_signon = Vec::new();
    // The output remains one continuous netchannel.  Keeping this snapshot
    // across ranges makes the first packet of the next range a real delta
    // from the final state of the previous range instead of a second sign-on.
    let mut previous_output_entities = None;
    let mut previous_output_tick = None;

    // Each requested range is read from a fresh source stream.  That is required
    // for reverse order: SourceTV deltas may only be decoded in source order.
    for &(start, end) in ranges {
        let demo = Demo::new(input);
        let mut stream = demo.get_stream();
        let source_header = Header::read(&mut stream)?;
        let mut packets = RawPacketStream::new(stream);
        let mut source = DemoHandler::default();
        source.handle_header(&source_header);
        let mut entities = EntitySnapshot::new();
        let mut source_snapshots = BTreeMap::new();
        let mut table_updates: Vec<Packet<'_>> = Vec::new();
        let mut checkpoint_written = false;

        loop {
            let before = packets.pos();
            let Some(packet) = packets.next(&source.state_handler)? else {
                break;
            };
            let after = packets.pos();
            if after <= signon_end_bits {
                if let Some(snapshot) =
                    observe_entities(&packet, &source.state_handler, &mut source_snapshots)?
                {
                    entities = snapshot;
                }
                source.handle_packet(packet.clone())?;
                if !output_signon_written {
                    output_handler.handle_packet(packet)?;
                }
                continue;
            }

            let tick = u32::from(packet.tick());
            if tick >= end {
                break;
            }
            let selected = start <= tick && packet.packet_type() != PacketType::Stop;
            if let Some(snapshot) =
                observe_entities(&packet, &source.state_handler, &mut source_snapshots)?
            {
                entities = snapshot;
            }
            let has_entities = matches!(&packet, Packet::Message(message) | Packet::Signon(message)
                if message.messages.iter().any(|message| matches!(message, Message::PacketEntities(_))));

            if !selected || (!checkpoint_written && !has_entities) {
                if !output_signon_written && packet.packet_type() == PacketType::Message {
                    let mut raw = input[before / 8..after / 8].to_vec();
                    raw[0] = PacketType::Signon as u8;
                    extra_signon.extend_from_slice(&raw);
                    output_handler.handle_packet(packet.clone())?;
                }
                if let Some(update) = string_table_update(&packet) {
                    table_updates.push(update);
                }
                source.handle_packet(packet)?;
                continue;
            }

            if !checkpoint_written {
                for mut update in table_updates.drain(..) {
                    update.set_tick(cursor.into());
                    set_server_tick(&mut update, cursor.into());
                    continue_packet_sequence(&mut update, &mut next_output_sequence);
                    if update.packet_type() == PacketType::Message {
                        frames += 1;
                    }
                    encode_packet(&update, &mut body, &output_handler)?;
                    output_handler.handle_packet(update)?;
                }
                checkpoint_written = true;
            }

            if packet.packet_type() != PacketType::SyncTick
                && (start != 0 || packet.packet_type() != PacketType::StringTables)
            {
                let output_tick = cursor + tick - start;
                let mut output_packet = packet.clone();
                output_packet.set_tick(output_tick.into());
                if replace_entity_snapshot(
                    &mut output_packet,
                    &entities,
                    &mut previous_output_entities,
                    previous_output_tick,
                    Some(output_tick.into()),
                ) {
                    previous_output_tick = Some(output_tick.into());
                }
                set_server_tick(&mut output_packet, output_tick.into());
                continue_packet_sequence(&mut output_packet, &mut next_output_sequence);
                if output_packet.packet_type() == PacketType::Message {
                    frames += 1;
                }
                encode_packet(&output_packet, &mut body, &output_handler)?;
                output_handler.handle_packet(output_packet)?;
            }
            source.handle_packet(packet)?;
        }
        if !checkpoint_written {
            return Err("montage range has no packet-entity checkpoint".into());
        }
        output_signon_written = true;
        cursor += end - start;
    }

    body.push(PacketType::Stop as u8);
    body.extend_from_slice(&total_ticks.to_le_bytes());
    header.ticks = total_ticks;
    header.duration = total_ticks as f32 / tick_rate;
    header.frames = frames;
    header.signon += extra_signon.len() as u32;

    let signon = &input[header_bytes..signon_end_bytes];
    let mut output =
        Vec::with_capacity(header_bytes + signon.len() + extra_signon.len() + body.len());
    {
        let mut output_stream = BitWriteStream::new(&mut output, LittleEndian);
        header.write(&mut output_stream)?;
    }
    output.extend_from_slice(signon);
    output.extend_from_slice(&extra_signon);
    output.extend_from_slice(&body);
    fs::write(output_path, output)?;
    eprintln!(
        "wrote {} SourceTV frames across {} ranges",
        frames,
        ranges.len()
    );
    Ok(())
}

// SourceTV packets contain message types that this parser intentionally skips.
// Keep them raw and replay each range's original history at its boundary so
// PacketEntities deltas retain the frames they reference.
fn append_raw_packet(
    input: &[u8],
    output: &mut Vec<u8>,
    source_start: usize,
    source_end: usize,
    tick: u32,
    kind: PacketType,
    state: &tf_demo_parser::ParserState,
    server_tick_map: Option<(u32, u32)>,
    next_sequence: &mut u32,
    frames: &mut u32,
) -> Result<(), MainError> {
    let mut raw = input[source_start..source_end].to_vec();
    raw[1..5].copy_from_slice(&tick.to_le_bytes());
    if matches!(kind, PacketType::Message) {
        if let Some((source_origin, output_origin)) = server_tick_map {
            rewrite_raw_server_ticks(&mut raw, state, source_origin, output_origin)?;
        }
        rewrite_raw_sequence(&mut raw, next_sequence);
        *frames = frames.saturating_add(1);
    }
    output.extend_from_slice(&raw);
    Ok(())
}

fn rewrite_raw_sequence(raw: &mut [u8], next_sequence: &mut u32) {
    raw[81..85].copy_from_slice(&next_sequence.to_le_bytes());
    raw[85..89].copy_from_slice(&next_sequence.to_le_bytes());
    *next_sequence = next_sequence.wrapping_add(1);
}

// A raw Message packet starts with its command/tick/meta (93 bytes), followed
// by a bit stream. Keep that stream intact, but retime the two server-tick
// references that make a PacketEntities delta chain continuous across cuts.
fn rewrite_raw_server_ticks(
    raw: &mut [u8],
    state: &tf_demo_parser::ParserState,
    source_origin: u32,
    output_origin: u32,
) -> Result<(), MainError> {
    const MESSAGE_DATA_OFFSET: usize = 93;
    if raw.len() < MESSAGE_DATA_OFFSET {
        return Err("raw message packet is too short".into());
    }
    let length = u32::from_le_bytes(raw[89..93].try_into()?) as usize;
    let end = MESSAGE_DATA_OFFSET
        .checked_add(length)
        .ok_or("raw message packet length overflows")?;
    if end > raw.len() {
        return Err("raw message packet data is truncated".into());
    }

    let mut stream = BitReadStream::new(BitReadBuffer::new(
        &raw[MESSAGE_DATA_OFFSET..end],
        LittleEndian,
    ));
    let mut replacements = Vec::new();
    while stream.bits_left() > 6 {
        let message_type = MessageType::read(&mut stream)?;
        let payload_start = stream.pos();
        match message_type {
            MessageType::NetTick => {
                let tick: ServerTick = stream.read()?;
                replacements.push((
                    payload_start,
                    map_server_tick(u32::from(tick), source_origin, output_origin),
                ));
                stream.read::<u16>()?;
                stream.read::<u16>()?;
            }
            MessageType::PacketEntities => {
                stream.read_sized::<u16>(11)?;
                let has_delta: bool = stream.read()?;
                if has_delta {
                    let delta_start = stream.pos();
                    let delta: ServerTick = stream.read()?;
                    replacements.push((
                        delta_start,
                        map_server_tick(u32::from(delta), source_origin, output_origin),
                    ));
                }
                stream.set_pos(payload_start)?;
                Message::skip_type(message_type, &mut stream, state)?;
            }
            _ => Message::skip_type(message_type, &mut stream, state)?,
        }
    }
    drop(stream);
    for (bit_offset, value) in replacements {
        write_le_bits(&mut raw[MESSAGE_DATA_OFFSET..end], bit_offset, value)?;
    }
    Ok(())
}

fn map_server_tick(source: u32, source_origin: u32, output_origin: u32) -> u32 {
    (i64::from(output_origin) + i64::from(source) - i64::from(source_origin)) as u32
}

fn write_le_bits(bytes: &mut [u8], bit_offset: usize, value: u32) -> Result<(), MainError> {
    if bit_offset
        .checked_add(32)
        .is_none_or(|end| end > bytes.len() * 8)
    {
        return Err("raw message tick field is truncated".into());
    }
    for bit in 0..32 {
        let index = bit_offset + bit;
        let mask = 1u8 << (index % 8);
        if value & (1 << bit) == 0 {
            bytes[index / 8] &= !mask;
        } else {
            bytes[index / 8] |= mask;
        }
    }
    Ok(())
}

fn cut_source_raw_replay(
    input: &[u8],
    output_path: &str,
    ranges: &[(u32, u32)],
) -> Result<(), MainError> {
    if ranges.is_empty() || ranges.iter().any(|(start, end)| start >= end) {
        return Err("source raw replay needs non-empty start/end tick pairs".into());
    }

    let demo = Demo::new(input);
    let mut stream = demo.get_stream();
    let header = Header::read(&mut stream)?;
    if ranges.iter().any(|(_, end)| *end > header.ticks) {
        return Err("source raw replay range exceeds demo duration".into());
    }
    let header_bytes = stream.pos() / 8;
    let mut packets = RawPacketStream::new(stream);
    let mut source = DemoHandler::default();
    source.handle_header(&header);
    let mut body_start = None;
    let mut last_signon_sequence = 0u32;

    loop {
        let start = packets.pos() / 8;
        let Some(packet) = packets.next(&source.state_handler)? else {
            break;
        };
        if body_start.is_none() {
            if let Packet::Signon(message) = &packet {
                last_signon_sequence = last_signon_sequence.max(message.meta.sequence_in);
                last_signon_sequence = last_signon_sequence.max(message.meta.sequence_out);
            }
            if matches!(packet, Packet::Message(_)) {
                body_start = Some(start);
            } else {
                source.handle_packet(packet)?;
                continue;
            }
        }
        break;
    }

    let body_start = body_start.ok_or("source demo has no normal packets")?;
    let mut body = Vec::new();
    let mut extra_signon = Vec::new();
    let mut next_sequence = last_signon_sequence.wrapping_add(1);
    let mut cursor = 0u32;
    let mut frames = 0u32;
    let mut timeline_server_origin = None;
    let mut next_server_tick = None;

    for (range_index, (start, end)) in ranges.iter().copied().enumerate() {
        let demo = Demo::new(input);
        let mut range_stream = demo.get_stream();
        let range_header = Header::read(&mut range_stream)?;
        let range_signon_end = range_stream.pos() + range_header.signon as usize * 8;
        let mut range_packets = RawPacketStream::new(range_stream);
        let mut range_source = DemoHandler::default();
        range_source.handle_header(&range_header);
        // Replay a range's history before its selected packets. This keeps the
        // exact SourceTV delta cache without rebuilding entity messages.
        let bootstrap_history = start > 0;
        let mut range_server_origin = None;
        let mut range_output_origin = None;

        loop {
            let raw_start = range_packets.pos() / 8;
            let Some(packet) = range_packets.next(&range_source.state_handler)? else {
                break;
            };
            let raw_end = range_packets.pos() / 8;
            if range_packets.pos() <= range_signon_end {
                range_source.handle_packet(packet)?;
                continue;
            }

            let tick = u32::from(packet.tick());
            if tick >= end || matches!(packet, Packet::Stop(_)) {
                break;
            }
            let packet_server_tick = match &packet {
                Packet::Message(message) | Packet::Signon(message) => {
                    message.messages.iter().find_map(|message| {
                        if let Message::NetTick(message) = message {
                            Some(u32::from(message.tick))
                        } else {
                            None
                        }
                    })
                }
                _ => None,
            };
            if tick < start {
                if range_index == 0
                    && bootstrap_history
                    && matches!(packet.packet_type(), PacketType::Message)
                {
                    let mut raw = input[raw_start..raw_end].to_vec();
                    raw[0] = PacketType::Signon as u8;
                    rewrite_raw_sequence(&mut raw, &mut next_sequence);
                    extra_signon.extend_from_slice(&raw);
                } else if bootstrap_history && matches!(packet.packet_type(), PacketType::Message) {
                    if let Some(server_tick) = packet_server_tick {
                        let source_origin = *range_server_origin.get_or_insert(server_tick);
                        let output_origin = *range_output_origin
                            .get_or_insert(next_server_tick.unwrap_or(server_tick));
                        let mapped = map_server_tick(server_tick, source_origin, output_origin);
                        append_raw_packet(
                            input,
                            &mut body,
                            raw_start,
                            raw_end,
                            cursor,
                            PacketType::Message,
                            &range_source.state_handler,
                            Some((source_origin, output_origin)),
                            &mut next_sequence,
                            &mut frames,
                        )?;
                        next_server_tick = Some(
                            next_server_tick
                                .unwrap_or(mapped)
                                .max(mapped)
                                .wrapping_add(1),
                        );
                    }
                }
                range_source.handle_packet(packet)?;
                continue;
            }
            if let Some(server_tick) = packet_server_tick {
                let timeline_tick = *timeline_server_origin.get_or_insert(server_tick);
                let source_origin = *range_server_origin.get_or_insert(server_tick);
                let output_origin = *range_output_origin.get_or_insert(
                    next_server_tick.unwrap_or(timeline_tick.saturating_add(cursor)),
                );
                let mapped = map_server_tick(server_tick, source_origin, output_origin);
                next_server_tick = Some(
                    next_server_tick
                        .unwrap_or(mapped)
                        .max(mapped)
                        .wrapping_add(1),
                );
            }
            let server_tick_map = range_server_origin.zip(range_output_origin);

            let kind = packet.packet_type();
            append_raw_packet(
                input,
                &mut body,
                raw_start,
                raw_end,
                cursor + tick - start,
                kind,
                &range_source.state_handler,
                server_tick_map,
                &mut next_sequence,
                &mut frames,
            )?;
            range_source.handle_packet(packet)?;
        }
        cursor = cursor.saturating_add(end - start);
    }
    body.push(PacketType::Stop as u8);
    body.extend_from_slice(&cursor.to_le_bytes());

    let tick_rate = header.ticks as f32 / header.duration;
    let signon_end = header_bytes + header.signon as usize;
    let mut output_header = header;
    output_header.duration = cursor as f32 / tick_rate;
    output_header.ticks = cursor;
    output_header.frames = frames;
    output_header.signon = output_header
        .signon
        .saturating_add(extra_signon.len() as u32);
    let mut output = Vec::with_capacity(header_bytes + extra_signon.len() + body.len());
    {
        let mut stream = BitWriteStream::new(&mut output, LittleEndian);
        output_header.write(&mut stream)?;
    }
    // `header.signon` ends before the first sync tick.  The bootstrap packets
    // belong in that sign-on span, otherwise the header boundary lands inside
    // a packet and TF2 sees an early dem_stop.
    output.extend_from_slice(&input[header_bytes..signon_end]);
    output.extend_from_slice(&extra_signon);
    output.extend_from_slice(&input[signon_end..body_start]);
    output.extend_from_slice(&body);
    validate_demo(&output)?;
    fs::write(output_path, output)?;
    eprintln!(
        "wrote {frames} raw SourceTV frames across {} ranges",
        ranges.len()
    );
    Ok(())
}

fn main() -> Result<(), MainError> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.len() >= 5 && args[2] == "--source-raw-replay" {
        if (args.len() - 3) % 2 != 0 {
            return Err("source raw replay needs start/end tick pairs".into());
        }
        let ranges = args[3..]
            .chunks_exact(2)
            .map(|pair| Ok((pair[0].parse()?, pair[1].parse()?)))
            .collect::<Result<Vec<(u32, u32)>, MainError>>()?;
        return cut_source_raw_replay(&fs::read(&args[0])?, &args[1], &ranges);
    }
    if args.len() >= 5 && args[2] == "--montage" {
        if (args.len() - 3) % 2 != 0 {
            return Err("montage needs start/end tick pairs".into());
        }
        let ranges = args[3..]
            .chunks_exact(2)
            .map(|pair| Ok((pair[0].parse()?, pair[1].parse()?)))
            .collect::<Result<Vec<(u32, u32)>, MainError>>()?;
        return cut_source_montage(&fs::read(&args[0])?, &args[1], &ranges);
    }
    if !(4..=5).contains(&args.len()) {
        eprintln!(
            "usage: pov_cut <input.dem> <output.dem> <start-tick> <end-tick> [server-tick-offset]"
        );
        std::process::exit(2);
    }
    let input = fs::read(&args[0])?;
    let start: u32 = args[2].parse()?;
    let end: u32 = args[3].parse()?;
    let server_tick_offset: u32 = args.get(4).map_or(Ok(0), |value| value.parse())?;
    if start >= end {
        return Err("start tick must be before end tick".into());
    }

    let demo = Demo::new(&input);
    let mut stream = demo.get_stream();
    let mut header = Header::read(&mut stream)?;
    if end > header.ticks {
        return Err("end tick exceeds demo duration".into());
    }
    let header_bytes = stream.pos() / 8;
    let signon_end_bits = stream.pos() + header.signon as usize * 8;
    let signon_end_bytes = header_bytes + header.signon as usize;
    let mut packets = RawPacketStream::new(stream);
    let mut source = DemoHandler::default();
    let mut output_handler = DemoHandler::default();
    source.handle_header(&header);
    output_handler.handle_header(&header);

    let mut body = Vec::new();
    let sync = Packet::SyncTick(SyncTickPacket { tick: 0u32.into() });
    encode_packet(&sync, &mut body, &output_handler)?;

    let mut entities = EntitySnapshot::new();
    let mut source_snapshots = BTreeMap::new();
    let mut checkpoint_written = start == 0;
    let mut full_entities_written = false;
    let mut history: VecDeque<HistoryPacket<'_>> = VecDeque::new();
    let mut table_updates: Vec<(u32, Packet<'_>)> = Vec::new();
    let mut user_cmd_state = None;
    let mut previous_output_entities = None;
    let mut previous_output_tick = None;
    let mut frames = 0u32;
    let mut warmup_ticks = 0u32;
    let mut history_start = start;

    while let Some(packet) = packets.next(&source.state_handler)? {
        let after = packets.pos();
        let in_signon = after <= signon_end_bits;

        if in_signon {
            if let Some(snapshot) =
                observe_entities(&packet, &source.state_handler, &mut source_snapshots)?
            {
                entities = snapshot;
            }
            source.handle_packet(packet.clone())?;
            output_handler.handle_packet(packet)?;
            continue;
        }

        let tick = u32::from(packet.tick());
        let selected = start <= tick && tick < end && packet.packet_type() != PacketType::Stop;
        if selected && !checkpoint_written {
            let first_checkpoint = history
                .iter()
                .position(|item| item.entities.is_some())
                .ok_or("selection has no packet-entity warmup")?;
            history.drain(..first_checkpoint);
            history_start = history.front().map(|item| item.tick).unwrap_or(start);
            warmup_ticks = start - history_start;

            for (_, mut update) in table_updates
                .drain(..)
                .filter(|(source_tick, _)| *source_tick <= history_start)
            {
                update.set_tick(0u32.into());
                encode_packet(&update, &mut body, &output_handler)?;
                output_handler.handle_packet(update)?;
                frames += 1;
            }

            let mut first_user_cmd = true;
            for mut item in history.drain(..) {
                if let Some(snapshot) = item.entities.as_ref() {
                    let server_tick = (server_tick_offset + item.tick - history_start).into();
                    if !replace_entity_snapshot(
                        &mut item.packet,
                        snapshot,
                        &mut previous_output_entities,
                        previous_output_tick,
                        Some(server_tick),
                    ) {
                        return Err("invalid packet-entity warmup".into());
                    }
                    previous_output_tick = Some(server_tick);
                    full_entities_written = true;
                }
                if first_user_cmd {
                    if let (Packet::UserCmd(packet), Some(absolute)) =
                        (&mut item.packet, item.user_cmd.take())
                    {
                        packet.cmd = absolute;
                        first_user_cmd = false;
                    }
                }
                if item.packet.packet_type() == PacketType::SyncTick
                    || item.packet.packet_type() == PacketType::StringTables
                {
                    continue;
                }
                item.packet.set_tick((item.tick - history_start).into());
                set_server_tick(
                    &mut item.packet,
                    (server_tick_offset + item.tick - history_start).into(),
                );
                if item.packet.packet_type() == PacketType::Message {
                    frames += 1;
                }
                encode_packet(&item.packet, &mut body, &output_handler)?;
                output_handler.handle_packet(item.packet)?;
            }
            checkpoint_written = true;
        }

        if let Some(snapshot) =
            observe_entities(&packet, &source.state_handler, &mut source_snapshots)?
        {
            entities = snapshot;
        }

        if selected {
            if packet.packet_type() != PacketType::SyncTick
                && (start == 0 || packet.packet_type() != PacketType::StringTables)
            {
                let mut output_packet = packet.clone();
                output_packet.set_tick((tick - start + warmup_ticks).into());
                let server_tick = (server_tick_offset + tick - history_start).into();
                if replace_entity_snapshot(
                    &mut output_packet,
                    &entities,
                    &mut previous_output_entities,
                    previous_output_tick,
                    Some(server_tick),
                ) {
                    previous_output_tick = Some(server_tick);
                }
                set_server_tick(&mut output_packet, server_tick);
                if output_packet.packet_type() == PacketType::Message {
                    frames += 1;
                }
                encode_packet(&output_packet, &mut body, &output_handler)?;
                output_handler.handle_packet(output_packet)?;
            }
        } else if tick < start {
            if let Some(update) = string_table_update(&packet) {
                table_updates.push((tick, update));
            }
            let absolute_user_cmd = if let Packet::UserCmd(user_cmd) = &packet {
                merge_user_cmd(&mut user_cmd_state, &user_cmd.cmd);
                user_cmd_state.clone()
            } else {
                None
            };
            if tick >= start.saturating_sub(64)
                && packet.packet_type() != PacketType::Stop
                && packet.packet_type() != PacketType::StringTables
            {
                let has_entities = matches!(&packet, Packet::Message(message) | Packet::Signon(message)
                    if message.messages.iter().any(|message| matches!(message, Message::PacketEntities(_))));
                history.push_back(HistoryPacket {
                    tick,
                    packet: packet.clone(),
                    entities: has_entities.then(|| entities.clone()),
                    user_cmd: absolute_user_cmd,
                });
            }
        }

        source.handle_packet(packet)?;
        if tick >= end {
            break;
        }
    }

    if !checkpoint_written || (start != 0 && !full_entities_written) {
        return Err("selection has no usable packet-entity checkpoint".into());
    }
    body.push(PacketType::Stop as u8);
    body.extend_from_slice(&(end - start + warmup_ticks).to_le_bytes());

    let tick_rate = header.ticks as f32 / header.duration;
    header.ticks = end - start + warmup_ticks;
    header.duration = header.ticks as f32 / tick_rate;
    header.frames = frames;

    let signon = &input[header_bytes..signon_end_bytes];
    let mut output = Vec::with_capacity(header_bytes + signon.len() + body.len());
    {
        let mut output_stream = BitWriteStream::new(&mut output, LittleEndian);
        header.write(&mut output_stream)?;
    }
    if output.len() != header_bytes || signon_end_bytes > input.len() {
        return Err("invalid demo sign-on boundary".into());
    }
    output.extend_from_slice(signon);
    output.extend_from_slice(&body);
    fs::write(&args[1], output)?;
    eprintln!(
        "wrote {} frames with {} entities and {} warmup ticks",
        frames,
        entities.len(),
        warmup_ticks
    );
    Ok(())
}
