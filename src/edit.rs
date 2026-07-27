use crate::demo::{
    CMD_PACKET, CMD_SIGNON, CMD_STOP, CMD_STRINGTABLES, CMD_SYNCTICK, CMD_USER, DemoInfo, DemoKind,
    HEADER_SIZE, PLAYBACK_OFFSET, Record, read_demo, read_u32,
};
use crate::{freecam_core, pov_core, source_core};
use main_error::MainError;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use tempfile::{Builder, TempPath};

fn prepare_target(target: &Path) -> Result<TempPath, MainError> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    Ok(Builder::new()
        .prefix(".pgz-")
        .suffix(".tmp")
        .tempfile_in(parent)?
        .into_temp_path())
}

fn persist(path: TempPath, target: &Path) -> Result<(), MainError> {
    path.persist(target).map_err(|error| error.error)?;
    Ok(())
}

fn rewrite_record(
    info: &DemoInfo,
    record: Record,
    command: Option<u8>,
    tick: Option<i32>,
) -> Vec<u8> {
    let mut output = info.data[record.start..record.end].to_vec();
    output[0] = command.unwrap_or(record.command);
    output[1..5].copy_from_slice(&tick.unwrap_or(record.tick).to_le_bytes());
    output
}

fn rewrite_user_sequence(record: &mut [u8], sequence: u32) -> Result<(), MainError> {
    if record.len() < 13 {
        return Err("truncated user command".into());
    }
    record[5..9].copy_from_slice(&sequence.to_le_bytes());
    let size = read_u32(record, 9)? as usize;
    let payload = record
        .get_mut(13..13 + size)
        .ok_or("truncated user command payload")?;
    if payload.first().is_none_or(|byte| byte & 1 == 0) {
        return Err("user command has no command number".into());
    }
    for bit in 0..32 {
        let position = bit + 1;
        let mask = 1 << (position % 8);
        if sequence & (1 << bit) != 0 {
            payload[position / 8] |= mask;
        } else {
            payload[position / 8] &= !mask;
        }
    }
    Ok(())
}

fn sequence_delta(value: u32, origin: u32) -> i32 {
    value.wrapping_sub(origin) as i32
}

pub fn edit_demo(
    info: &DemoInfo,
    ranges: &[(u32, u32)],
    target: &Path,
    workspace: &Path,
) -> Result<PathBuf, MainError> {
    edit_demo_with_progress(info, ranges, target, workspace, &mut |_| {})
}

pub fn edit_demo_with_progress(
    info: &DemoInfo,
    ranges: &[(u32, u32)],
    target: &Path,
    workspace: &Path,
    progress: &mut dyn FnMut(u8),
) -> Result<PathBuf, MainError> {
    let ranges = crate::demo::normalize_ranges(ranges, info.ticks, true)?;
    match info.kind {
        DemoKind::Pov => write_checkpoint_edit(info, &ranges, target, workspace, 0, 0, progress),
        DemoKind::SourceTv => write_source(info, &ranges, target, progress),
    }
}

pub fn edit_demo_with_freecam(
    info: &DemoInfo,
    ranges: &[(u32, u32)],
    target: &Path,
    workspace: &Path,
) -> Result<PathBuf, MainError> {
    edit_demo_with_freecam_progress(info, ranges, target, workspace, &mut |_| {})
}

