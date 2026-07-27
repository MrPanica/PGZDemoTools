use std::{collections::BTreeSet, env, fs};

use bitbuffer::{BitRead, BitWrite, BitWriteStream, LittleEndian};
use main_error::MainError;
use tf_demo_parser::demo::data::userinfo::{PlayerInfo, UserInfo};
use tf_demo_parser::demo::header::Header;
use tf_demo_parser::demo::message::packetentities::{EntityId, PacketEntity, UpdateType};
use tf_demo_parser::demo::message::setconvar::ConVar;
use tf_demo_parser::demo::message::usermessage::UserMessage;
use tf_demo_parser::demo::message::Message;
use tf_demo_parser::demo::packet::stringtable::StringTableEntry;
use tf_demo_parser::demo::packet::{Packet, PacketType};
use tf_demo_parser::demo::parser::{DemoHandler, Encode, RawPacketStream};
use tf_demo_parser::Demo;

#[derive(Clone, Copy, Eq, PartialEq)]
enum SpectatorMode {
    SourceTv,
    Replay,
}

#[derive(Default)]
struct UnlockState {
    spectator_added: bool,
    spectator_userinfo_added: bool,
    transmit_all_added: bool,
}

fn collect_userinfo(
    index: u16,
    entry: &StringTableEntry<'_>,
    entities: &mut BTreeSet<u32>,
    user_ids: &mut BTreeSet<u16>,
) -> Result<(), MainError> {
    if let Some(user) = UserInfo::parse_from_string_table(
        index,
        entry.text.as_deref(),
        entry.extra_data.as_ref().map(|data| data.data.clone()),
    )? {
        entities.insert(user.entity_id.into());
        user_ids.insert(user.player_info.user_id.into());
    }
    Ok(())
}

fn find_spectator(input: &[u8]) -> Result<(EntityId, u16, u8), MainError> {
    let demo = Demo::new(input);
    let mut stream = demo.get_stream();
    let header = Header::read(&mut stream)?;
    let mut packets = RawPacketStream::new(stream);
    let mut handler = DemoHandler::default();
    handler.handle_header(&header);
    let mut max_players = None;
    let mut occupied_entities = BTreeSet::new();
    let mut occupied_user_ids = BTreeSet::new();

    while let Some(packet) = packets.next(&handler.state_handler)? {
        if let Packet::StringTables(packet) = &packet {
            for table in &packet.tables {
                if table.name == "userinfo" {
                    for (index, entry) in &table.entries {
                        collect_userinfo(
                            *index,
                            entry,
                            &mut occupied_entities,
                            &mut occupied_user_ids,
                        )?;
                    }
                }
            }
        }
        if let Packet::Signon(message_packet) | Packet::Message(message_packet) = &packet {
            for message in &message_packet.messages {
                match message {
                    Message::ServerInfo(info) => max_players = Some(info.max_player_count),
                    Message::PacketEntities(entities) => {
                        if let Some(max_players) = max_players {
                            occupied_entities.extend(
                                entities
                                    .entities
                                    .iter()
                                    .map(|entity| u32::from(entity.entity_index))
                                    .filter(|entity| *entity <= u32::from(max_players)),
                            );
                        }
                    }
                    Message::CreateStringTable(message) if message.table.name == "userinfo" => {
                        for (index, entry) in &message.table.entries {
                            collect_userinfo(
                                *index,
                                entry,
                                &mut occupied_entities,
                                &mut occupied_user_ids,
                            )?;
                        }
                    }
                    Message::UpdateStringTable(message)
                        if handler
                            .string_table_names
                            .get(message.table_id as usize)
                            .is_some_and(|name| name == "userinfo") =>
                    {
                        for (index, entry) in &message.entries {
                            collect_userinfo(
                                *index,
                                entry,
                                &mut occupied_entities,
                                &mut occupied_user_ids,
                            )?;
                        }
                    }
                    _ => {}
                }
            }
        }
        handler.handle_packet(packet)?;
    }

    let max_players = max_players.ok_or("demo has no server info")?;
    let entity = (1..=u32::from(max_players))
        .rev()
        .find(|entity| !occupied_entities.contains(entity))
        .ok_or("no free spectator slot")?;
    let user_id = (1..=u16::MAX)
        .find(|user_id| !occupied_user_ids.contains(user_id))
        .ok_or("no free spectator user id")?;
    eprintln!("using spectator slot {entity}/{max_players}, user id {user_id}");
    Ok((entity.into(), user_id, max_players))
}

