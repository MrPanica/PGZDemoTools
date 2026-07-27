use crate::demo::{DemoInfo, safe_name};
use crate::voice_core;
use main_error::MainError;
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::Builder;
use uuid::Uuid;
use zip::write::SimpleFileOptions;

const SILENCE_OPUS: &[u8] = b"\xf8\xff\xfe";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoicePlayer {
    pub entity: u32,
    pub client: u8,
    pub name: String,
    pub steamid: String,
    pub packets: usize,
    pub first_tick: i64,
    pub last_tick: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DemoPlayer {
    pub entity: u32,
    pub name: String,
    pub steamid: String,
    pub user_id: u16,
}

#[derive(Clone, Debug, Serialize)]
pub struct DemoEvent {
    pub tick: i64,
    pub kind: String,
    pub actor: String,
    pub target: String,
    pub detail: String,
}

pub struct DemoIndex {
    pub players: Vec<VoicePlayer>,
    pub all_players: Vec<DemoPlayer>,
    pub events: Vec<DemoEvent>,
}

fn cache_stale(output: &Path) -> bool {
    let players = output.join("players.tsv");
    let all_players = output.join("all_players.tsv");
    let events = output.join("events.tsv");
    if [&players, &all_players, &events]
        .iter()
        .any(|path| !path.is_file())
    {
        return true;
    }
    let executable_time = std::env::current_exe()
        .and_then(|path| path.metadata())
        .and_then(|metadata| metadata.modified());
    let event_time = events.metadata().and_then(|metadata| metadata.modified());
    matches!((executable_time, event_time), (Ok(executable), Ok(event)) if executable > event)
}

pub fn extract_demo_index(info: &DemoInfo, output: &Path) -> Result<DemoIndex, MainError> {
    if cache_stale(output) {
        fs::create_dir_all(output)?;
        voice_core::extract_demo_index(&info.data, output)?;
    }

    let mut players = Vec::new();
    for line in BufReader::new(File::open(output.join("players.tsv"))?)
        .lines()
        .skip(1)
    {
        let line = line?;
        let parts: Vec<_> = line.trim_end_matches(['\r', '\n']).split('\t').collect();
        if parts.len() < 7 {
            continue;
        }
        players.push(VoicePlayer {
            entity: parts[0].parse()?,
            client: parts[1].parse()?,
            name: parts[2..parts.len() - 4].join("\t"),
            steamid: parts[parts.len() - 4].to_owned(),
            packets: parts[parts.len() - 3].parse()?,
            first_tick: parts[parts.len() - 2].parse()?,
            last_tick: parts[parts.len() - 1].parse()?,
        });
    }

    let mut all_players = Vec::new();
    for line in BufReader::new(File::open(output.join("all_players.tsv"))?)
        .lines()
        .skip(1)
    {
        let line = line?;
        let parts: Vec<_> = line.trim_end_matches(['\r', '\n']).split('\t').collect();
        if parts.len() == 4 {
            all_players.push(DemoPlayer {
                entity: parts[0].parse()?,
                name: parts[1].to_owned(),
                steamid: parts[2].to_owned(),
                user_id: parts[3].parse()?,
            });
        }
    }

    let mut events = Vec::new();
    for line in BufReader::new(File::open(output.join("events.tsv"))?)
        .lines()
        .skip(1)
    {
        let line = line?;
        let parts: Vec<_> = line.trim_end_matches(['\r', '\n']).split('\t').collect();
        if parts.len() == 5 {
            events.push(DemoEvent {
                tick: parts[0].parse()?,
                kind: parts[1].to_owned(),
                actor: parts[2].to_owned(),
                target: parts[3].to_owned(),
                detail: parts[4].to_owned(),
            });
        }
    }
    Ok(DemoIndex {
        players,
        all_players,
        events,
    })
}

fn steam_crc(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn opus_samples_48k(packet: &[u8]) -> u64 {
    let Some(toc) = packet.first().copied() else {
        return 0;
    };
    let config = toc >> 3;
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ if packet.len() > 1 => packet[1] & 0x3f,
        _ => return 0,
    } as u64;
    let per_frame = if config < 12 {
        [480, 960, 1920, 2880][(config & 3) as usize]
    } else if config < 16 {
        [480, 960][(config & 1) as usize]
    } else {
        [120, 240, 480, 960][(config & 3) as usize]
    };
    let total = per_frame * frames;
    if total <= 5760 { total } else { 0 }
}

#[derive(Default)]
struct ContainerState {
    rate: u32,
    sequence: u32,
}

fn container_packets(raw: &[u8], state: &mut ContainerState) -> Vec<Vec<u8>> {
    if raw.len() < 12
        || steam_crc(&raw[..raw.len() - 4])
            != u32::from_le_bytes(raw[raw.len() - 4..].try_into().unwrap())
    {
        return Vec::new();
    }
    if state.rate == 0 {
        state.rate = 24_000;
    }
    let body = &raw[..raw.len() - 4];
    let mut index = 8usize;
    let mut packets = Vec::new();
    while index + 3 <= body.len() {
        let tag = body[index];
        let size = u16::from_le_bytes([body[index + 1], body[index + 2]]) as usize;
        index += 3;
        if tag == 11 {
            state.rate = if size == 0 { 24_000 } else { size as u32 };
            continue;
        }
        if tag == 0 {
            let numerator = size as u64 * 48_000;
            let denominator = state.rate as u64 * 960;
            let samples = numerator.div_ceil(denominator);
            packets.extend((0..samples).map(|_| SILENCE_OPUS.to_vec()));
            continue;
        }
        if index + size > body.len() {
            break;
        }
        let payload = &body[index..index + size];
        index += size;
        if tag != 6 {
            break;
        }
        let mut cursor = 0usize;
        while cursor + 4 <= payload.len() {
            let frame_len = u16::from_le_bytes([payload[cursor], payload[cursor + 1]]) as usize;
            let sequence = u16::from_le_bytes([payload[cursor + 2], payload[cursor + 3]]) as u32;
            cursor += 4;
            if frame_len == 0xffff {
                state.sequence = 0;
                continue;
            }
            if cursor + frame_len > payload.len() {
                break;
            }
            let packet = &payload[cursor..cursor + frame_len];
            cursor += frame_len;
            let mut expected = state.sequence;
            if sequence < expected || sequence - expected > 128 {
                expected = sequence;
            }
            packets.extend((expected..sequence).map(|_| SILENCE_OPUS.to_vec()));
            if opus_samples_48k(packet) != 0 {
                packets.push(packet.to_vec());
            }
            state.sequence = sequence + 1;
        }
    }
    packets
}

fn ogg_crc(page: &[u8]) -> u32 {
    let mut crc = 0u32;
    for byte in page {
        crc ^= (*byte as u32) << 24;
        for _ in 0..8 {
            crc = (crc << 1)
                ^ if crc & 0x8000_0000 != 0 {
                    0x04c1_1db7
                } else {
                    0
                };
        }
    }
    crc
}

fn ogg_page(packet: &[u8], serial: u32, sequence: u32, granule: u64, flags: u8) -> Vec<u8> {
    let mut lacing = vec![255u8; packet.len() / 255];
    lacing.push((packet.len() % 255) as u8);
    let mut page = Vec::with_capacity(27 + lacing.len() + packet.len());
    page.extend_from_slice(b"OggS");
    page.extend_from_slice(&[0, flags]);
    page.extend_from_slice(&granule.to_le_bytes());
    page.extend_from_slice(&serial.to_le_bytes());
    page.extend_from_slice(&sequence.to_le_bytes());
    page.extend_from_slice(&0u32.to_le_bytes());
    page.push(lacing.len() as u8);
    page.extend_from_slice(&lacing);
    page.extend_from_slice(packet);
    let checksum = ogg_crc(&page);
    page[22..26].copy_from_slice(&checksum.to_le_bytes());
    page
}

fn write_ogg(path: &Path, packets: &[Vec<u8>]) -> Result<(), MainError> {
    let vendor = b"TF2 Demo Tools";
    let mut opus_head = b"OpusHead".to_vec();
    opus_head.extend_from_slice(&[1, 1]);
    opus_head.extend_from_slice(&0u16.to_le_bytes());
    opus_head.extend_from_slice(&24_000u32.to_le_bytes());
    opus_head.extend_from_slice(&0i16.to_le_bytes());
    opus_head.push(0);
    let mut opus_tags = b"OpusTags".to_vec();
    opus_tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    opus_tags.extend_from_slice(vendor);
    opus_tags.extend_from_slice(&0u32.to_le_bytes());
    let serial = u32::from_le_bytes(Uuid::new_v4().as_bytes()[..4].try_into().unwrap());
    let mut output = BufWriter::new(File::create(path)?);
    output.write_all(&ogg_page(&opus_head, serial, 0, 0, 2))?;
    output.write_all(&ogg_page(&opus_tags, serial, 1, 0, 0))?;
    let mut granule = 0u64;
    for (index, packet) in packets.iter().enumerate() {
        granule += opus_samples_48k(packet);
        output.write_all(&ogg_page(
            packet,
            serial,
            index as u32 + 2,
            granule,
            if index + 1 == packets.len() { 4 } else { 0 },
        ))?;
    }
    output.flush()?;
    Ok(())
}

fn decode_hex(value: &str) -> Result<Vec<u8>, MainError> {
    if value.len() % 2 != 0 {
        return Err("invalid voice packet hex".into());
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).map_err(Into::into))
        .collect()
}

