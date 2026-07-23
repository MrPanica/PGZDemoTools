use main_error::MainError;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Write;
use tf_demo_parser::demo::data::DemoTick;
use tf_demo_parser::demo::gamevent::GameEvent;
use tf_demo_parser::demo::message::Message;
use tf_demo_parser::demo::message::usermessage::{ChatMessageKind, HudTextLocation, UserMessage};
use tf_demo_parser::demo::packet::stringtable::StringTableEntry;
use tf_demo_parser::demo::parser::MessageHandler;
use tf_demo_parser::{Demo, DemoParser, MessageType, ParserState};

#[derive(Default, Debug, Clone)]
struct PlayerInfo {
    name: String,
    steam_id: String,
    user_id: u16,
}

#[derive(Debug)]
struct DemoEvent {
    tick: i64,
    kind: &'static str,
    actor: String,
    target: String,
    detail: String,
}

#[derive(Default)]
struct Collector {
    players: BTreeMap<u32, PlayerInfo>,
    codec: Option<String>,
    packets: BTreeMap<u8, Vec<(i64, Vec<u8>)>>,
    events: Vec<DemoEvent>,
}

fn clean(value: impl AsRef<str>) -> String {
    value.as_ref().replace(['\t', '\r', '\n'], " ")
}

fn clean_chat(value: impl AsRef<str>) -> String {
    clean(value)
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>()
        .trim()
        .to_owned()
}

fn placeholder_chat(actor: &str, detail: &str) -> bool {
    actor == detail
        && actor.chars().count() == 1
        && actor
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_uppercase())
}

impl Collector {
    fn player(&self, user_id: u16) -> String {
        self.players
            .values()
            .find(|player| player.user_id == user_id)
            .map(|player| player.name.clone())
            .unwrap_or_else(|| format!("#{}", user_id))
    }

    fn push(
        &mut self,
        tick: DemoTick,
        kind: &'static str,
        actor: String,
        target: String,
        detail: String,
    ) {
        self.events.push(DemoEvent {
            tick: i64::from(u32::from(tick)),
            kind,
            actor: clean(actor),
            target: clean(target),
            detail: clean(detail),
        });
    }