fn unlock_packet(
    packet: &mut Packet<'_>,
    mode: SpectatorMode,
    spectator: EntityId,
    spectator_user_id: u16,
    max_players: u8,
    state: &mut UnlockState,
) -> Result<(), MainError> {
    let is_message = matches!(packet, Packet::Message(_));
    let message_packet = match packet {
        Packet::Signon(packet) | Packet::Message(packet) => packet,
        _ => return Ok(()),
    };

    for message in &mut message_packet.messages {
        if let Message::ServerInfo(info) = message {
            let entity_number = u32::from(spectator);
            if entity_number == 0 || entity_number > u32::from(info.max_player_count) {
                return Err("no free spectator slot".into());
            }
            info.stv = mode == SpectatorMode::SourceTv;
            info.replay = mode == SpectatorMode::Replay;
            info.player_slot = entity_number as u8 - 1;
        }
    }

    message_packet.messages.retain(|message| {
        !matches!(
            message,
            Message::SetView(_) | Message::UserMessage(UserMessage::VGuiMenu(_))
        )
    });

    for message in &mut message_packet.messages {
        let Message::SetConVar(message) = message else {
            continue;
        };
        let mut found = false;
        for var in &mut message.vars {
            if var.key == "tv_transmitall" {
                var.value = "1".to_owned();
                found = true;
            }
        }
        if !state.transmit_all_added {
            if !found {
                message.vars.push(ConVar {
                    key: "tv_transmitall".to_owned(),
                    value: "1".to_owned(),
                });
            }
            message.length = message.vars.len().try_into()?;
            state.transmit_all_added = true;
        }
    }

    if !state.spectator_userinfo_added {
        for message in &mut message_packet.messages {
            let Message::CreateStringTable(message) = message else {
                continue;
            };
            if message.table.name != "userinfo" {
                continue;
            }
            let slot = u32::from(spectator) - 1;
            if slot >= u32::from(message.table.max_entries) {
                return Err("no free userinfo slot".into());
            }
            let mut entry = UserInfo {
                entity_id: spectator,
                player_info: PlayerInfo {
                    name: if mode == SpectatorMode::Replay {
                        "Replay"
                    } else {
                        "SourceTV"
                    }
                    .to_owned(),
                    user_id: spectator_user_id.into(),
                    steam_id: "BOT".to_owned(),
                    is_fake_player: 1,
                    is_hl_tv: u8::from(mode == SpectatorMode::SourceTv),
                    is_replay: u8::from(mode == SpectatorMode::Replay),
                    ..Default::default()
                },
            }
            .encode_to_string_table()?;
            entry.text = Some(slot.to_string().into());
            message.table.entries.push((slot as u16, entry));
            message.table.entries.sort_by_key(|(index, _)| *index);
            state.spectator_userinfo_added = true;
            break;
        }
    }

    if is_message {
        message_packet.meta.view_angles = Default::default();
    }

    for message in &mut message_packet.messages {
        let Message::PacketEntities(entities) = message else {
            continue;
        };
        if entities.delta.is_some() {
            continue;
        }
        if entities
            .entities
            .iter()
            .any(|entity| entity.entity_index == spectator)
        {
            return Err("free spectator slot is occupied in entity checkpoint".into());
        }
        let player_class = entities
            .entities
            .iter()
            .find(|entity| {
                let index = u32::from(entity.entity_index);
                (1..=u32::from(max_players)).contains(&index)
                    && entity.update_type == UpdateType::Enter
            })
            .map(|entity| entity.server_class)
            .ok_or("failed to find player server class")?;
        entities.entities.push(PacketEntity {
            server_class: player_class,
            entity_index: spectator,
            props: vec![],
            in_pvs: false,
            update_type: UpdateType::Enter,
            serial_number: 1,
            delay: None,
            delta: None,
            baseline_index: entities.base_line,
        });
        entities
            .entities
            .sort_by_key(|entity| u32::from(entity.entity_index));
        state.spectator_added = true;
        break;
    }

    Ok(())
}

