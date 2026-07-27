use encoding_rs::WINDOWS_1251;
use main_error::MainError;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const HEADER_SIZE: usize = 1072;
pub const PLAYBACK_OFFSET: usize = 1056;
pub const CMD_SIGNON: u8 = 1;
pub const CMD_PACKET: u8 = 2;
pub const CMD_SYNCTICK: u8 = 3;
pub const CMD_USER: u8 = 5;
pub const CMD_STOP: u8 = 7;
pub const CMD_STRINGTABLES: u8 = 8;

const CMD_CONSOLE: u8 = 4;
const CMD_DATATABLES: u8 = 6;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DemoKind {
    Pov,
    SourceTv,
}

impl DemoKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pov => "POV",
            Self::SourceTv => "SourceTV",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Record {
    pub start: usize,
    pub end: usize,
    pub command: u8,
    pub tick: i32,
}

#[derive(Clone)]
pub struct DemoInfo {
    pub path: PathBuf,
    pub data: Arc<[u8]>,
    pub protocol: i32,
    pub network: i32,
    pub server: String,
    pub client: String,
    pub map: String,
    pub game: String,
    pub duration: f64,
    pub ticks: u32,
    pub frames: u32,
    pub tick_rate: f64,
    pub signon_end: usize,
    pub records: Vec<Record>,
    pub body: Vec<Record>,
    pub complete_stop: bool,
    pub kind: DemoKind,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DemoMeta {
    pub name: String,
    pub size: u64,
    pub server: String,
    pub client: String,
    pub map: String,
    pub game: String,
    pub duration: f64,
    pub ticks: u32,
    pub frames: u32,
    pub tick_rate: f64,
    pub protocol: i32,
    pub network_protocol: i32,
    pub kind: &'static str,
    pub complete_stop: bool,
    pub density: Vec<usize>,
}

fn read_i32(data: &[u8], offset: usize) -> Result<i32, MainError> {
    let bytes: [u8; 4] = data
        .get(offset..offset + 4)
        .ok_or_else(|| format!("truncated int32 at byte {offset}"))?
        .try_into()?;
    Ok(i32::from_le_bytes(bytes))
}

pub(crate) fn read_u32(data: &[u8], offset: usize) -> Result<u32, MainError> {
    let bytes: [u8; 4] = data
        .get(offset..offset + 4)
        .ok_or_else(|| format!("truncated uint32 at byte {offset}"))?
        .try_into()?;
    Ok(u32::from_le_bytes(bytes))
}

fn raw_block_end(data: &[u8], size_offset: usize) -> Result<usize, MainError> {
    let size = read_i32(data, size_offset)?;
    if size < 0 {
        return Err(format!("invalid block size {size} at byte {size_offset}").into());
    }
    let end = size_offset
        .checked_add(4)
        .and_then(|value| value.checked_add(size as usize))
        .ok_or("demo block size overflow")?;
    if end > data.len() {
        return Err(format!("invalid block size {size} at byte {size_offset}").into());
    }
    Ok(end)
}

fn scan_records(data: &[u8], playback_ticks: u32) -> Result<(Vec<Record>, bool), MainError> {
    let mut records = Vec::new();
    let mut offset = HEADER_SIZE;
    while offset < data.len() {
        if data.len() - offset < 5 {
            let stop = [&[CMD_STOP][..], &(playback_ticks as i32).to_le_bytes()].concat();
            if data[offset..] == stop[..data.len() - offset] {
                return Ok((records, false));
            }
            return Err(format!("truncated command at byte {offset}").into());
        }
        let command = data[offset];
        let tick = read_i32(data, offset + 1)?;
        let mut end = offset + 5;
        match command {
            CMD_SIGNON | CMD_PACKET => end = raw_block_end(data, end + 84)?,
            CMD_CONSOLE | CMD_DATATABLES | CMD_STRINGTABLES => end = raw_block_end(data, end)?,
            CMD_USER => end = raw_block_end(data, end + 4)?,
            CMD_SYNCTICK | CMD_STOP => {}
            _ => return Err(format!("unknown demo command {command} at byte {offset}").into()),
        }
        records.push(Record {
            start: offset,
            end,
            command,
            tick,
        });
        offset = end;
        if command == CMD_STOP {
            if offset != data.len() {
                return Err(format!("data after dem_stop at byte {offset}").into());
            }
            return Ok((records, true));
        }
    }
    Err("demo has no dem_stop command".into())
}

fn text_field(data: &[u8], offset: usize) -> String {
    let field = data.get(offset..offset + 260).unwrap_or_default();
    let raw = &field[..field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len())];
    let text = match std::str::from_utf8(raw) {
        Ok(value) => value.to_owned(),
        Err(_) => WINDOWS_1251.decode(raw).0.into_owned(),
    };
    text.replace('\u{fffd}', "?")
}

pub fn read_demo(path: impl AsRef<Path>) -> Result<DemoInfo, MainError> {
    let path = path.as_ref();
    let data: Arc<[u8]> = fs::read(path)?.into();
    read_demo_bytes(path.to_path_buf(), data)
}