    fn push_chat(&mut self, tick: DemoTick, actor: String, detail: String) {
        let tick = i64::from(u32::from(tick));
        let actor = clean_chat(actor);
        let detail = clean_chat(detail);
        if detail.is_empty() || placeholder_chat(&actor, &detail) {
            return;
        }
        if self.events.iter().rev().take_while(|event| event.tick == tick).any(|event| {
            event.kind == "chat" && event.actor == actor && event.detail == detail
        }) {
            return;
        }
        self.events.push(DemoEvent {
            tick,
            kind: "chat",
            actor,
            target: String::new(),
            detail,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::placeholder_chat;

    #[test]
    fn filters_parser_placeholders_but_not_chat() {
        assert!(placeholder_chat("Q", "Q"));
        assert!(placeholder_chat("C", "C"));
        assert!(!placeholder_chat("Bob", "Q"));
    }
}

impl MessageHandler for Collector {
    type Output = Collector;

    fn does_handle(message_type: MessageType) -> bool {
        matches!(
            message_type,
            MessageType::VoiceInit
                | MessageType::VoiceData
                | MessageType::GameEvent
                | MessageType::UserMessage
        )
    }

    fn handle_message(&mut self, message: &Message, tick: DemoTick, _state: &ParserState) {
        match message {
            Message::VoiceInit(msg) => self.codec = Some(format!("{:?}", msg)),
            Message::VoiceData(msg) => {
                let byte_len = msg.data.bit_len().div_ceil(8);
                let bytes = msg
                    .data
                    .clone()
                    .read_bytes(byte_len)
                    .map(|chunk| chunk.to_vec())
                    .unwrap_or_default();
                self.packets
                    .entry(msg.client)
                    .or_default()
                    .push((i64::from(u32::from(tick)), bytes));
            }
            Message::GameEvent(msg) => match &msg.event {
                GameEvent::PlayerDeath(event) => self.push(
                    tick,
                    "death",
                    if event.attacker == 0 {
                        "World".into()
                    } else {
                        self.player(event.attacker)
                    },
                    self.player(event.user_id),
                    event.weapon.to_string(),
                ),
                GameEvent::PlayerChat(event) => {
                    self.push_chat(tick, self.player(event.user_id), event.text.to_string())
                }
                GameEvent::PlayerSay(event) => {
                    self.push_chat(tick, self.player(event.user_id), event.text.to_string())
                }
                GameEvent::PartyChat(event) => {
                    self.push_chat(tick, event.steam_id.to_string(), event.text.to_string())
                }
                GameEvent::HLTVChat(event) => self.push_chat(tick, "HLTV".into(), event.text.to_string()),
                GameEvent::PlayerSpawn(event) => {
                    let class = match event.class {
                        1 => "Scout",
                        2 => "Sniper",
                        3 => "Soldier",
                        4 => "Demoman",
                        5 => "Medic",
                        6 => "Heavy",
                        7 => "Pyro",
                        8 => "Spy",
                        9 => "Engineer",
                        _ => "Unknown class",
                    };
                    let team = match event.team {
                        2 => "RED",
                        3 => "BLU",
                        _ => "Spectator",
                    };
                    self.push(
                        tick,
                        "spawn",
                        self.player(event.user_id),
                        String::new(),
                        format!("{} · {}", team, class),
                    );
                }
                GameEvent::PlayerClass(event) => self.push(
                    tick,
                    "class",
                    self.player(event.user_id),
                    String::new(),
                    event.class.to_string(),
                ),
                GameEvent::PlayerTeam(event) => self.push(
                    tick,
                    "team",
                    self.player(event.user_id),
                    String::new(),
                    match event.team {
                        2 => "RED".into(),
                        3 => "BLU".into(),
                        _ => "Spectator".into(),
                    },
                ),
                GameEvent::TeamPlayRoundStart(_) => self.push(
                    tick,
                    "round_start",
                    String::new(),
                    String::new(),
                    String::new(),
                ),
                GameEvent::TeamPlayRoundWin(event) => self.push(
                    tick,
                    "round_win",
                    String::new(),
                    String::new(),
                    match event.team {
                        2 => "RED".into(),
                        3 => "BLU".into(),
                        _ => format!("Team {}", event.team),
                    },
                ),
                _ => {}
            },
            Message::UserMessage(UserMessage::SayText2(message))
                if message.kind != ChatMessageKind::NameChange =>
            {
                self.push_chat(
                    tick,
                    message
                        .from
                        .as_ref()
                        .map(|name| name.to_string())
                        .unwrap_or_default(),
                    message.plain_text(),
                )
            }
            Message::UserMessage(UserMessage::Text(message))
                if message.location == HudTextLocation::PrintTalk =>
            {
                self.push_chat(tick, String::new(), message.plain_text())
            }
            _ => {}
        }
    }

    fn handle_string_entry(
        &mut self,
        table: &str,
        index: usize,
        entry: &StringTableEntry,
        _state: &ParserState,
    ) {
        if table != "userinfo" {
            return;
        }
        if let Some(data) = entry.extra_data.as_ref() {
            if let Ok(Some(info)) = tf_demo_parser::demo::data::UserInfo::parse_from_string_table(
                index as u16,
                entry.text.as_ref().map(|text| text.as_ref()),
                Some(data.data.clone()),
            ) {
                self.players.insert(
                    info.entity_id.into(),
                    PlayerInfo {
                        name: info.player_info.name.clone(),
                        steam_id: info.player_info.steam_id.clone(),
                        user_id: info.player_info.user_id.into(),
                    },
                );
            }
        }
    }

    fn into_output(self, _state: &ParserState) -> Self::Output {
        self
    }
}

fn main() -> Result<(), MainError> {
    let args: Vec<_> = env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: voice_extract <demo.dem> <output_dir>");
        std::process::exit(1);
    }
    let out_dir = &args[2];
    fs::create_dir_all(out_dir)?;

    let file = fs::read(&args[1])?;
    let parser = DemoParser::new_with_analyser(Demo::new(&file).get_stream(), Collector::default());
    let (_header, result) = parser.parse()?;

    let mut players_file = fs::File::create(format!("{}/players.tsv", out_dir))?;
    writeln!(
        players_file,
        "entity_id\tclient_id\tname\tsteamid\tpacket_count\tfirst_tick\tlast_tick"
    )?;
    for (client_id, packets) in &result.packets {
        let entity_id = *client_id as u32 + 1;
        let player = result.players.get(&entity_id);
        writeln!(
            players_file,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            entity_id,
            client_id,
            clean(player.map(|value| value.name.as_str()).unwrap_or("?")),
            clean(player.map(|value| value.steam_id.as_str()).unwrap_or("?")),
            packets.len(),
            packets.first().map(|(tick, _)| *tick).unwrap_or(0),
            packets.last().map(|(tick, _)| *tick).unwrap_or(0)
        )?;
    }

    let mut all_players_file = fs::File::create(format!("{}/all_players.tsv", out_dir))?;
    writeln!(all_players_file, "entity_id\tname\tsteamid\tuser_id")?;
    for (entity_id, player) in &result.players {
        writeln!(
            all_players_file,
            "{}\t{}\t{}\t{}",
            entity_id,
            clean(&player.name),
            clean(&player.steam_id),
            player.user_id
        )?;
    }

    let frames_dir = format!("{}/frames", out_dir);
    fs::create_dir_all(&frames_dir)?;
    for (client_id, packets) in &result.packets {
        let mut frames_file = fs::File::create(format!("{}/{}.txt", frames_dir, client_id))?;
        for (tick, raw) in packets {
            let hex = raw.iter().map(|byte| format!("{:02x}", byte)).collect::<String>();
            writeln!(frames_file, "{}|{}", tick, hex)?;
        }
    }

    let mut events_file = fs::File::create(format!("{}/events.tsv", out_dir))?;
    writeln!(events_file, "tick\tkind\tactor\ttarget\tdetail")?;
    for event in &result.events {
        writeln!(
            events_file,
            "{}\t{}\t{}\t{}\t{}",
            event.tick, event.kind, event.actor, event.target, event.detail
        )?;
    }

    println!(
        "codec: {:?}; voice clients: {}; events: {}",
        result.codec,
        result.packets.len(),
        result.events.len()
    );
    Ok(())
}