fn encode_packet(
    packet: &Packet<'_>,
    output: &mut Vec<u8>,
    handler: &DemoHandler<'_, tf_demo_parser::demo::parser::handler::NullHandler>,
) -> Result<(), MainError> {
    if matches!(packet, Packet::Stop(_)) {
        output.push(PacketType::Stop as u8);
        output.extend_from_slice(&u32::from(packet.tick()).to_le_bytes());
        return Ok(());
    }
    let mut encoded = Vec::new();
    {
        let mut stream = BitWriteStream::new(&mut encoded, LittleEndian);
        packet.encode(&mut stream, &handler.state_handler)?;
    }
    output.extend_from_slice(&encoded);
    Ok(())
}

fn validate_output(
    input: &[u8],
    mode: SpectatorMode,
    spectator: EntityId,
) -> Result<(), MainError> {
    let demo = Demo::new(input);
    let mut stream = demo.get_stream();
    let header = Header::read(&mut stream)?;
    let mut packets = RawPacketStream::new(stream);
    let mut handler = DemoHandler::default();
    handler.handle_header(&header);
    let mut server_info_valid = false;
    let mut spectator_found = false;
    let mut spectator_userinfo_found = false;
    let mut transmit_all_found = false;
    let mut complete_stop = false;
    let mut full_snapshots = 0usize;
    let mut spectator_snapshots = 0usize;

    while let Some(packet) = packets.next(&handler.state_handler)? {
        complete_stop |= matches!(packet, Packet::Stop(_));
        if matches!(
            packet.packet_type(),
            PacketType::ConsoleCmd | PacketType::UserCmd
        ) {
            return Err("generated demo still contains POV commands".into());
        }
        if let Packet::Signon(message_packet) | Packet::Message(message_packet) = &packet {
            for message in &message_packet.messages {
                match message {
                    Message::ServerInfo(info) => {
                        let entity = u32::from(spectator);
                        server_info_valid = info.stv == (mode == SpectatorMode::SourceTv)
                            && info.replay == (mode == SpectatorMode::Replay)
                            && u32::from(info.player_slot) + 1 == entity
                            && u32::from(info.max_player_count) >= entity;
                    }
                    Message::PacketEntities(entities) => {
                        let contains_spectator = entities.entities.iter().any(|entity| {
                            entity.entity_index == spectator
                                && entity.update_type == UpdateType::Enter
                        });
                        spectator_found |= contains_spectator;
                        if entities.delta.is_none() {
                            full_snapshots += 1;
                            spectator_snapshots += usize::from(contains_spectator);
                        }
                    }
                    Message::CreateStringTable(message) if message.table.name == "userinfo" => {
                        for (index, entry) in &message.table.entries {
                            let Some(user) = UserInfo::parse_from_string_table(
                                *index,
                                entry.text.as_deref(),
                                entry.extra_data.as_ref().map(|data| data.data.clone()),
                            )?
                            else {
                                continue;
                            };
                            spectator_userinfo_found |= user.entity_id == spectator
                                && user.player_info.steam_id == "BOT"
                                && (user.player_info.is_hl_tv != 0)
                                    == (mode == SpectatorMode::SourceTv)
                                && (user.player_info.is_replay != 0)
                                    == (mode == SpectatorMode::Replay);
                        }
                    }
                    Message::SetConVar(message) => {
                        for var in &message.vars {
                            if var.key == "tv_transmitall" {
                                if var.value != "1" {
                                    return Err("generated demo locks the roaming camera".into());
                                }
                                transmit_all_found = true;
                            }
                        }
                    }
                    Message::SetView(_) | Message::UserMessage(UserMessage::VGuiMenu(_)) => {
                        return Err("generated demo still contains a locked camera message".into());
                    }
                    _ => {}
                }
            }
        }
        handler.handle_packet(packet)?;
    }

    if !complete_stop {
        return Err("generated demo has no complete stop".into());
    }
    if full_snapshots == 0 || spectator_snapshots != full_snapshots {
        return Err("generated demo loses the spectator at an entity checkpoint".into());
    }
    if !server_info_valid || !spectator_found || !spectator_userinfo_found || !transmit_all_found {
        return Err("generated demo has no valid spectator slot".into());
    }
    Ok(())
}