pub fn edit_demo_with_freecam_progress(
    info: &DemoInfo,
    ranges: &[(u32, u32)],
    target: &Path,
    workspace: &Path,
    progress: &mut dyn FnMut(u8),
) -> Result<PathBuf, MainError> {
    if info.kind != DemoKind::Pov {
        return Err("free camera is available for POV demos only".into());
    }
    fs::create_dir_all(workspace)?;
    let directory = Builder::new()
        .prefix("pov_freecam_")
        .tempdir_in(workspace)?;
    let mut montage_progress = |value: u8| progress(value.saturating_mul(90) / 100);
    let montage = edit_demo_with_progress(
        info,
        ranges,
        &directory.path().join("montage.dem"),
        workspace,
        &mut montage_progress,
    )?;
    progress(92);
    let montage_info = read_demo(&montage)?;
    let unlocked = freecam_core::unlock_pov_freecam(&montage_info.data)?;
    progress(97);
    let temporary = prepare_target(target)?;
    let temporary_path: &Path = temporary.as_ref();
    {
        let mut output = BufWriter::new(File::create(temporary_path)?);
        output.write_all(&unlocked)?;
        output.flush()?;
    }
    let edited = read_demo(temporary_path)?;
    if edited.kind != DemoKind::SourceTv
        || !edited.complete_stop
        || edited.ticks != montage_info.ticks
        || edited.frames != montage_info.frames
    {
        return Err("free-camera montage verification failed".into());
    }
    persist(temporary, target)?;
    progress(100);
    Ok(target.to_path_buf())
}

fn write_source(
    info: &DemoInfo,
    ranges: &[(u32, u32)],
    target: &Path,
    progress: &mut dyn FnMut(u8),
) -> Result<PathBuf, MainError> {
    let temporary = prepare_target(target)?;
    source_core::cut_source_raw_replay_with_progress(
        &info.data,
        temporary.as_ref(),
        ranges,
        progress,
    )?;
    persist(temporary, target)?;
    let expected: u32 = ranges.iter().map(|(start, end)| end - start).sum();
    let edited = read_demo(target)?;
    if !edited.complete_stop
        || edited.ticks < expected
        || edited.ticks > expected.saturating_add(64 * ranges.len() as u32)
    {
        return Err("SourceTV montage verification failed".into());
    }
    progress(100);
    Ok(target.to_path_buf())
}

fn write_checkpoint_edit(
    info: &DemoInfo,
    ranges: &[(u32, u32)],
    target: &Path,
    workspace: &Path,
    mut server_tick_offset: u32,
    string_table_start_tick: u32,
    progress: &mut dyn FnMut(u8),
) -> Result<PathBuf, MainError> {
    if ranges.len() > 1 {
        fs::create_dir_all(workspace)?;
        let directory = Builder::new()
            .prefix("pov_montage_")
            .tempdir_in(workspace)?;
        let mut fragments = Vec::with_capacity(ranges.len());
        let mut previous_end = None;
        for (index, current) in ranges.iter().copied().enumerate() {
            let fragment = directory.path().join(format!("{index}.dem"));
            let table_start = previous_end
                .filter(|previous| current.0 >= *previous)
                .unwrap_or(0);
            let range_start = index * 90 / ranges.len();
            let range_width = 90 / ranges.len();
            let mut range_progress =
                |value| progress((range_start + usize::from(value) * range_width / 100) as u8);
            write_checkpoint_edit(
                info,
                &[current],
                &fragment,
                workspace,
                server_tick_offset,
                table_start,
                &mut range_progress,
            )?;
            server_tick_offset = server_tick_offset.saturating_add(read_demo(&fragment)?.ticks);
            previous_end = Some(current.1);
            fragments.push(fragment);
        }
        progress(95);
        let result = join_checkpoint_fragments(&fragments, target);
        if result.is_ok() {
            progress(100);
        }
        return result;
    }

    let (start, end) = ranges[0];
    let temporary = prepare_target(target)?;
    pov_core::cut_pov_with_progress(
        &info.data,
        temporary.as_ref(),
        start,
        end,
        server_tick_offset,
        string_table_start_tick,
        progress,
    )?;
    let temporary_path: &Path = temporary.as_ref();
    let edited = read_demo(temporary_path)?;
    if !edited.complete_stop || edited.ticks < end - start || edited.ticks > end - start + 64 {
        return Err("pov_cut produced an invalid demo".into());
    }
    persist(temporary, target)?;
    progress(100);
    Ok(target.to_path_buf())
}