pub fn build_player_ogg(
    frames_file: &Path,
    target: &Path,
    tick_rate: f64,
    keep_gaps: bool,
) -> Result<PathBuf, MainError> {
    let mut state = ContainerState {
        rate: 24_000,
        sequence: 0,
    };
    let mut packets = Vec::new();
    let mut granule = 0u64;
    for line in BufReader::new(File::open(frames_file)?).lines() {
        let line = line?;
        let Some((tick, raw)) = line.trim().split_once('|') else {
            continue;
        };
        let tick: i64 = tick.parse()?;
        if keep_gaps {
            let target_granule = (tick as f64 / tick_rate * 48_000.0).round() as u64;
            while granule + 960 <= target_granule {
                packets.push(SILENCE_OPUS.to_vec());
                granule += 960;
            }
        }
        for packet in container_packets(&decode_hex(raw)?, &mut state) {
            granule += opus_samples_48k(&packet);
            packets.push(packet);
        }
    }
    if packets.is_empty() {
        return Err("selected player has no valid Opus packets".into());
    }
    write_ogg(target, &packets)?;
    Ok(target.to_path_buf())
}

pub fn export_voices(
    info: &DemoInfo,
    output: &Path,
    queries: &[String],
    all: bool,
    keep_gaps: bool,
    audio_format: &str,
    workspace: &Path,
) -> Result<Vec<PathBuf>, MainError> {
    fs::create_dir_all(output)?;
    fs::create_dir_all(workspace)?;
    let temporary = Builder::new().prefix("tf2_voice_").tempdir_in(workspace)?;
    let index = extract_demo_index(info, temporary.path())?;
    let selected: Vec<_> = if all {
        index.players.iter().collect()
    } else {
        index
            .players
            .iter()
            .filter(|player| {
                queries.iter().any(|query| {
                    query
                        .parse::<u8>()
                        .is_ok_and(|client| client == player.client)
                        || player.name.to_lowercase().contains(&query.to_lowercase())
                })
            })
            .collect()
    };
    if selected.is_empty() {
        return Err("no matching players with voice data".into());
    }
    let mut targets = Vec::with_capacity(selected.len());
    for player in selected {
        let name = safe_name(&player.name, &format!("client-{}", player.client));
        let ogg = output.join(format!("{name}.ogg"));
        build_player_ogg(
            &temporary
                .path()
                .join("frames")
                .join(format!("{}.txt", player.client)),
            &ogg,
            info.tick_rate,
            keep_gaps,
        )?;
        if audio_format == "ogg" {
            targets.push(ogg);
            continue;
        }
        let target = ogg.with_extension(audio_format);
        let status = Command::new("ffmpeg")
            .args(["-y", "-i"])
            .arg(&ogg)
            .arg(&target)
            .status()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    format!("--format {audio_format} needs ffmpeg in PATH")
                } else {
                    error.to_string()
                }
            })?;
        if !status.success() {
            return Err(format!("ffmpeg failed with {status}").into());
        }
        fs::remove_file(&ogg)?;
        targets.push(target);
    }
    Ok(targets)
}