fn unlock_pov_spectator(input: &[u8], mode: SpectatorMode) -> Result<Vec<u8>, MainError> {
    let (spectator, spectator_user_id, max_players) = find_spectator(input)?;
    let demo = Demo::new(input);
    let mut stream = demo.get_stream();
    let mut header = Header::read(&mut stream)?;
    let signon_end = stream.pos() + header.signon as usize * 8;
    let mut packets = RawPacketStream::new(stream);
    let mut source = DemoHandler::default();
    let mut output_handler = DemoHandler::default();
    source.handle_header(&header);
    output_handler.handle_header(&header);

    let mut signon = Vec::new();
    let mut body = Vec::new();
    let mut state = UnlockState::default();
    let mut complete_stop = false;

    while let Some(packet) = packets.next(&source.state_handler)? {
        let in_signon = packets.pos() <= signon_end;
        let mut output_packet = packet.clone();
        unlock_packet(
            &mut output_packet,
            mode,
            spectator,
            spectator_user_id,
            max_players,
            &mut state,
        )?;

        if !matches!(
            output_packet.packet_type(),
            PacketType::ConsoleCmd | PacketType::UserCmd
        ) {
            complete_stop |= matches!(output_packet, Packet::Stop(_));
            encode_packet(
                &output_packet,
                if in_signon { &mut signon } else { &mut body },
                &output_handler,
            )?;
            output_handler.handle_packet(output_packet)?;
        }
        source.handle_packet(packet)?;
    }
    if !complete_stop {
        body.push(PacketType::Stop as u8);
        body.extend_from_slice(&header.ticks.to_le_bytes());
    }

    if !state.spectator_added {
        return Err("failed to add spectator entity".into());
    }
    if !state.spectator_userinfo_added {
        return Err("failed to add spectator userinfo".into());
    }
    if !state.transmit_all_added {
        return Err("failed to unlock spectator PVS".into());
    }
    header.signon = signon.len().try_into()?;

    let mut output = Vec::with_capacity(input.len());
    {
        let mut stream = BitWriteStream::new(&mut output, LittleEndian);
        header.write(&mut stream)?;
    }
    output.extend_from_slice(&signon);
    output.extend_from_slice(&body);
    validate_output(&output, mode, spectator)?;
    Ok(output)
}

pub fn unlock_pov_freecam(input: &[u8]) -> Result<Vec<u8>, MainError> {
    unlock_pov_spectator(input, SpectatorMode::SourceTv)
}

pub fn convert_pov_replay(input: &[u8]) -> Result<Vec<u8>, MainError> {
    unlock_pov_spectator(input, SpectatorMode::Replay)
}

fn main() -> Result<(), MainError> {
    let args: Vec<_> = env::args().skip(1).collect();
    if !(args.len() == 2 || args.len() == 3 && args[2] == "--replay") {
        eprintln!("usage: pov_unlock_freecam <input.dem> <output.dem> [--replay]");
        std::process::exit(2);
    }

    let input = fs::read(&args[0])?;
    let output = if args.len() == 3 {
        convert_pov_replay(&input)?
    } else {
        unlock_pov_freecam(&input)?
    };
    fs::write(&args[1], output)?;
    Ok(())
}
