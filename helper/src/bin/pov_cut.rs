use bitbuffer::{BitRead, BitWrite, BitWriteStream, LittleEndian};
use main_error::MainError;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::{env, fs};
use tf_demo_parser::demo::data::ServerTick;
use tf_demo_parser::demo::header::Header;
use tf_demo_parser::demo::message::packetentities::{
    EntityId, PacketEntitiesMessage, PacketEntity, UpdateType,
};
use tf_demo_parser::demo::message::Message;
use tf_demo_parser::demo::packet::synctick::SyncTickPacket;
use tf_demo_parser::demo::packet::usercmd::UserCmd;
use tf_demo_parser::demo::packet::{Packet, PacketType};
use tf_demo_parser::demo::parser::{DemoHandler, Encode, RawPacketStream};
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
            let oldest = *snapshots.keys().next().expect("snapshot cache is not empty");
            snapshots.remove(&oldest);
        }
        result = Some(entities);
    }
    Ok(result)
}

fn packet_server_tick(packet: &Packet<'_>) -> Option<ServerTick> {
    let (Packet::Message(packet) | Packet::Signon(packet)) = packet else {
        return None;
    };
    packet.messages.iter().find_map(|message| match message {
        Message::NetTick(message) => Some(message.tick),
        _ => None,
    })
}

fn replace_entity_snapshot(
    packet: &mut Packet<'_>,
    current: &EntitySnapshot,
    previous: &mut Option<EntitySnapshot>,
    previous_tick: Option<ServerTick>,
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
        let mut entities = Vec::new();
        for current_entity in current.values().filter(|entity| entity.in_pvs) {
            let mut entity;
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
                                ||
                            old.props
                                .binary_search_by_key(&prop.index, |old_prop| old_prop.index)
                                .map_or(true, |index| old.props[index] != **prop)
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    if props.is_empty() && !source_updates.contains_key(&current_entity.entity_index) {
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
            entity.in_pvs = true;
            entity.delta = previous_tick;
            entities.push(entity);
        }
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
            removed_entities: source_removed
                .into_iter()
                .filter(|entity| previous.as_ref().is_some_and(|old| old.contains_key(entity)))
                .collect(),
            max_entries: update.max_entries,
            delta: previous_tick,
            base_line: update.base_line,
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
}

fn main() -> Result<(), MainError> {
    let mut free_camera = false;
    let args = env::args()
        .skip(1)
        .filter(|arg| {
            if arg == "--free-camera" {
                free_camera = true;
                false
            } else {
                true
            }
        })
        .collect::<Vec<_>>();
    if args.len() != 4 {
        eprintln!(
            "usage: pov_cut [--free-camera] <input.dem> <output.dem> <start-tick> <end-tick>"
        );
        std::process::exit(2);
    }
    let input = fs::read(&args[0])?;
    let start: u32 = args[2].parse()?;
    let end: u32 = args[3].parse()?;
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
            let history_start = history.front().map(|item| item.tick).unwrap_or(start);
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
                    let server_tick = packet_server_tick(&item.packet)
                        .ok_or("packet-entity warmup has no server tick")?;
                    if !replace_entity_snapshot(
                        &mut item.packet,
                        snapshot,
                        &mut previous_output_entities,
                        previous_output_tick,
                    ) {
                        return Err("invalid packet-entity warmup".into());
                    }
                    previous_output_tick = Some(server_tick);
                    full_entities_written = true;
                }
                if first_user_cmd && !free_camera {
                    if let (Packet::UserCmd(packet), Some(absolute)) =
                        (&mut item.packet, item.user_cmd.take())
                    {
                        packet.cmd = absolute;
                        first_user_cmd = false;
                    }
                }
                if item.packet.packet_type() == PacketType::SyncTick
                    || item.packet.packet_type() == PacketType::StringTables
                    || (free_camera && item.packet.packet_type() == PacketType::UserCmd)
                {
                    continue;
                }
                item.packet.set_tick((item.tick - history_start).into());
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
                && (!free_camera || packet.packet_type() != PacketType::UserCmd)
            {
                let mut output_packet = packet.clone();
                output_packet.set_tick((tick - start + warmup_ticks).into());
                if replace_entity_snapshot(
                    &mut output_packet,
                    &entities,
                    &mut previous_output_entities,
                    previous_output_tick,
                ) {
                    previous_output_tick = packet_server_tick(&output_packet)
                        .or_else(|| Some(source.server_tick));
                }
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

    let mut output = Vec::with_capacity(header_bytes + header.signon as usize + body.len());
    {
        let mut output_stream = BitWriteStream::new(&mut output, LittleEndian);
        header.write(&mut output_stream)?;
    }
    if output.len() != header_bytes || signon_end_bytes > input.len() {
        return Err("invalid demo sign-on boundary".into());
    }
    output.extend_from_slice(&input[header_bytes..signon_end_bytes]);
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