pub fn read_demo_bytes(path: PathBuf, data: Arc<[u8]>) -> Result<DemoInfo, MainError> {
    if data.len() < HEADER_SIZE || data[..8].split(|byte| *byte == 0).next() != Some(b"HL2DEMO") {
        return Err("not a Source demo".into());
    }
    let protocol = read_i32(&data, 8)?;
    let network = read_i32(&data, 12)?;
    if protocol != 3 {
        return Err(
            format!("unsupported demo protocol {protocol}; expected TF2 protocol 3").into(),
        );
    }
    let playback_time = f32::from_le_bytes(data[PLAYBACK_OFFSET..PLAYBACK_OFFSET + 4].try_into()?);
    let ticks = read_i32(&data, PLAYBACK_OFFSET + 4)?;
    let frames = read_i32(&data, PLAYBACK_OFFSET + 8)?;
    let signon_length = read_i32(&data, PLAYBACK_OFFSET + 12)?;
    if playback_time <= 0.0 || ticks <= 0 || frames < 0 || signon_length <= 0 {
        return Err("invalid demo header".into());
    }
    let signon_end = HEADER_SIZE
        .checked_add(signon_length as usize)
        .ok_or("signon length overflow")?;
    if signon_end > data.len() {
        return Err("signon block extends past end of file".into());
    }
    let ticks = ticks as u32;
    let (records, complete_stop) = scan_records(&data, ticks)?;
    let boundary = signon_end == HEADER_SIZE
        || records
            .iter()
            .any(|record| record.start == signon_end || record.end == signon_end);
    if !boundary {
        return Err("signon length does not end on a command boundary".into());
    }
    let body: Vec<_> = records
        .iter()
        .copied()
        .filter(|record| record.start >= signon_end && record.command != CMD_STOP)
        .collect();
    let duration = playback_time as f64;
    let kind = if body.iter().any(|record| record.command == CMD_USER) {
        DemoKind::Pov
    } else {
        DemoKind::SourceTv
    };
    let server = text_field(&data, 16);
    let client = text_field(&data, 276);
    let map = text_field(&data, 536);
    let game = text_field(&data, 796);
    Ok(DemoInfo {
        path,
        data,
        protocol,
        network,
        server,
        client,
        map,
        game,
        duration,
        ticks,
        frames: frames as u32,
        tick_rate: ticks as f64 / duration,
        signon_end,
        records,
        body,
        complete_stop,
        kind,
    })
}

impl DemoInfo {
    pub fn meta(&self) -> DemoMeta {
        let mut density = vec![0usize; 160];
        for record in &self.body {
            if record.command == CMD_PACKET && record.tick >= 0 && (record.tick as u32) < self.ticks
            {
                let bucket = ((record.tick as usize) * density.len() / self.ticks as usize)
                    .min(density.len() - 1);
                density[bucket] += record.end - record.start;
            }
        }
        DemoMeta {
            name: self
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            size: self.data.len() as u64,
            server: self.server.clone(),
            client: self.client.clone(),
            map: self.map.clone(),
            game: self.game.clone(),
            duration: self.duration,
            ticks: self.ticks,
            frames: self.frames,
            tick_rate: self.tick_rate,
            protocol: self.protocol,
            network_protocol: self.network,
            kind: self.kind.label(),
            complete_stop: self.complete_stop,
            density,
        }
    }
}

pub fn normalize_ranges(
    ranges: &[(u32, u32)],
    ticks: u32,
    ordered: bool,
) -> Result<Vec<(u32, u32)>, MainError> {
    if ranges.is_empty()
        || ranges
            .iter()
            .any(|(start, end)| *start >= *end || *end > ticks)
    {
        return Err("invalid edit range".into());
    }
    if ordered {
        return Ok(ranges.to_vec());
    }
    let mut clean = ranges.to_vec();
    clean.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for (start, end) in clean {
        if let Some(previous) = merged.last_mut() {
            if start <= previous.1 {
                previous.1 = previous.1.max(end);
                continue;
            }
        }
        merged.push((start, end));
    }
    Ok(merged)
}

pub fn safe_name(value: &str, fallback: &str) -> String {
    let mut value: String = value
        .chars()
        .map(|character| {
            if character.is_control() || "<>:\"/\\|?*".contains(character) {
                '_'
            } else {
                character
            }
        })
        .collect();
    value = value.trim_matches([' ', '.']).chars().take(100).collect();
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value
    }
}

pub fn parse_time_range(value: &str) -> Result<(f64, f64), MainError> {
    let (start, end) = value
        .split_once(':')
        .ok_or("--range must use START:END seconds")?;
    let start: f64 = start
        .parse()
        .map_err(|_| "--range must use START:END seconds")?;
    let end: f64 = end
        .parse()
        .map_err(|_| "--range must use START:END seconds")?;
    if start < 0.0 || end <= start {
        return Err("--range end must be greater than start".into());
    }
    Ok((start, end))
}

#[cfg(test)]
mod tests {
    use super::{normalize_ranges, parse_time_range, safe_name};

    #[test]
    fn range_and_filename_compatibility() {
        assert_eq!(
            normalize_ranges(&[(5, 9), (1, 3), (3, 6)], 10, false).unwrap(),
            vec![(1, 9)]
        );
        assert_eq!(
            normalize_ranges(&[(5, 9), (1, 3)], 10, true).unwrap(),
            vec![(5, 9), (1, 3)]
        );
        assert_eq!(parse_time_range("1.5:2.5").unwrap(), (1.5, 2.5));
        assert_eq!(safe_name(" тест?.dem ", "output"), "тест_.dem");
    }
}