pub fn create_zip(paths: &[PathBuf], archive: &Path) -> Result<PathBuf, MainError> {
    let output = File::create(archive)?;
    let mut zip = zip::ZipWriter::new(output);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut buffer = [0u8; 64 * 1024];
    for path in paths {
        let name = path
            .file_name()
            .ok_or("voice path has no file name")?
            .to_string_lossy();
        zip.start_file(name, options)?;
        let mut source = File::open(path)?;
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            zip.write_all(&buffer[..read])?;
        }
    }
    zip.finish()?;
    Ok(archive.to_path_buf())
}

pub fn unique_clients(clients: &[u8]) -> Vec<u8> {
    let mut seen = BTreeSet::new();
    clients
        .iter()
        .copied()
        .filter(|client| seen.insert(*client))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{SILENCE_OPUS, ogg_crc, ogg_page, opus_samples_48k, steam_crc};

    #[test]
    fn audio_self_check() {
        assert_eq!(steam_crc(b"abc123"), 3_473_062_748);
        assert_eq!(opus_samples_48k(SILENCE_OPUS), 960);
        let page = ogg_page(b"x", 1, 0, 0, 2);
        let stored = u32::from_le_bytes(page[22..26].try_into().unwrap());
        let mut clean = page;
        clean[22..26].fill(0);
        assert_eq!(&clean[..4], b"OggS");
        assert_eq!(ogg_crc(&clean), stored);
    }
}