fn join_checkpoint_fragments(paths: &[PathBuf], target: &Path) -> Result<PathBuf, MainError> {
    let infos = paths.iter().map(read_demo).collect::<Result<Vec<_>, _>>()?;
    let first = infos.first().ok_or("no POV fragments to join")?;
    let ticks: u32 = infos.iter().map(|info| info.ticks).sum();
    let frames: u32 = infos
        .iter()
        .flat_map(|info| &info.body)
        .filter(|record| record.command == CMD_PACKET)
        .count()
        .try_into()?;
    let mut header = first.data[..HEADER_SIZE].to_vec();
    let startup = &first.data[HEADER_SIZE..first.signon_end];
    header[PLAYBACK_OFFSET..PLAYBACK_OFFSET + 4]
        .copy_from_slice(&((ticks as f64 / first.tick_rate) as f32).to_le_bytes());
    header[PLAYBACK_OFFSET + 4..PLAYBACK_OFFSET + 8].copy_from_slice(&(ticks as i32).to_le_bytes());
    header[PLAYBACK_OFFSET + 8..PLAYBACK_OFFSET + 12]
        .copy_from_slice(&(frames as i32).to_le_bytes());
    header[PLAYBACK_OFFSET + 12..PLAYBACK_OFFSET + 16]
        .copy_from_slice(&(startup.len() as i32).to_le_bytes());

    let startup_packet = first.records.iter().rev().find(|record| {
        record.end <= first.signon_end && matches!(record.command, CMD_SIGNON | CMD_PACKET)
    });
    let (mut next_sequence_in, mut last_sequence_out, mut next_user_sequence) =
        if let Some(record) = startup_packet {
            let sequence_in = read_u32(&first.data, record.start + 81)?;
            let sequence_out = read_u32(&first.data, record.start + 85)?;
            (
                Some(sequence_in.wrapping_add(1)),
                Some(sequence_out),
                Some(sequence_out.wrapping_add(1)),
            )
        } else {
            (None, None, None)
        };

    let temporary = prepare_target(target)?;
    {
        let temporary_path: &Path = temporary.as_ref();
        let mut output = BufWriter::new(File::create(temporary_path)?);
        output.write_all(&header)?;
        output.write_all(startup)?;
        let mut cursor = 0u32;
        for (index, info) in infos.iter().enumerate() {
            let mut source_user_origin = info
                .body
                .iter()
                .find(|record| record.command == CMD_USER)
                .map(|record| read_u32(&info.data, record.start + 5))
                .transpose()?;
            let mut output_user_origin = next_user_sequence;
            let mut seen_user = false;
            for record in &info.body {
                if record.command == CMD_STOP
                    || (index > 0 && matches!(record.command, CMD_SYNCTICK | CMD_STRINGTABLES))
                {
                    continue;
                }
                let output_tick = cursor
                    .checked_add(record.tick.try_into()?)
                    .ok_or("POV output tick overflow")?;
                let mut rewritten = rewrite_record(info, *record, None, Some(output_tick as i32));
                if record.command == CMD_PACKET {
                    if next_sequence_in.is_none() {
                        let sequence_in = read_u32(&rewritten, 81)?;
                        let sequence_out = read_u32(&rewritten, 85)?;
                        next_sequence_in = Some(sequence_in);
                        last_sequence_out = Some(sequence_out);
                        next_user_sequence = Some(sequence_out.wrapping_add(1));
                        output_user_origin = next_user_sequence;
                    }
                    let source_out = read_u32(&rewritten, 85)?;
                    if seen_user {
                        if let Some(source_origin) = source_user_origin {
                            let origin =
                                output_user_origin.ok_or("missing output user sequence")?;
                            let latest = next_user_sequence
                                .ok_or("missing latest user sequence")?
                                .wrapping_sub(1);
                            let mapped = origin
                                .wrapping_add_signed(sequence_delta(source_out, source_origin));
                            last_sequence_out =
                                Some(mapped.min(latest).max(last_sequence_out.unwrap_or(mapped)));
                        }
                    }
                    rewritten[81..85].copy_from_slice(&next_sequence_in.unwrap().to_le_bytes());
                    rewritten[85..89].copy_from_slice(&last_sequence_out.unwrap().to_le_bytes());
                    next_sequence_in = Some(next_sequence_in.unwrap().wrapping_add(1));
                } else if record.command == CMD_USER {
                    let source_sequence = read_u32(&rewritten, 5)?;
                    if source_user_origin.is_none() {
                        source_user_origin = Some(source_sequence);
                        output_user_origin = next_user_sequence;
                    }
                    let sequence = output_user_origin
                        .ok_or("missing output user sequence")?
                        .wrapping_add_signed(sequence_delta(
                            source_sequence,
                            source_user_origin.unwrap(),
                        ));
                    rewrite_user_sequence(&mut rewritten, sequence)?;
                    next_user_sequence = Some(sequence.wrapping_add(1));
                    seen_user = true;
                }
                output.write_all(&rewritten)?;
            }
            cursor = cursor.saturating_add(info.ticks);
        }
        output.write_all(&[CMD_STOP])?;
        output.write_all(&(ticks as i32).to_le_bytes())?;
        output.flush()?;
    }
    persist(temporary, target)?;
    verify_edit(target, ticks, frames)?;
    Ok(target.to_path_buf())
}

fn verify_edit(path: &Path, expected_ticks: u32, expected_frames: u32) -> Result<(), MainError> {
    let info = read_demo(path)?;
    let normal_packets: Vec<_> = info
        .body
        .iter()
        .filter(|record| record.command == CMD_PACKET)
        .collect();
    if !info.complete_stop
        || info.ticks != expected_ticks
        || info.frames != expected_frames
        || info
            .body
            .first()
            .is_none_or(|record| record.command != CMD_SYNCTICK)
        || normal_packets.len() != expected_frames as usize
        || normal_packets
            .iter()
            .any(|record| record.tick < 0 || record.tick as u32 > expected_ticks)
    {
        return Err(format!("verification failed for {}", path.display()).into());
    }
    Ok(())
}

pub fn split_demo(
    source: &Path,
    output: &Path,
    parts: Option<u32>,
    seconds: Option<f64>,
    workspace: &Path,
) -> Result<Vec<PathBuf>, MainError> {
    let info = read_demo(source)?;
    let span = if let Some(seconds) = seconds {
        if seconds <= 0.0 {
            return Err("--seconds must be positive".into());
        }
        (seconds * info.tick_rate).round().max(1.0) as u32
    } else {
        let parts = parts.unwrap_or(5);
        if parts < 2 || parts > info.ticks {
            return Err("--parts must be between 2 and the tick count".into());
        }
        info.ticks.div_ceil(parts)
    };
    fs::create_dir_all(output)?;
    let count = info.ticks.div_ceil(span);
    let width = count.to_string().len();
    let mut targets = Vec::with_capacity(count as usize);
    for (index, start) in (0..info.ticks).step_by(span as usize).enumerate() {
        let end = start.saturating_add(span).min(info.ticks);
        let target = output.join(format!(
            "{}.part{:0width$}.dem",
            source.file_stem().unwrap_or_default().to_string_lossy(),
            index + 1,
            width = width
        ));
        if target.exists() {
            return Err(format!("output exists: {}", target.display()).into());
        }
        targets.push(edit_demo(&info, &[(start, end)], &target, workspace)?);
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::{rewrite_user_sequence, sequence_delta};

    #[test]
    fn user_sequence_matches_reference_self_check() {
        let mut record = Vec::from([5, 0, 0, 0, 0]);
        record.extend_from_slice(&7u32.to_le_bytes());
        record.extend_from_slice(&5u32.to_le_bytes());
        record.extend_from_slice(&((7u64 << 1) | 1).to_le_bytes()[..5]);
        rewrite_user_sequence(&mut record, 11).unwrap();
        assert_eq!(u32::from_le_bytes(record[5..9].try_into().unwrap()), 11);
        assert_eq!(
            (u64::from_le_bytes([
                record[13], record[14], record[15], record[16], record[17], 0, 0, 0
            ]) >> 1) as u32,
            11
        );
        assert_eq!(sequence_delta(1, u32::MAX), 2);
    }
}
