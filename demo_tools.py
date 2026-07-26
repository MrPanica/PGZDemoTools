#!/usr/bin/env python3
"""Local TF2 demo editor: POV/STV cuts, montage and per-player voice export."""

from __future__ import annotations

import argparse
import binascii
import json
import math
import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import threading
import urllib.parse
import uuid
import webbrowser
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


HEADER_SIZE = 1072
PLAYBACK_OFFSET = 1056
CMD_SIGNON, CMD_PACKET, CMD_SYNCTICK = 1, 2, 3
CMD_CONSOLE, CMD_USER, CMD_DATATABLES, CMD_STOP, CMD_STRINGTABLES = 4, 5, 6, 7, 8
MAX_UPLOAD = 8 * 1024**3
SILENCE_OPUS = b"\xf8\xff\xfe"  # Valid 20 ms Opus DTX packet.
DEFAULT_WORKSPACE = Path(
    os.environ.get("PGZ_DEMO_WORKSPACE", Path(__file__).resolve().parent / ".work")
)


def temporary_directory(prefix: str, root: Path = DEFAULT_WORKSPACE):
    root = Path(root).resolve()
    root.mkdir(parents=True, exist_ok=True)
    return tempfile.TemporaryDirectory(prefix=prefix, dir=root)


def i32(data: bytes, offset: int) -> int:
    if offset + 4 > len(data):
        raise ValueError(f"truncated int32 at byte {offset}")
    return struct.unpack_from("<i", data, offset)[0]


def raw_block_end(data: bytes, size_offset: int) -> int:
    size = i32(data, size_offset)
    if size < 0 or size_offset + 4 + size > len(data):
        raise ValueError(f"invalid block size {size} at byte {size_offset}")
    return size_offset + 4 + size


def scan_records(data: bytes, playback_ticks: int):
    records, offset = [], HEADER_SIZE
    while offset < len(data):
        if len(data) - offset < 5:
            tail = data[offset:]
            stop = struct.pack("<Bi", CMD_STOP, playback_ticks)
            if tail == stop[: len(tail)]:
                return records, False
            raise ValueError(f"truncated command at byte {offset}")
        command, tick = struct.unpack_from("<Bi", data, offset)
        end = offset + 5
        if command in (CMD_SIGNON, CMD_PACKET):
            end = raw_block_end(data, end + 84)
        elif command in (CMD_CONSOLE, CMD_DATATABLES, CMD_STRINGTABLES):
            end = raw_block_end(data, end)
        elif command == CMD_USER:
            end = raw_block_end(data, end + 4)
        elif command not in (CMD_SYNCTICK, CMD_STOP):
            raise ValueError(f"unknown demo command {command} at byte {offset}")
        records.append((offset, end, command, tick))
        offset = end
        if command == CMD_STOP:
            if offset != len(data):
                raise ValueError(f"data after dem_stop at byte {offset}")
            return records, True
    raise ValueError("demo has no dem_stop command")


def text_field(data: bytes, offset: int) -> str:
    raw = data[offset : offset + 260].split(b"\0", 1)[0]
    try:
        return raw.decode("utf-8")
    except UnicodeDecodeError:
        return raw.decode("cp1251", "replace")


def read_demo(path: Path):
    # ponytail: whole-file buffering keeps the parser tiny; switch to mmap above 8 GiB.
    data = path.read_bytes()
    if len(data) < HEADER_SIZE or data[:8].rstrip(b"\0") != b"HL2DEMO":
        raise ValueError("not a Source demo")
    protocol, network = struct.unpack_from("<ii", data, 8)
    if protocol != 3:
        raise ValueError(f"unsupported demo protocol {protocol}; expected TF2 protocol 3")
    playback_time, ticks, frames, signon_length = struct.unpack_from(
        "<fiii", data, PLAYBACK_OFFSET
    )
    if playback_time <= 0 or ticks <= 0 or signon_length <= 0:
        raise ValueError("invalid demo header")
    signon_end = HEADER_SIZE + signon_length
    if signon_end > len(data):
        raise ValueError("signon block extends past end of file")
    records, complete_stop = scan_records(data, ticks)
    boundaries = {HEADER_SIZE}
    for start, end, _command, _tick in records:
        boundaries.update((start, end))
    if signon_end not in boundaries:
        raise ValueError("signon length does not end on a command boundary")
    body = [r for r in records if r[0] >= signon_end and r[2] != CMD_STOP]
    tick_rate = ticks / playback_time
    return {
        "path": path,
        "data": data,
        "protocol": protocol,
        "network": network,
        "server": text_field(data, 16),
        "client": text_field(data, 276),
        "map": text_field(data, 536),
        "game": text_field(data, 796),
        "time": playback_time,
        "ticks": ticks,
        "frames": frames,
        "tick_rate": tick_rate,
        "signon_end": signon_end,
        "records": records,
        "body": body,
        "complete_stop": complete_stop,
        "kind": "POV" if any(r[2] == CMD_USER for r in body) else "SourceTV",
    }


def rewritten_record(info, record, command=None, tick=None) -> bytes:
    start, end, old_command, old_tick = record
    result = bytearray(info["data"][start:end])
    result[0] = old_command if command is None else command
    struct.pack_into("<i", result, 1, old_tick if tick is None else tick)
    return result


def rewrite_user_sequence(record: bytearray, sequence: int):
    struct.pack_into("<I", record, 5, sequence)
    size = struct.unpack_from("<I", record, 9)[0]
    payload = int.from_bytes(record[13 : 13 + size], "little")
    if not payload & 1:
        raise ValueError("user command has no command number")
    payload = (payload & ~(((1 << 32) - 1) << 1)) | (sequence << 1) | 1
    record[13 : 13 + size] = payload.to_bytes(size, "little")


def sequence_delta(value: int, origin: int) -> int:
    delta = (value - origin) & 0xFFFFFFFF
    return delta - 0x100000000 if delta & 0x80000000 else delta


def normalize_ranges(ranges, ticks, ordered=False):
    clean = sorted((int(a), int(b)) for a, b in ranges)
    if not clean or any(a < 0 or b > ticks or a >= b for a, b in clean):
        raise ValueError("invalid edit range")
    if ordered:
        return [(int(a), int(b)) for a, b in ranges]
    merged = []
    for start, end in clean:
        if merged and start <= merged[-1][1]:
            merged[-1] = (merged[-1][0], max(end, merged[-1][1]))
        else:
            merged.append((start, end))
    return merged


def write_edit(info, ranges, target: Path):
    ranges = normalize_ranges(ranges, info["ticks"], ordered=True)
    if info["kind"] == "POV":
        return write_checkpoint_edit(info, ranges, target)
    return write_source_web(info, ranges, target)


def bridge_tick(cursor, index, total, ticks):
    return cursor + index * ticks // total if ticks else cursor


def write_source_web(
    info,
    ranges,
    target: Path,
    normalize_packet_sequences=True,
    bridge_as_packets=False,
    bridge_ticks=0,
    replay_full_history=False,
):
    """Stable SourceTV path used by the web editor and normal CLI commands."""
    body = info["body"]

    def selected(record, start, end, is_last):
        return start <= record[3] < end or (is_last and record[3] == end)

    def bridge_records(previous_end, start):
        if previous_end is None:
            return []
        bridge_start = 0 if replay_full_history or previous_end > start else previous_end
        # ponytail: replaying packet history is enough for the CLI experiment; add mid-demo stringtables only if a demo proves it necessary.
        return [
            record
            for record in body
            if record[2] in (CMD_PACKET, CMD_STRINGTABLES)
            and bridge_start <= record[3] < start
        ]

    first_start = ranges[0][0]
    initial_warmup = [
        record for record in body if record[2] == CMD_PACKET and record[3] < first_start
    ]
    selected_packets = [
        record
        for index, (start, end) in enumerate(ranges)
        for record in body
        if record[2] == CMD_PACKET and selected(record, start, end, index == len(ranges) - 1)
    ]
    if not selected_packets:
        raise ValueError("selection contains no demo packets")
    bridges = [bridge_records(previous[1], start) for previous, (start, _end) in zip(ranges, ranges[1:])]
    bridge_frames = sum(len(bridge) for bridge in bridges)
    bridge_count = sum(bool(bridge) for bridge in bridges)
    output_ticks = sum(end - start for start, end in ranges) + (bridge_ticks * bridge_count if bridge_as_packets else 0)
    startup = info["data"][HEADER_SIZE : info["signon_end"]]
    header = bytearray(info["data"][:HEADER_SIZE])
    struct.pack_into(
        "<fiii",
        header,
        PLAYBACK_OFFSET,
        output_ticks / info["tick_rate"],
        output_ticks,
        len(selected_packets) + (bridge_frames if bridge_as_packets else 0),
        len(startup) + sum(record[1] - record[0] for record in initial_warmup),
    )
    temporary = target.with_suffix(target.suffix + ".tmp")
    target.parent.mkdir(parents=True, exist_ok=True)
    try:
        with temporary.open("wb") as output:
            startup_packets = [
                record
                for record in info["records"]
                if record[1] <= info["signon_end"]
                and record[2] in (CMD_SIGNON, CMD_PACKET)
            ]
            next_sequence = (
                (struct.unpack_from("<I", info["data"], startup_packets[-1][0] + 81)[0] + 1)
                & 0xFFFFFFFF
                if normalize_packet_sequences and startup_packets
                else None
            )

            def write_record(record, command=None, tick=None):
                nonlocal next_sequence
                rewritten = rewritten_record(info, record, command=command, tick=tick)
                # Packet metadata is still read after a dem_packet is repackaged
                # as dem_signon. Keep one monotonic stream through warmup, bridges
                # and selected ranges; otherwise a reverse range jumps backwards.
                if normalize_packet_sequences and record[2] == CMD_PACKET:
                    if next_sequence is None:
                        next_sequence = struct.unpack_from("<I", rewritten, 81)[0]
                    struct.pack_into("<II", rewritten, 81, next_sequence, next_sequence)
                    next_sequence = (next_sequence + 1) & 0xFFFFFFFF
                output.write(rewritten)

            output.write(header)
            output.write(startup)
            for record in initial_warmup:
                write_record(record, command=CMD_SIGNON)
            output.write(struct.pack("<Bi", CMD_SYNCTICK, 0))
            cursor, previous_end = 0, None
            for index, (start, end) in enumerate(ranges):
                if previous_end is not None:
                    bridge = bridge_records(previous_end, start)
                    packets = bridge
                    packet_index = 0
                    for record in bridge:
                        command = (
                            CMD_PACKET
                            if bridge_as_packets
                            else CMD_SIGNON if record[2] == CMD_PACKET else None
                        )
                        tick = cursor
                        if bridge_as_packets:
                            tick = bridge_tick(cursor, packet_index, len(packets), bridge_ticks)
                            packet_index += 1
                        write_record(record, command=command, tick=tick)
                    if bridge_as_packets and bridge:
                        cursor += bridge_ticks
                for record in body:
                    if record[2] != CMD_SYNCTICK and selected(record, start, end, index == len(ranges) - 1):
                        write_record(record, tick=cursor + record[3] - start)
                cursor += end - start
                previous_end = end
            output.write(struct.pack("<Bi", CMD_STOP, output_ticks))
        os.replace(temporary, target)
    finally:
        if temporary.exists():
            temporary.unlink()
    verify_edit(
        target,
        output_ticks,
        len(selected_packets) + (bridge_frames if bridge_as_packets else 0),
    )
    return target


def write_source_experiment(info, ranges, target: Path):
    """Cut SourceTV with the proven raw replay montage path."""
    temporary = target.with_suffix(target.suffix + ".tmp")
    target.parent.mkdir(parents=True, exist_ok=True)
    try:
        command = [
            str(helper_binary("pov_cut")),
            str(info["path"]),
            str(temporary),
            "--source-raw-replay",
        ]
        command.extend(str(tick) for pair in ranges for tick in pair)
        result = subprocess.run(
            command,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=900,
        )
        if result.returncode:
            detail = result.stderr.strip() or result.stdout.strip() or f"exit {result.returncode}"
            raise ValueError(f"pov_cut failed: {detail}")
        os.replace(temporary, target)
    finally:
        if temporary.exists():
            temporary.unlink()
    expected_ticks = sum(end - start for start, end in ranges)
    edited = read_demo(target)
    if not (
        edited["complete_stop"]
        and expected_ticks <= edited["ticks"] <= expected_ticks + 64 * len(ranges)
    ):
        raise ValueError("SourceTV montage verification failed")
    return target


def write_join(paths, target: Path, ranges=None):
    """Join compatible SourceTV recordings, restoring state at every boundary."""
    infos = [read_demo(path) for path in paths]
    first = infos[0]
    keys = ("protocol", "network", "map", "game", "server", "kind")
    if len(infos) < 2 or any(any(info[key] != first[key] for key in keys) for info in infos[1:]):
        raise ValueError("demos must use the same protocol, SourceTV server, map and game")
    if any(not math.isclose(info["tick_rate"], first["tick_rate"], rel_tol=1e-4) for info in infos[1:]):
        raise ValueError("demos use different tickrates")
    if ranges is None:
        tick_ranges = [(0, info["ticks"]) for info in infos]
    else:
        if len(ranges) != len(infos):
            raise ValueError("provide exactly one --range for every demo")
        tick_ranges = [
            normalize_ranges(
                [(round(start * info["tick_rate"]), round(end * info["tick_rate"]))],
                info["ticks"],
                ordered=True,
            )[0]
            for info, (start, end) in zip(infos, ranges)
        ]
    ticks = sum(end - start for start, end in tick_ranges)
    frames = sum(
        record[2] == CMD_PACKET and start <= record[3] < end
        for info, (start, end) in zip(infos, tick_ranges)
        for record in info["body"]
    )
    first_warmup = [
        record
        for record in first["body"]
        if record[2] == CMD_PACKET and record[3] < tick_ranges[0][0]
    ]
    header = bytearray(first["data"][:HEADER_SIZE])
    startup = first["data"][HEADER_SIZE : first["signon_end"]]
    signon_length = len(startup) + sum(record[1] - record[0] for record in first_warmup)
    struct.pack_into(
        "<fiii", header, PLAYBACK_OFFSET, ticks / first["tick_rate"], ticks, frames, signon_length
    )
    temporary = target.with_suffix(target.suffix + ".tmp")
    target.parent.mkdir(parents=True, exist_ok=True)
    try:
        with temporary.open("wb") as output:
            output.write(header)
            output.write(startup)
            for record in first_warmup:
                output.write(rewritten_record(first, record, command=CMD_SIGNON))
            output.write(struct.pack("<Bi", CMD_SYNCTICK, 0))
            cursor = 0
            for index, (info, (start, end)) in enumerate(zip(infos, tick_ranges)):
                if index:
                    for record in info["records"]:
                        if record[0] >= info["signon_end"]:
                            break
                        output.write(rewritten_record(info, record, tick=cursor))
                    for record in info["body"]:
                        if record[2] == CMD_PACKET and record[3] < start:
                            output.write(
                                rewritten_record(info, record, command=CMD_SIGNON, tick=cursor)
                            )
                    output.write(struct.pack("<Bi", CMD_SYNCTICK, cursor))
                for record in info["body"]:
                    if record[2] != CMD_SYNCTICK and start <= record[3] < end:
                        output.write(rewritten_record(info, record, tick=cursor + record[3] - start))
                cursor += end - start
            output.write(struct.pack("<Bi", CMD_STOP, ticks))
        os.replace(temporary, target)
    finally:
        if temporary.exists():
            temporary.unlink()
    verify_edit(target, ticks, frames)
    return target


def verify_edit(path: Path, expected_ticks: int, expected_frames: int):
    info = read_demo(path)
    normal_packets = [r for r in info["body"] if r[2] == CMD_PACKET]
    if (
        not info["complete_stop"]
        or info["ticks"] != expected_ticks
        or info["frames"] != expected_frames
        or not info["body"]
        or info["body"][0][2] != CMD_SYNCTICK
        or len(normal_packets) != expected_frames
        or any(tick < 0 or tick > expected_ticks for *_rest, tick in normal_packets)
    ):
        raise ValueError(f"verification failed for {path}")


def demo_meta(info):
    buckets = [0] * 160
    for start, end, command, tick in info["body"]:
        if command == CMD_PACKET and 0 <= tick < info["ticks"]:
            buckets[min(len(buckets) - 1, tick * len(buckets) // info["ticks"])] += end - start
    return {
        "name": info["path"].name,
        "size": info["path"].stat().st_size,
        "server": info["server"],
        "client": info["client"],
        "map": info["map"],
        "game": info["game"],
        "duration": info["time"],
        "ticks": info["ticks"],
        "frames": info["frames"],
        "tickRate": info["tick_rate"],
        "protocol": info["protocol"],
        "networkProtocol": info["network"],
        "kind": info["kind"],
        "completeStop": info["complete_stop"],
        "density": buckets,
    }


def safe_name(value: str, fallback="output") -> str:
    value = re.sub(r"[<>:\"/\\|?*\x00-\x1f]", "_", value).strip(" .")
    return value[:100] or fallback


def helper_binary(name: str) -> Path:
    executable = f"{name}.exe" if os.name == "nt" else name
    candidates = [
        Path(__file__).parent / "helper" / "target" / "release" / executable,
        Path(__file__).parent / ".build" / name / "release" / executable,
        Path(__file__).with_name(executable),
    ]
    for path in candidates:
        if path.is_file():
            return path
    raise ValueError(f"{name} is missing; run `python demo_tools.py build-helper`")


def voice_helper() -> Path:
    return helper_binary("voice_extract")


def join_checkpoint_fragments(paths, target: Path):
    """Join independently checkpointed fragments without a second sign-on."""
    infos = [read_demo(path) for path in paths]
    first = infos[0]
    ticks = sum(info["ticks"] for info in infos)
    frames = sum(record[2] == CMD_PACKET for info in infos for record in info["body"])
    header = bytearray(first["data"][:HEADER_SIZE])
    startup = first["data"][HEADER_SIZE : first["signon_end"]]
    struct.pack_into(
        "<fiii", header, PLAYBACK_OFFSET, ticks / first["tick_rate"], ticks, frames, len(startup)
    )
    temporary = target.with_suffix(target.suffix + ".tmp")
    target.parent.mkdir(parents=True, exist_ok=True)
    try:
        with temporary.open("wb") as output:
            output.write(header)
            output.write(startup)
            startup_packets = [
                record
                for record in first["records"]
                if record[1] <= first["signon_end"]
                and record[2] in (CMD_SIGNON, CMD_PACKET)
            ]
            if startup_packets:
                startup_in, startup_out = struct.unpack_from(
                    "<II", first["data"], startup_packets[-1][0] + 81
                )
                next_sequence_in = (startup_in + 1) & 0xFFFFFFFF
                last_sequence_out = startup_out
                next_user_sequence = (startup_out + 1) & 0xFFFFFFFF
            else:
                next_sequence_in = last_sequence_out = next_user_sequence = None
            cursor = 0
            for index, info in enumerate(infos):
                source_user_origin = next(
                    (
                        struct.unpack_from("<I", info["data"], record[0] + 5)[0]
                        for record in info["body"]
                        if record[2] == CMD_USER
                    ),
                    None,
                )
                output_user_origin = next_user_sequence
                seen_user = False
                for record in info["body"]:
                    if record[2] == CMD_STOP or index and record[2] in (
                        CMD_SYNCTICK,
                        CMD_STRINGTABLES,
                    ):
                        continue
                    rewritten = rewritten_record(info, record, tick=cursor + record[3])
                    if record[2] == CMD_PACKET:
                        if next_sequence_in is None:
                            next_sequence_in, last_sequence_out = struct.unpack_from(
                                "<II", rewritten, 81
                            )
                            next_user_sequence = (last_sequence_out + 1) & 0xFFFFFFFF
                            output_user_origin = next_user_sequence
                        source_out = struct.unpack_from("<I", rewritten, 85)[0]
                        if seen_user and source_user_origin is not None:
                            mapped = (
                                output_user_origin
                                + sequence_delta(source_out, source_user_origin)
                            ) & 0xFFFFFFFF
                            latest_user = (next_user_sequence - 1) & 0xFFFFFFFF
                            mapped = max(last_sequence_out, min(mapped, latest_user))
                            last_sequence_out = mapped
                        struct.pack_into(
                            "<II",
                            rewritten,
                            81,
                            next_sequence_in,
                            last_sequence_out,
                        )
                        next_sequence_in = (next_sequence_in + 1) & 0xFFFFFFFF
                    elif record[2] == CMD_USER:
                        source_sequence = struct.unpack_from("<I", rewritten, 5)[0]
                        if source_user_origin is None:
                            source_user_origin = source_sequence
                            output_user_origin = next_user_sequence
                        sequence = (
                            output_user_origin
                            + sequence_delta(source_sequence, source_user_origin)
                        ) & 0xFFFFFFFF
                        rewrite_user_sequence(rewritten, sequence)
                        next_user_sequence = (sequence + 1) & 0xFFFFFFFF
                        seen_user = True
                    output.write(rewritten)
                cursor += info["ticks"]
            output.write(struct.pack("<Bi", CMD_STOP, ticks))
        os.replace(temporary, target)
    finally:
        if temporary.exists():
            temporary.unlink()
    verify_edit(target, ticks, frames)
    return target


def write_checkpoint_edit(
    info, ranges, target: Path, server_tick_offset=0, string_table_start_tick=0
):
    if len(ranges) > 1:
        with temporary_directory("pov_montage_") as workspace:
            fragments = [Path(workspace) / f"{index}.dem" for index in range(len(ranges))]
            previous_end = None
            for current, fragment in zip(ranges, fragments):
                table_start = (
                    previous_end
                    if previous_end is not None and current[0] >= previous_end
                    else 0
                )
                write_checkpoint_edit(
                    info, [current], fragment, server_tick_offset, table_start
                )
                server_tick_offset += read_demo(fragment)["ticks"]
                previous_end = current[1]
            return join_checkpoint_fragments(fragments, target)
    start, end = ranges[0]
    target.parent.mkdir(parents=True, exist_ok=True)
    temporary = target.with_suffix(target.suffix + ".tmp")
    command = [str(helper_binary("pov_cut_stable"))]
    command.extend(
        (
            str(info["path"]),
            str(temporary),
            str(start),
            str(end),
            str(server_tick_offset),
            str(string_table_start_tick),
        )
    )
    try:
        result = subprocess.run(
            command,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=900,
        )
        if result.returncode:
            detail = result.stderr.strip() or result.stdout.strip() or f"exit {result.returncode}"
            raise ValueError(f"pov_cut failed: {detail}")
        edited = read_demo(temporary)
        if not edited["complete_stop"] or not end - start <= edited["ticks"] <= end - start + 64:
            raise ValueError("pov_cut produced an invalid demo")
        os.replace(temporary, target)
    finally:
        if temporary.exists():
            temporary.unlink()
    return target


def extract_demo_index(demo: Path, output: Path):
    players_file = output / "players.tsv"
    all_players_file = output / "all_players.tsv"
    events_file = output / "events.tsv"
    helper = voice_helper()
    if (
        not players_file.exists()
        or not all_players_file.exists()
        or not events_file.exists()
        or helper.stat().st_mtime_ns > events_file.stat().st_mtime_ns
    ):
        output.mkdir(parents=True, exist_ok=True)
        result = subprocess.run(
            [str(helper), str(demo), str(output)],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=300,
        )
        if result.returncode:
            raise ValueError((result.stderr or result.stdout or "voice parser failed").strip())
    players = []
    with players_file.open(encoding="utf-8") as source:
        next(source, None)
        for line in source:
            parts = line.rstrip("\r\n").split("\t")
            if len(parts) < 7:
                continue
            players.append(
                {
                    "entity": int(parts[0]),
                    "client": int(parts[1]),
                    "name": "\t".join(parts[2:-4]),
                    "steamid": parts[-4],
                    "packets": int(parts[-3]),
                    "firstTick": int(parts[-2]),
                    "lastTick": int(parts[-1]),
                }
            )
    all_players = []
    with all_players_file.open(encoding="utf-8") as source:
        next(source, None)
        for line in source:
            parts = line.rstrip("\r\n").split("\t")
            if len(parts) == 4:
                all_players.append(
                    {
                        "entity": int(parts[0]),
                        "name": parts[1],
                        "steamid": parts[2],
                        "userId": int(parts[3]),
                    }
                )
    events = []
    with events_file.open(encoding="utf-8") as source:
        next(source, None)
        for line in source:
            parts = line.rstrip("\r\n").split("\t")
            if len(parts) == 5:
                events.append(
                    {
                        "tick": int(parts[0]),
                        "kind": parts[1],
                        "actor": parts[2],
                        "target": parts[3],
                        "detail": parts[4],
                    }
                )
    return players, all_players, events


def extract_voice_index(demo: Path, output: Path):
    return extract_demo_index(demo, output)[0]


def steam_crc(data: bytes) -> int:
    return binascii.crc32(data) & 0xFFFFFFFF


def opus_samples_48k(packet: bytes) -> int:
    if not packet:
        return 0
    toc, config, code = packet[0], packet[0] >> 3, packet[0] & 3
    if code == 0:
        frames = 1
    elif code in (1, 2):
        frames = 2
    elif len(packet) > 1:
        frames = packet[1] & 0x3F
    else:
        return 0
    if config < 12:
        per_frame = (480, 960, 1920, 2880)[config & 3]
    elif config < 16:
        per_frame = (480, 960)[config & 1]
    else:
        per_frame = (120, 240, 480, 960)[config & 3]
    total = per_frame * frames
    return total if 0 < total <= 5760 else 0


def container_packets(raw: bytes, state: dict):
    if len(raw) < 12 or steam_crc(raw[:-4]) != int.from_bytes(raw[-4:], "little"):
        return []
    body, index, packets = raw[:-4], 8, []
    while index + 3 <= len(body):
        tag = body[index]
        size = int.from_bytes(body[index + 1 : index + 3], "little")
        index += 3
        if tag == 11:
            state["rate"] = size or 24000
            continue
        if tag == 0:
            samples = math.ceil(size * 48000 / state["rate"] / 960)
            packets.extend([SILENCE_OPUS] * samples)
            continue
        if index + size > len(body):
            break
        payload, index = body[index : index + size], index + size
        if tag != 6:
            break
        cursor = 0
        while cursor + 4 <= len(payload):
            frame_len, sequence = struct.unpack_from("<HH", payload, cursor)
            cursor += 4
            if frame_len == 0xFFFF:
                state["sequence"] = 0
                continue
            if cursor + frame_len > len(payload):
                break
            packet, cursor = payload[cursor : cursor + frame_len], cursor + frame_len
            expected = state["sequence"]
            if sequence < expected or sequence - expected > 128:
                expected = sequence
            packets.extend([SILENCE_OPUS] * (sequence - expected))
            if opus_samples_48k(packet):
                packets.append(packet)
            state["sequence"] = sequence + 1
    return packets


def ogg_crc(page: bytes) -> int:
    crc = 0
    for byte in page:
        crc ^= byte << 24
        for _ in range(8):
            crc = ((crc << 1) & 0xFFFFFFFF) ^ (0x04C11DB7 if crc & 0x80000000 else 0)
    return crc


def ogg_page(packet: bytes, serial: int, sequence: int, granule: int, flags=0) -> bytes:
    lacing = [255] * (len(packet) // 255) + [len(packet) % 255]
    header = struct.pack(
        "<4sBBQIIIB", b"OggS", 0, flags, granule, serial, sequence, 0, len(lacing)
    ) + bytes(lacing)
    page = bytearray(header + packet)
    struct.pack_into("<I", page, 22, ogg_crc(page))
    return bytes(page)


def write_ogg(path: Path, packets):
    vendor = b"TF2 Demo Tools"
    headers = [
        b"OpusHead" + struct.pack("<BBHIhB", 1, 1, 0, 24000, 0, 0),
        b"OpusTags" + struct.pack("<I", len(vendor)) + vendor + struct.pack("<I", 0),
    ]
    serial = int.from_bytes(os.urandom(4), "little")
    with path.open("wb") as output:
        output.write(ogg_page(headers[0], serial, 0, 0, 2))
        output.write(ogg_page(headers[1], serial, 1, 0))
        granule = 0
        for index, packet in enumerate(packets):
            granule += opus_samples_48k(packet)
            output.write(
                ogg_page(packet, serial, index + 2, granule, 4 if index == len(packets) - 1 else 0)
            )


def build_player_ogg(frames_file: Path, target: Path, tick_rate: float, keep_gaps: bool):
    state, packets, granule = {"rate": 24000, "sequence": 0}, [], 0
    with frames_file.open(encoding="ascii") as source:
        for line in source:
            if "|" not in line:
                continue
            tick_text, raw_hex = line.strip().split("|", 1)
            tick = int(tick_text)
            if keep_gaps:
                target_granule = round(tick / tick_rate * 48000)
                while granule + 960 <= target_granule:
                    packets.append(SILENCE_OPUS)
                    granule += 960
            for packet in container_packets(bytes.fromhex(raw_hex), state):
                packets.append(packet)
                granule += opus_samples_48k(packet)
    if not packets:
        raise ValueError("selected player has no valid Opus packets")
    write_ogg(target, packets)
    return target


HTML = r'''<!doctype html>
<html lang="ru">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>TF2 Demo Tools</title>
<style>
:root{color-scheme:dark;--bg:#0d1012;--panel:#171b1e;--panel2:#202529;--line:#343b40;--ink:#f4efe8;--muted:#9da6aa;--accent:#ef7f45;--accent2:#ffb36f;--ok:#67c69a}
*{box-sizing:border-box}body{margin:0;background:radial-gradient(circle at 20% -10%,#283038 0,transparent 36%),var(--bg);color:var(--ink);font:15px/1.45 "Segoe UI",system-ui,sans-serif;min-height:100vh}
button,input{font:inherit}button{border:0;cursor:pointer}.shell{width:min(1180px,calc(100% - 32px));margin:auto;padding:28px 0 56px}.top{display:flex;align-items:center;justify-content:space-between;margin-bottom:34px}.brand{font-weight:800;letter-spacing:.08em;font-size:14px}.brand i{display:inline-block;width:11px;height:11px;background:var(--accent);border-radius:3px;transform:rotate(45deg);margin-right:10px}.ghost{background:transparent;color:var(--muted);padding:9px 12px;border:1px solid var(--line);border-radius:10px}.hero{padding:54px 0 28px;max-width:720px}.eyebrow{color:var(--accent2);text-transform:uppercase;letter-spacing:.13em;font-size:12px;font-weight:700}.hero h1{font-size:clamp(38px,7vw,72px);line-height:.98;letter-spacing:-.055em;margin:14px 0 20px}.hero p{font-size:18px;color:var(--muted);max-width:590px}.drop{margin-top:26px;border:1px dashed #59636a;background:linear-gradient(145deg,#171c20,#121618);border-radius:20px;padding:52px 24px;text-align:center;transition:.2s}.drop.drag{border-color:var(--accent);background:#211b18;transform:translateY(-2px)}.drop strong{display:block;font-size:20px;margin-bottom:7px}.drop span{color:var(--muted)}.primary{background:var(--accent);color:#1b120e;font-weight:800;padding:12px 17px;border-radius:11px}.drop .primary{margin-top:20px}.hidden{display:none!important}.status{margin:18px 0;color:var(--muted);min-height:22px}.status.error{color:#ff8b7d}.workspace{display:grid;gap:16px}.demo-head,.panel{background:rgba(23,27,30,.94);border:1px solid var(--line);border-radius:16px}.demo-head{padding:20px}.demo-title{display:flex;align-items:flex-start;justify-content:space-between;gap:16px}.demo-title h2{font-size:22px;margin:0 0 4px;word-break:break-word}.sub{color:var(--muted)}.badges{display:flex;gap:7px;flex-wrap:wrap;margin-top:14px}.badge{background:var(--panel2);border:1px solid var(--line);border-radius:99px;padding:6px 9px;font-size:12px}.badge.accent{color:var(--accent2);border-color:#74432b}.stats{display:grid;grid-template-columns:repeat(5,1fr);gap:8px;margin-top:18px}.stat{background:#111517;border-radius:10px;padding:11px}.stat small{display:block;color:var(--muted);font-size:11px;text-transform:uppercase;letter-spacing:.08em}.stat b{display:block;margin-top:4px;font-size:14px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.panel{padding:20px}.panel h3{font-size:16px;margin:0}.panel-head{display:flex;justify-content:space-between;align-items:center;gap:12px;margin-bottom:16px}.timeline{position:relative;padding:28px 0 30px}.axis{position:absolute;left:0;right:0;display:flex;justify-content:space-between;color:#7f898e;font:11px ui-monospace,monospace}.axis.top{top:4px;margin:0}.axis.bottom{bottom:2px}.track{height:116px;background:#101416;border:1px solid var(--line);border-radius:10px;position:relative;overflow:hidden}canvas{width:100%;height:100%;display:block}.selection{position:absolute;top:0;bottom:0;background:rgba(239,127,69,.18);border-left:2px solid var(--accent);border-right:2px solid var(--accent);pointer-events:none}.ranges{position:absolute;inset:0;pointer-events:none}.range-mark{position:absolute;top:8px;height:7px;background:var(--ok);border-radius:10px}.slider{position:absolute;left:0;right:0;top:50%;width:100%;margin:0;appearance:none;background:transparent;pointer-events:none}.slider::-webkit-slider-thumb{appearance:none;width:18px;height:32px;border-radius:6px;background:var(--ink);border:5px solid var(--accent);pointer-events:auto;cursor:ew-resize}.editor-row{display:grid;grid-template-columns:1fr 1fr auto auto;gap:10px;align-items:end}.field label{display:block;color:var(--muted);font-size:12px;margin-bottom:5px}.field input{width:100%;background:#101416;color:var(--ink);border:1px solid var(--line);border-radius:9px;padding:10px}.secondary{background:var(--panel2);border:1px solid var(--line);color:var(--ink);padding:11px 14px;border-radius:10px}.segments{display:flex;gap:8px;flex-wrap:wrap;margin-top:15px}.segment{background:#101416;border:1px solid #345b4a;color:#b8e7d0;padding:7px 9px;border-radius:9px}.segment button{color:#90a099;background:none;margin-left:8px}.grid{display:grid;grid-template-columns:1.15fr .85fr;gap:16px}.players{display:grid;gap:7px;max-height:265px;overflow:auto;margin:14px 0}.player{display:grid;grid-template-columns:auto 1fr auto;align-items:center;gap:10px;background:#111517;padding:9px 10px;border-radius:9px}.player small{color:var(--muted)}.switch{display:flex;align-items:center;gap:8px;color:var(--muted);margin:12px 0}.actions{display:flex;gap:9px;flex-wrap:wrap}.note{color:var(--muted);font-size:12px;margin-top:12px}.spinner{width:17px;height:17px;border:2px solid #ffffff55;border-top-color:white;border-radius:50%;display:inline-block;vertical-align:-3px;animation:spin .7s linear infinite}@keyframes spin{to{transform:rotate(360deg)}}
@media(max-width:760px){.shell{width:min(100% - 20px,1180px);padding-top:18px}.hero{padding-top:30px}.stats{grid-template-columns:1fr 1fr}.grid{grid-template-columns:1fr}.editor-row{grid-template-columns:1fr 1fr}.track{height:96px}.demo-title{display:block}.demo-title .ghost{margin-top:12px}.axis span:nth-child(even){display:none}}
:root{color-scheme:light;--bg:#f9f9ff;--panel:#fff;--panel2:#edf1f9;--line:#c4c6d0;--ink:#1a1b20;--muted:#45464f;--accent:#415f91;--accent2:#284777;--ok:#356859;--surface:#f9f9ff;--surface-high:#e1e2e9;--on-accent:#fff;--accent-soft:#d6e3ff;--error:#ba1a1a;--event-death:#ba1a1a;--event-spawn:#287d5a;--event-chat:#00639b;--event-round:#76558f;--event-change:#8b5000;--shadow:0 1px 2px #00000018,0 2px 8px #0000000d}
[data-theme=dark]{color-scheme:dark;--bg:#111318;--panel:#191c20;--panel2:#23262d;--line:#44474f;--ink:#e2e2e9;--muted:#c4c6d0;--accent:#aac7ff;--accent2:#d6e3ff;--ok:#83d5bb;--surface:#111318;--surface-high:#2a2d33;--on-accent:#0a305f;--accent-soft:#284777;--error:#ffb4ab;--event-death:#ffb4ab;--event-spawn:#83d5bb;--event-chat:#8fcdff;--event-round:#ddb8f6;--event-change:#ffb86f;--shadow:none}
@media(prefers-color-scheme:dark){:root:not([data-theme]){color-scheme:dark;--bg:#111318;--panel:#191c20;--panel2:#23262d;--line:#44474f;--ink:#e2e2e9;--muted:#c4c6d0;--accent:#aac7ff;--accent2:#d6e3ff;--ok:#83d5bb;--surface:#111318;--surface-high:#2a2d33;--on-accent:#0a305f;--accent-soft:#284777;--error:#ffb4ab;--event-death:#ffb4ab;--event-spawn:#83d5bb;--event-chat:#8fcdff;--event-round:#ddb8f6;--event-change:#ffb86f;--shadow:none}}
body{background:var(--bg);color:var(--ink);letter-spacing:.005em}.shell{width:min(1160px,calc(100% - 32px));padding-top:18px}.top{margin-bottom:18px;min-height:48px}.brand{letter-spacing:.06em}.brand i{background:var(--accent);border-radius:50%;transform:none}.top-actions{display:flex;align-items:center;gap:8px;flex-wrap:wrap;justify-content:flex-end}.control{font:inherit;background:var(--panel2);color:var(--ink);border:0;border-bottom:1px solid var(--muted);border-radius:8px 8px 0 0;padding:9px 30px 9px 11px;min-width:118px}.hero{padding:40px 0 20px;max-width:760px}.eyebrow{color:var(--accent);letter-spacing:.09em}.hero h1{font-size:clamp(36px,6vw,58px);line-height:1.05;letter-spacing:-.035em;margin:12px 0 16px;font-weight:650}.hero p{font-size:18px}.drop{border:2px dashed var(--line);background:var(--panel2);border-radius:16px;padding:44px 24px;box-shadow:none}.drop.drag{border-color:var(--accent);background:var(--accent-soft);transform:none}.primary{background:var(--accent);color:var(--on-accent);font-weight:650;border-radius:20px;padding:11px 19px}.secondary{background:var(--accent-soft);border:0;color:var(--accent2);border-radius:20px;font-weight:600}.ghost{border:1px solid var(--line);border-radius:20px;color:var(--accent);background:transparent}.demo-head,.panel{background:var(--panel);border:1px solid var(--line);border-radius:12px;box-shadow:var(--shadow)}.badge{background:var(--panel2);border:0}.badge.accent{color:var(--accent2);border:0;background:var(--accent-soft)}.stat,.player{background:var(--panel2);border-radius:8px}.track{height:132px;background:var(--panel2);border:1px solid var(--line);border-radius:8px;overflow:hidden}.selection{background:color-mix(in srgb,var(--accent) 16%,transparent);border-color:var(--accent)}.range-mark{background:var(--ok)}.slider::-webkit-slider-thumb{background:var(--panel);border:4px solid var(--accent);border-radius:50%}.field input{background:var(--panel);color:var(--ink);border:1px solid var(--line);border-radius:4px 4px 0 0}.segment{background:var(--accent-soft);border:0;color:var(--accent2);border-radius:18px}.segment button{color:var(--accent2)}.status.error{color:var(--error)}.event-legend{display:flex;gap:12px;flex-wrap:wrap;margin:-4px 0 10px;color:var(--muted);font-size:12px}.event-legend span{display:inline-flex;align-items:center;gap:6px}.event-legend i{width:8px;height:8px;border-radius:50%;background:var(--dot)}.event-tip{position:absolute;z-index:6;display:none;max-width:min(330px,80%);padding:10px 12px;background:var(--surface-high);color:var(--ink);border-radius:8px;box-shadow:0 4px 18px #0004;pointer-events:none;font-size:13px}.event-tip b,.event-tip small{display:block}.event-tip small{color:var(--muted);margin:2px 0 5px}.event-tip p{margin:0;overflow-wrap:anywhere}.audio-options{display:grid;grid-template-columns:1fr;gap:10px;margin:12px 0}.audio-options .control{width:100%}.sr-only{position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip:rect(0,0,0,0);white-space:nowrap;border:0}.note.warn{color:var(--event-change)}
@media(max-width:760px){.top{align-items:flex-start}.top-actions{max-width:70%}.control{min-width:105px}.hero{padding-top:24px}.hero h1{font-size:38px}.track{height:112px}.axis span:nth-child(even){display:none}}
.choice{display:inline-flex;align-items:center;gap:2px;padding:3px;border:1px solid var(--line);border-radius:18px;background:var(--panel)}.choice button{width:28px;height:28px;padding:0;border-radius:50%;background:transparent;color:var(--muted);font-size:16px;line-height:1}.choice button[aria-pressed=true]{background:var(--accent-soft);color:var(--accent2);box-shadow:inset 0 0 0 1px var(--accent)}.field label small{color:var(--accent);font:11px ui-monospace,monospace;margin-left:4px}.timeline{padding:26px 0 30px}.axis{z-index:2;pointer-events:none}.axis.top{top:1px;padding:0 2px}.axis.bottom{bottom:2px}.track{z-index:0}.track canvas{position:relative;z-index:0}.ranges{z-index:2}.selection{z-index:3;background:color-mix(in srgb,var(--accent) 8%,transparent);box-shadow:-100vmax 0 0 100vmax #0005}.slider{z-index:4}.event-tip{z-index:6}
/* Timeline follows the compact editor reference: density stays visible; only unselected time is dimmed. */
:root{--timeline:#ed7d45;--timeline-light:#ffb087;--timeline-ok:#67c69a}.language-select{min-width:158px}.track{height:116px;background:#171d20;border-color:var(--line)}.selection{background:transparent;border-color:var(--timeline);box-shadow:-100vmax 0 0 100vmax #0006}.range-mark{top:8px;height:7px;background:var(--timeline-ok);border-radius:0}.slider::-webkit-slider-thumb{width:17px;height:32px;border:5px solid var(--timeline-light);border-radius:5px;background:var(--timeline);box-shadow:0 0 0 1px #9a4a2a}.montage-inline{margin-top:18px;padding-top:16px;border-top:1px solid var(--line)}.montage-inline .panel-head{margin-bottom:0}.montage-inline .actions{margin-top:12px}.segment{background:transparent;border:1px solid var(--ok);color:var(--ok);border-radius:8px}.segment button{color:var(--ok)}
/* Compact material layout adapted from the supplied reference. */
.demo-head,.panel{border-radius:10px}.demo-head,.panel{padding:18px}.stats{grid-template-columns:repeat(auto-fit,minmax(128px,1fr));gap:10px}.stat{padding:10px 11px;min-width:0}.stat b{font-size:13px}.event-tip{position:fixed}.pov-controls{display:grid;grid-template-columns:minmax(160px,1fr) auto;gap:8px 14px;align-items:center;margin:4px 0 13px;padding:10px;background:var(--panel2);border-radius:8px}.pov-controls>label:first-child{display:grid;gap:5px;color:var(--muted);font-size:12px}.pov-controls .switch{margin:20px 0 0}.pov-controls small{grid-column:1/-1;font-size:12px}.event-legend{min-height:18px}.track canvas{cursor:crosshair}@media(max-width:760px){.pov-controls{grid-template-columns:1fr}.pov-controls .switch{margin:0}.stats{grid-template-columns:1fr 1fr}}
/* Supplied compact editor layout: the editor stays right of player voices. */
:root{color-scheme:light;--bg:#f7f6f3;--panel:#fff;--panel2:#fbfaf8;--line:#e6e3dc;--line-strong:#d6d2c7;--ink:#232220;--muted:#6b6a63;--accent:#cf6a35;--accent2:#8a3f1c;--ok:#1f8f6f;--surface-high:#fbfaf8;--on-accent:#fff;--accent-soft:#fbeee5;--event-death:#c0463d;--event-spawn:#8461c9;--event-chat:#3d6fa8;--event-round:#c98a1f;--event-change:#1f8f6f;--timeline:#8a5a3c;--timeline-light:#a87552;--timeline-ok:#1f8f6f;--shadow:none}
[data-theme=dark]{color-scheme:dark;--bg:#17181a;--panel:#1e1f22;--panel2:#232427;--line:#2c2d31;--line-strong:#3a3b40;--ink:#eceae6;--muted:#a4a29c;--accent:#cf6a35;--accent2:#e8a578;--ok:#3fb38a;--surface-high:#232427;--on-accent:#fff;--accent-soft:#3a2a1f;--event-death:#df726a;--event-spawn:#a988e4;--event-chat:#6d9bd0;--event-round:#e1ad49;--event-change:#56b999;--timeline:#8a5a3c;--timeline-light:#a87552;--timeline-ok:#3fb38a;--shadow:none}
@media(prefers-color-scheme:dark){:root:not([data-theme]){color-scheme:dark;--bg:#17181a;--panel:#1e1f22;--panel2:#232427;--line:#2c2d31;--line-strong:#3a3b40;--ink:#eceae6;--muted:#a4a29c;--accent:#cf6a35;--accent2:#e8a578;--ok:#3fb38a;--surface-high:#232427;--on-accent:#fff;--accent-soft:#3a2a1f;--event-death:#df726a;--event-spawn:#a988e4;--event-chat:#6d9bd0;--event-round:#e1ad49;--event-change:#56b999;--timeline:#8a5a3c;--timeline-light:#a87552;--timeline-ok:#3fb38a;--shadow:none}}
body{background:var(--bg);font-size:14px;letter-spacing:0}.shell{width:min(1180px,calc(100% - 48px));padding:24px 0 60px}.top{margin-bottom:6px;min-height:34px}.brand{font-weight:600;letter-spacing:.02em}.brand i{width:8px;height:8px;margin-right:10px;border-radius:50%;background:var(--accent)}.top-actions{gap:10px}.language-select{min-width:144px;padding:7px 28px 7px 12px;border:1px solid var(--line);border-radius:8px;background:var(--panel);font-size:13px}.choice{padding:4px;gap:3px;border-radius:10px;background:var(--panel);border-color:var(--line)}.choice button{width:38px;height:34px;border-radius:7px;font-size:16px}.choice button[aria-pressed=true]{background:var(--accent-soft);color:var(--accent2);box-shadow:none}.ghost{padding:7px 14px;border-color:var(--line-strong);border-radius:8px;color:var(--ink);background:var(--panel);font-size:13px}.primary{padding:7px 14px;border:1px solid var(--accent);border-radius:8px;background:var(--accent);color:#fff;font-weight:600;font-size:13px}.secondary{padding:7px 14px;border:1px solid var(--line-strong);border-radius:8px;background:var(--panel);color:var(--ink);font-size:13px}.hero{padding:54px 0 28px}.drop{border-color:var(--line-strong);border-radius:12px;background:var(--panel);box-shadow:none}.workspace{gap:18px}.demo-head,.panel{border-radius:12px;border-color:var(--line);box-shadow:none}.demo-head.info-card{padding:20px 22px;margin:0}.demo-title h2{font-size:19px;font-weight:600;margin-bottom:6px}.sub{color:var(--muted)}.badges{gap:8px;margin:14px 0 16px}.badge{padding:5px 11px;border:1px solid var(--line);border-radius:6px;background:var(--panel2);color:var(--muted);font-size:12px}.badge.accent{background:var(--accent);border-color:var(--accent);color:#fff;font-weight:500}.stats.metrics-grid{grid-template-columns:repeat(auto-fit,minmax(130px,1fr));gap:10px;margin-top:0}.stat{padding:12px 14px;border-radius:8px;background:var(--panel2)}.stat small{font-size:11px;color:var(--muted);letter-spacing:.05em}.stat b{margin-top:6px;font-family:ui-monospace,Consolas,monospace;font-size:16px;font-weight:600}.layout{display:grid;grid-template-columns:248px minmax(0,1fr);gap:18px}.sidebar{padding:0;align-self:start}.players-card{padding:14px 14px 16px}.players-card .panel-head{margin:0 2px 10px}.players-card .panel-head h3{font-size:12px;text-transform:uppercase;letter-spacing:.05em;color:var(--muted)}.players-card .panel-head .ghost{padding:5px 8px;font-size:11px}.search-mini{margin-bottom:8px}.search-mini input{width:100%;padding:6px 10px;border:1px solid var(--line);border-radius:6px;background:var(--panel2);color:var(--ink);font-size:12.5px}.players{display:grid;gap:0;max-height:300px;margin:0;overflow:auto}.player{grid-template-columns:auto minmax(0,1fr) auto;gap:10px;padding:7px 6px;border-radius:6px;background:transparent}.player:hover{background:var(--panel2)}.player input{accent-color:var(--accent);width:14px;height:14px}.player .info{display:grid;min-width:0;gap:1px}.player .name{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-size:13px}.player small{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font:10.5px ui-monospace,Consolas,monospace;color:var(--muted)}.player .mic{opacity:.35}.player input:checked~.mic{opacity:1;color:var(--accent)}.pov-controls{grid-template-columns:1fr;margin:10px 0;padding:10px;border-radius:8px;background:var(--panel2)}.pov-controls .switch{margin:0}.pov-controls>label:first-child{font-size:11.5px}.control{min-width:0;border:1px solid var(--line);border-radius:6px;background:var(--panel2);padding:6px 9px;color:var(--ink);font-size:12.5px}.audio-options{gap:8px;margin:10px 0}.switch{margin:0;font-size:12px}.players-card .actions{display:grid;gap:8px}.players-card .actions button{justify-content:center}.players-card .actions .primary{background:var(--accent-soft);color:var(--accent2);border-color:transparent}.players-card .actions .ghost{font-size:12px}.note{font-size:11.5px;margin-top:10px}.editor{padding:20px 22px}.editor>.panel-head{margin-bottom:12px}.editor>.panel-head h3{font-size:13px}.editor>.panel-head .sub{font-size:12px}.event-legend{gap:14px;margin:0 0 10px;min-height:0;font-size:11.5px}.event-legend i{border-radius:2px}.timeline-wrap{position:relative;padding:10px 12px 0;border:1px solid var(--line);border-radius:8px;background:var(--panel2)}.timeline{padding:22px 0 28px}.axis{font-size:11px;color:var(--muted)}.axis.top{top:0;padding:0}.axis.bottom{bottom:0}.track{height:132px;border:0;border-radius:2px;background:#17181a}.selection{border-color:#000;box-shadow:-100vmax 0 0 100vmax #0007}.range-mark{top:0;height:10px;border-radius:2px;background:var(--timeline-ok)}.slider::-webkit-slider-thumb{width:14px;height:32px;border:0;border-radius:7px;background:var(--accent);box-shadow:0 1px 3px #0008}.editor-row.range-controls{display:flex;align-items:flex-end;gap:10px;margin:20px 0;flex-wrap:wrap}.range-field label{font-size:11.5px;margin-bottom:5px}.range-field label small{font:11px ui-monospace,Consolas,monospace;color:var(--muted);margin-left:4px}.range-field input{width:120px;padding:7px 9px;border:1px solid var(--line);border-radius:6px;background:var(--panel2);font:12.5px ui-monospace,Consolas,monospace}.range-controls .sep{flex:1;display:block;min-width:20px;height:1px;margin-bottom:12px;background:var(--line)}.editor-actions{display:flex;gap:8px;align-items:center}.clips-section{border-top:1px solid var(--line);padding-top:16px}.clips-section .panel-head{margin-bottom:12px}.clips-section .panel-head h3{font-size:13px}.segments{display:grid;gap:8px;margin:0}.segment{display:flex;align-items:center;gap:14px;min-width:0;padding:12px 14px;border:1px solid var(--line);border-radius:8px;background:var(--panel2);color:var(--ink);font:500 13px ui-monospace,Consolas,monospace}.segment .clip-meta{display:flex;gap:14px;flex:1;flex-wrap:wrap;color:var(--muted);font-size:11.5px}.segment .clip-meta b{color:var(--ink);font-weight:400}.segment button{margin-left:auto;padding:5px 10px;border:1px solid var(--line-strong);border-radius:6px;background:var(--panel);color:var(--ink);font:12px inherit}.montage-inline .actions{margin-top:12px}.event-tip{padding:6px 10px;border:1px solid var(--line-strong);border-radius:6px;background:var(--panel);box-shadow:0 6px 18px #0004;font-size:11.5px}.event-tip small{font:11px ui-monospace,Consolas,monospace;color:var(--muted)}
@media(max-width:760px){.shell{width:min(100% - 24px,1180px);padding-top:18px}.top{align-items:flex-start}.top-actions{gap:6px;max-width:72%}.language-select{min-width:0;width:128px}.layout{grid-template-columns:1fr}.sidebar{order:2}.editor{padding:16px}.track{height:112px}.axis span:nth-child(even){display:none}.range-controls .sep{display:none}.range-field{flex:1}.range-field input{width:100%}.editor-actions{width:100%}.segment{align-items:flex-start;flex-wrap:wrap}.segment button{margin-left:0}}
.selection{box-shadow:-100vmax 0 0 100vmax #0009,100vmax 0 0 100vmax #0009;pointer-events:auto;cursor:grab;touch-action:none}.selection.is-dragging{cursor:grabbing}.audio-format{display:grid;gap:4px;color:var(--muted);font-size:11.5px}.chat-history{margin:-2px 0 10px;padding:7px 9px;border:1px solid var(--line);border-radius:6px;background:var(--panel2);font-size:11.5px}.chat-history summary{cursor:pointer;color:var(--ink)}.chat-history[open] summary{margin-bottom:6px}.chat-row{display:grid;grid-template-columns:74px 1fr;gap:8px;padding:3px 0;border-top:1px solid var(--line);color:var(--muted)}.chat-row time{font:11px ui-monospace,Consolas,monospace;color:var(--accent2)}.chat-row b{color:var(--ink);font-weight:500}.output-timeline{margin:0 0 18px;padding:19px 0 21px;position:relative}.output-track{display:flex;min-height:58px;overflow:hidden;border:1px solid var(--line-strong);border-radius:8px;background:var(--panel2)}.output-clip{position:relative;min-width:44px;display:flex;flex-direction:column;justify-content:center;gap:2px;padding:6px 10px;border-right:1px solid #0004;background:var(--clip,var(--accent));color:#fff;font:11px ui-monospace,Consolas,monospace;white-space:nowrap;overflow:hidden;cursor:grab}.output-clip:active{cursor:grabbing}.output-clip:last-child{border-right:0}.output-clip b{color:#fff;font-size:12px}.output-axis{position:absolute;left:0;right:0;color:var(--muted);font:10.5px ui-monospace,Consolas,monospace;pointer-events:none}.output-axis span{position:absolute;white-space:nowrap}.output-axis.top{top:0}.output-axis.bottom{bottom:0}.segment{cursor:grab}.segment.dragging{opacity:.45}.segment.drop-target{outline:2px solid var(--accent);outline-offset:1px}.drag-handle{color:var(--muted);font:16px/1 sans-serif;cursor:grab}.segment .clip-order{min-width:24px;color:var(--accent2);font-weight:700}.segment .clip-range{min-width:110px}.segment button{cursor:pointer}.language-menu{position:relative}.language-button{display:flex;align-items:center;gap:8px;padding:7px 10px;border:1px solid var(--line);border-radius:8px;background:var(--panel);color:var(--ink);font-size:13px;white-space:nowrap}.language-button:hover{border-color:var(--line-strong)}.language-button svg{transition:transform .15s}.language-menu.open .language-button svg{transform:rotate(180deg)}.flag{font-size:16px;line-height:1}.language-options{position:absolute;top:calc(100% + 6px);right:0;z-index:12;display:none;min-width:166px;padding:4px;border:1px solid var(--line);border-radius:8px;background:var(--panel);box-shadow:0 8px 24px #0003}.language-menu.open .language-options{display:grid}.language-options button{display:flex;align-items:center;gap:8px;width:100%;padding:7px 8px;border-radius:6px;background:transparent;color:var(--ink);text-align:left;font-size:13px}.language-options button:hover,.language-options button[aria-checked=true]{background:var(--panel2);color:var(--accent2)}@media(max-width:760px){.language-button{padding:7px 8px}.language-button span:not(.flag){display:none}.language-options button span:not(.flag){display:inline}.output-clip{font-size:10px}.segment .clip-range{min-width:0}.drag-handle{order:-1}}
.flag{display:inline-block;width:18px;height:12px;flex:0 0 18px;overflow:hidden;border:1px solid #0003;border-radius:2px;box-shadow:0 0 0 1px #fff1}.flag-ru{background:linear-gradient(to bottom,#fff 0 33.33%,#1d57a6 33.33% 66.66%,#d32932 66.66%)}.flag-en{background:#012169;background-image:linear-gradient(33deg,transparent 42%,#fff 43% 48%,#c8102e 49% 53%,#fff 54% 58%,transparent 59%),linear-gradient(-33deg,transparent 42%,#fff 43% 48%,#c8102e 49% 53%,#fff 54% 58%,transparent 59%),linear-gradient(to bottom,transparent 36%,#fff 36% 64%,transparent 64%),linear-gradient(to right,transparent 40%,#fff 40% 60%,transparent 60%),linear-gradient(to bottom,transparent 43%,#c8102e 43% 57%,transparent 57%),linear-gradient(to right,transparent 44%,#c8102e 44% 56%,transparent 56%)}.output-timeline{margin-bottom:24px;padding-bottom:30px}.output-clip.dragging{opacity:.45}.player-tabs{display:flex;gap:2px;padding:2px;border:1px solid var(--line);border-radius:7px;background:var(--panel2)}.player-tabs button{padding:4px 6px;border-radius:5px;background:transparent;color:var(--muted);font-size:11px;white-space:nowrap}.player-tabs button[aria-selected=true]{background:var(--panel);color:var(--ink);box-shadow:var(--shadow)}
</style></head>
<body><main class="shell">
<header class="top"><div class="brand"><i></i>TF2 DEMO TOOLS</div><div class="top-actions"><div id="languageMenu" class="language-menu"><button id="languageButton" class="language-button" type="button" aria-haspopup="menu" aria-expanded="false"><span id="languageFlag" class="flag flag-ru" aria-hidden="true"></span><span id="languageLabel">Русский</span><svg aria-hidden="true" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="m6 9 6 6 6-6"/></svg></button><div id="languageOptions" class="language-options" role="menu"><button type="button" value="system" role="menuitemradio"><span id="systemFlag" class="flag flag-ru" aria-hidden="true"></span><span data-t="systemLanguage">Язык системы</span></button><button type="button" value="ru" role="menuitemradio"><span class="flag flag-ru" aria-hidden="true"></span><span>Русский</span></button><button type="button" value="en" role="menuitemradio"><span class="flag flag-en" aria-hidden="true"></span><span>English</span></button></div></div><div id="themeMenu" class="theme-menu"><button id="themeButton" class="theme-button" type="button" aria-haspopup="menu" aria-expanded="false" aria-label="Тема" title="Тема системы"><span id="themeIcon" aria-hidden="true">◐</span></button><div id="theme" class="theme-options" role="menu" aria-label="Тема"><button type="button" value="system" role="menuitemradio" data-t-title="systemTheme" title="Тема системы" aria-label="Тема системы"><span aria-hidden="true">◐</span></button><button type="button" value="light" role="menuitemradio" data-t-title="lightTheme" title="Светлая" aria-label="Светлая"><span aria-hidden="true">☀</span></button><button type="button" value="dark" role="menuitemradio" data-t-title="darkTheme" title="Тёмная" aria-label="Тёмная"><span aria-hidden="true">☾</span></button></div></div><button id="reset" class="ghost hidden" data-t="otherDemo">Другая демка</button></div></header>
<section id="intro"><div class="hero"><div class="eyebrow" data-t="eyebrow">Локальный редактор</div><h1 id="heroTitle">Монтаж TF2-демок<br>без лишних шагов.</h1><p data-t="heroText">POV и SourceTV, события, точная шкала тиков, монтаж отрезков и голоса игроков. Файл обрабатывается локально.</p></div>
<div id="drop" class="drop" tabindex="0"><strong data-t="dropTitle">Перетащите сюда .dem</strong><span data-t="dropText">или выберите файл с диска</span><br><button id="pick" class="primary" data-t="pickDemo">Выбрать демку</button><input id="file" type="file" accept=".dem" hidden></div></section>
<div id="status" class="status" role="status" aria-live="polite"></div>
<section id="workspace" class="workspace hidden">
<div class="demo-head info-card"><div class="demo-title"><div><h2 id="demoName"></h2><div id="demoSub" class="sub"></div></div></div><div id="badges" class="badges"></div><div id="stats" class="stats metrics-grid"></div></div>
<div class="layout"><aside class="sidebar panel"><div class="players-card"><div class="panel-head"><div id="playerTabs" class="player-tabs" role="tablist"><button type="button" value="voices" role="tab" aria-selected="true" data-t="voicesTitle">Голоса игроков</button><button type="button" value="all" role="tab" aria-selected="false" data-t="demoPlayers">Игроки в демке</button></div><button id="toggleAll" class="ghost" data-t="selectAll">Выбрать всех</button></div><div class="search-mini"><input id="playerSearch" type="search" data-t-placeholder="findPlayer" placeholder="Найти игрока"></div><div id="players" class="players"><span class="sub" data-t="loadingVoices">Ищу голосовые дорожки…</span></div><div id="voiceOnly"><div class="audio-options"><label class="switch"><input id="keepGaps" type="checkbox" checked> <span data-t="keepGaps">Оставить паузы как в демке</span></label><label class="audio-format"><span data-t="audioFormat">Формат звука</span><select id="audioFormat" class="control"><option value="ogg">OGG · Opus</option><option value="wav">WAV · PCM</option><option value="mp3">MP3 · 128 kbps</option></select></label></div><div class="actions"><button id="downloadVoices" class="primary" data-t="downloadAudio">Скачать аудио</button><button id="downloadAllVoices" class="ghost" data-t="downloadArchive">Скачать всех .zip</button></div><div id="audioNote" class="note"></div></div></div></aside>
<div class="panel editor"><div class="panel-head"><h3 data-t="timelineTitle">Дорожка демки</h3><span class="sub" data-t="timelineSub">пакетная плотность</span></div><div id="eventLegend" class="event-legend"></div><details id="chatHistory" class="chat-history hidden"><summary id="chatSummary"></summary><div id="chatRows"></div></details><div class="timeline-wrap"><div class="timeline"><div id="tickAxis" class="axis top"></div><div id="track" class="track"><canvas id="events"></canvas><div id="eventTip" class="event-tip" role="tooltip"></div><div id="savedRanges" class="ranges"></div><div id="selection" class="selection"></div><input id="startRange" class="slider" type="range"><input id="endRange" class="slider" type="range"></div><div id="timeAxis" class="axis bottom"></div></div></div>
<div class="editor-row range-controls"><div class="field range-field"><label for="startTime"><span data-t="startSeconds">Начало, сек</span><small id="startTick"></small></label><input id="startTime" type="number" min="0" step="0.01"></div><i class="sep"></i><div class="field range-field"><label for="endTime"><span data-t="endSeconds">Конец, сек</span><small id="endTick"></small></label><input id="endTime" type="number" min="0" step="0.01"></div><div class="editor-actions"><button id="addSegment" class="primary" data-t="addMontage">+ В монтаж</button><button id="clearSegments" class="ghost" data-t="clear">Очистить</button></div></div><div class="montage-inline clips-section"><div class="panel-head"><h3 data-t="montageTitle">Монтаж</h3><span id="montageSummary" class="sub"></span></div><div id="outputTimeline" class="output-timeline hidden"><div id="outputTickAxis" class="output-axis top"></div><div id="outputTrack" class="output-track"></div><div id="outputTimeAxis" class="output-axis bottom"></div></div><div id="segments" class="segments"></div><div class="actions"><button id="downloadMontage" class="primary" data-t="downloadMontage">Скачать монтаж .dem</button></div><div class="note" data-t="montageNote">Перетаскивайте отрезки в нужном порядке; монтаж повторит эту последовательность.</div></div></div></div>
</section></main>
<script src="/lame.min.js"></script><script>
const $=s=>document.querySelector(s), intro=$('#intro'), workspace=$('#workspace'), statusEl=$('#status');
const I18N={
ru:{languageLabel:'Язык',systemLanguage:'Язык системы',themeLabel:'Тема',systemTheme:'Тема системы',lightTheme:'Светлая',darkTheme:'Тёмная',otherDemo:'Другая демка',eyebrow:'Локальный редактор',heroTitle:'Монтаж TF2-демок<br>без лишних шагов.',heroText:'POV и SourceTV, точная шкала тиков, монтаж отрезков и голоса игроков. Файл обрабатывается локально.',dropTitle:'Перетащите сюда .dem',dropText:'или выберите файл с диска',pickDemo:'Выбрать демку',downloadClip:'Скачать отрезок',timelineTitle:'Дорожка демки',timelineSub:'пакетная плотность',startSeconds:'Начало, сек',endSeconds:'Конец, сек',addMontage:'+ В монтаж',clear:'Очистить',montageTitle:'Монтаж',montageText:'Добавьте несколько участков по времени. Пропуски будут безопасно промотаны, а POV-команды сохранятся.',downloadMontage:'Скачать монтаж .dem',montageNote:'Перетаскивайте отрезки в нужном порядке; монтаж повторит эту последовательность.',voicesTitle:'Голоса игроков',findPlayer:'Найти игрока',selectAll:'Выбрать всех',loadingVoices:'Ищу голосовые дорожки…',keepGaps:'Оставить паузы как в демке',audioFormat:'Формат звука',downloadAudio:'Скачать аудио',needDemo:'Нужен файл .dem',uploading:'Загружаю {name}…',ready:'Демка готова к монтажу.',fileReady:'Файл готов.',preparing:'Готовлю…',serverUnknown:'Сервер не указан',clientUnknown:'клиент не указан',restoredStop:'восстановлен dem_stop',duration:'Длительность',ticks:'Тики',tickrate:'Tickrate',frames:'Кадры',size:'Размер',noSegments:'Нет отрезков',segments:'{count} отр. · {time}',remove:'Удалить',noVoices:'Голосовых пакетов нет.',packets:'{count} пак.',selectPlayer:'Выберите хотя бы одного игрока.',addFirst:'Сначала добавьте отрезки в монтаж.',eventsLoading:'События загружаются…',noEvents:'В демке не найдено отображаемых событий',eventDeath:'Убийство',eventSpawn:'Спавн',eventChat:'Чат',chatHistory:'Чат · {count}',eventRound:'Раунд',eventClass:'Смена класса',eventTeam:'Смена команды',eventChanges:'Команда / класс',roundStarted:'Раунд начался',roundWon:'Победа: {team}',eventAt:'{time} · тик {tick}',audioOgg:'OGG сохраняет исходный Opus без потерь. Несколько дорожек будут упакованы в ZIP.',audioWav:'WAV не сжимается: дорожки с паузами могут быть очень большими.',audioMp3:'MP3 кодируется локально в браузере с битрейтом 128 kbps.',converting:'Кодирую {current}/{total}…',audioReady:'Аудио готово.',audioError:'Не удалось декодировать голосовую дорожку: {error}'},
en:{languageLabel:'Language',systemLanguage:'System language',themeLabel:'Theme',systemTheme:'System theme',lightTheme:'Light',darkTheme:'Dark',otherDemo:'Another demo',eyebrow:'Local editor',heroTitle:'Edit TF2 demos<br>without extra steps.',heroText:'POV and SourceTV, an exact tick scale, montage cuts and player voice tracks. Processing stays local.',dropTitle:'Drop a .dem file here',dropText:'or choose a file from disk',pickDemo:'Choose demo',downloadClip:'Download clip',timelineTitle:'Demo timeline',timelineSub:'packet density',startSeconds:'Start, sec',endSeconds:'End, sec',addMontage:'+ Add to montage',clear:'Clear',montageTitle:'Montage',montageText:'Add several time ranges. Gaps are fast-forwarded safely and POV commands are preserved.',downloadMontage:'Download montage .dem',montageNote:'Drag ranges into the needed order; the montage uses that sequence.',voicesTitle:'Player voices',selectAll:'Select all',loadingVoices:'Looking for voice tracks…',keepGaps:'Keep pauses from the demo',audioFormat:'Audio format',downloadAudio:'Download audio',needDemo:'Please choose a .dem file',uploading:'Uploading {name}…',ready:'Demo is ready to edit.',fileReady:'File is ready.',preparing:'Preparing…',serverUnknown:'Server not specified',clientUnknown:'client not specified',restoredStop:'dem_stop restored',duration:'Duration',ticks:'Ticks',tickrate:'Tickrate',frames:'Frames',size:'Size',noSegments:'No ranges',segments:'{count} ranges · {time}',remove:'Remove',noVoices:'No voice packets found.',packets:'{count} packets',selectPlayer:'Select at least one player.',addFirst:'Add at least one range first.',eventsLoading:'Loading events…',noEvents:'No supported events were found in this demo',eventDeath:'Kill',eventSpawn:'Spawn',eventChat:'Chat',chatHistory:'Chat · {count}',eventRound:'Round',eventClass:'Class change',eventTeam:'Team change',eventChanges:'Team / class',roundStarted:'Round started',roundWon:'Winner: {team}',eventAt:'{time} · tick {tick}',audioOgg:'OGG keeps the original Opus stream without re-encoding. Multiple tracks are packed into ZIP.',audioWav:'WAV is uncompressed: tracks with pauses can be very large.',audioMp3:'MP3 is encoded locally in the browser at 128 kbps.',converting:'Encoding {current}/{total}…',audioReady:'Audio is ready.',audioError:'Could not decode the voice track: {error}'}
};
I18N.ru.ticksShort='тики';I18N.en.ticksShort='ticks';I18N.ru.downloadArchive='Скачать всех .zip';I18N.en.downloadArchive='Download all .zip';I18N.ru.map='Карта';I18N.en.map='Map';I18N.ru.server='Сервер';I18N.en.server='Server';I18N.en.findPlayer='Find player';I18N.ru.demoPlayers='Игроки в демке';I18N.en.demoPlayers='Demo players';
let language='ru',languageSetting='system',state={id:null,meta:null,ranges:[],players:[],allPlayers:[],playerTab:'voices',events:[],eventsLoaded:false,hit:[],selectionReady:false};
const tr=(key,values={})=>Object.entries(values).reduce((text,[name,value])=>text.replace(`{${name}}`,value),I18N[language][key]||key);
const locale=()=>language==='ru'?'ru-RU':'en-US';
const fmt=s=>{const hundredths=Math.round(Math.max(0,s)*100),m=Math.floor(hundredths/6000),x=hundredths-m*6000;return `${m}:${(x/100).toFixed(2).padStart(5,'0')}`};
const esc=s=>String(s??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const choiceValue=id=>$('#'+id).querySelector('button[aria-checked=true]')?.value||'system';
function setChoice(id,value){$('#'+id).querySelectorAll('button').forEach(button=>button.setAttribute('aria-checked',button.value===value))}
function applyLanguage(value){languageSetting=value;const systemLanguage=navigator.language.toLowerCase().startsWith('ru')?'ru':'en';language=value==='system'?systemLanguage:value;const flagClass=code=>'flag flag-'+code;document.documentElement.lang=language;document.querySelectorAll('[data-t]').forEach(el=>el.textContent=tr(el.dataset.t));document.querySelectorAll('[data-t-title]').forEach(el=>{el.title=tr(el.dataset.tTitle);el.ariaLabel=el.title});document.querySelectorAll('[data-t-placeholder]').forEach(el=>el.placeholder=tr(el.dataset.tPlaceholder));$('#heroTitle').innerHTML=tr('heroTitle');$('#languageFlag').className=flagClass(value==='system'?systemLanguage:value);$('#systemFlag').className=flagClass(systemLanguage);$('#languageLabel').textContent=value==='system'?tr('systemLanguage'):value==='ru'?'Русский':'English';$('#languageOptions').querySelectorAll('button[value]').forEach(button=>button.setAttribute('aria-checked',button.value===value));localStorage.setItem('demoToolsLanguage',value);$('#theme').ariaLabel=tr('themeLabel');$('#themeButton').ariaLabel=tr('themeLabel');const themeValue=choiceValue('theme');$('#themeButton').title=tr(themeValue==='light'?'lightTheme':themeValue==='dark'?'darkTheme':'systemTheme');if(state.meta){render();renderSegments();renderPlayers();renderLegend();renderChat();draw();say(tr('ready'))}updateAudioNote()}
function applyTheme(value){if(value==='system')document.documentElement.removeAttribute('data-theme');else document.documentElement.dataset.theme=value;setChoice('theme',value);$('#themeIcon').textContent=value==='light'?'☀':value==='dark'?'☾':'◐';$('#themeButton').title=tr(value==='light'?'lightTheme':value==='dark'?'darkTheme':'systemTheme');localStorage.setItem('demoToolsTheme',value);if(state.meta)draw()}
function say(text,error=false){statusEl.textContent=text;statusEl.className='status'+(error?' error':'')}
function filename(res){const h=res.headers.get('content-disposition')||'';const m=h.match(/filename\*=UTF-8''([^;]+)/i);return m?decodeURIComponent(m[1]):'download'}
function downloadBlob(blob,name){const a=document.createElement('a');a.href=URL.createObjectURL(blob);a.download=name;a.click();setTimeout(()=>URL.revokeObjectURL(a.href),10000)}
async function post(url,data,button){const old=button?.innerHTML;if(button){button.disabled=true;button.innerHTML=`<i class="spinner"></i> ${tr('preparing')}`}try{const r=await fetch(url,{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(data)});if(!r.ok)throw new Error((await r.json()).error||r.statusText);downloadBlob(await r.blob(),filename(r));say(tr('fileReady'))}catch(e){say(e.message,true)}finally{if(button){button.disabled=false;button.innerHTML=old}}}
function saveSession(){if(state.id)localStorage.setItem('tf2DemoToolsSession',JSON.stringify({id:state.id,ranges:state.ranges,selection:[+$('#startRange').value||0,+$('#endRange').value||state.meta.ticks]}))}
async function upload(file){if(!file||!file.name.toLowerCase().endsWith('.dem'))return say(tr('needDemo'),true);say(tr('uploading',{name:file.name}));try{const r=await fetch('/api/upload?name='+encodeURIComponent(file.name),{method:'POST',body:file});const data=await r.json();if(!r.ok)throw new Error(data.error);state={id:data.id,meta:data.meta,ranges:[],players:[],allPlayers:[],playerTab:'voices',events:[],eventsLoaded:false,hit:[],selectionReady:false};render();saveSession();intro.classList.add('hidden');workspace.classList.remove('hidden');$('#reset').classList.remove('hidden');say(tr('ready'));loadVoices()}catch(e){say(e.message,true)}}
function axis(max,formatter){return [0,.25,.5,.75,1].map(x=>`<span>${formatter(max*x)}</span>`).join('')}
const EVENT_STYLE={death:['eventDeath','--event-death',.68],spawn:['eventSpawn','--event-spawn',.42],chat:['eventChat','--event-chat',.5],round_start:['eventRound','--event-round',.9],round_win:['eventRound','--event-round',.9],class:['eventClass','--event-change',.58],team:['eventTeam','--event-change',.58]};
function eventText(event){if(event.kind==='death')return `${event.actor} → ${event.target}${event.detail?' · '+event.detail:''}`;if(event.kind==='chat')return `${event.actor}: ${event.detail}`;if(event.kind==='round_start')return tr('roundStarted');if(event.kind==='round_win')return tr('roundWon',{team:event.detail});return [event.actor,event.detail].filter(Boolean).join(' · ')}
function renderLegend(){const groups={};state.events.forEach(event=>{const style=EVENT_STYLE[event.kind];if(style){const key=style[0];groups[key]??={style,count:0};groups[key].count++}});$('#eventLegend').innerHTML=Object.values(groups).map(({style,count})=>`<span style="--dot:var(${style[1]})"><i></i>${tr(style[0])} · ${count}</span>`).join('')}
function renderChat(){const chat=state.events.filter(event=>event.kind==='chat'),box=$('#chatHistory');box.classList.toggle('hidden',!chat.length);if(!chat.length)return;$('#chatSummary').textContent=tr('chatHistory',{count:chat.length});$('#chatRows').innerHTML=chat.map(event=>`<div class="chat-row"><time>${fmt(event.tick/state.meta.tickRate)}</time><span><b>${esc(event.actor)}</b>: ${esc(event.detail)}</span></div>`).join('')}
function draw(){if(!state.meta)return;const c=$('#events'),box=c.getBoundingClientRect(),d=devicePixelRatio||1;c.width=Math.max(1,Math.round(box.width*d));c.height=Math.max(1,Math.round(box.height*d));const x=c.getContext('2d'),style=getComputedStyle(document.documentElement),density=state.meta.density||[],peak=Math.max(1,...density),unit=box.width/Math.max(1,density.length);x.setTransform(d,0,0,d,0,0);x.clearRect(0,0,box.width,box.height);x.fillStyle=style.getPropertyValue('--timeline');density.forEach((value,index)=>{const height=box.height*(.58+.28*value/peak);x.fillRect(index*unit+1,box.height-height,Math.max(1,unit-2),height)});x.fillStyle=style.getPropertyValue('--timeline-ok');x.fillRect(0,8,box.width,7);state.hit=[];for(const event of state.events){const spec=EVENT_STYLE[event.kind];if(!spec||!Number.isFinite(+event.tick))continue;const width=Math.max(3,Math.min(6,box.width/160)),position=Math.max(0,Math.min(box.width,+event.tick/state.meta.ticks*box.width)),height=Math.max(16,box.height*spec[2]);x.fillStyle=style.getPropertyValue(spec[1]);x.fillRect(position-width/2,box.height-height,width,height);state.hit.push({event,x:position,width})}}
function render(){const m=state.meta;$('#demoName').textContent=m.name;$('#demoSub').textContent=m.client||tr('clientUnknown');$('#badges').innerHTML=`<span class="badge accent">${m.kind}</span>`;const mb=language==='ru'?' МБ':' MB';$('#stats').innerHTML=[[tr('map'),m.map],[tr('server'),m.server||tr('serverUnknown')],[tr('duration'),fmt(m.duration)],[tr('ticks'),m.ticks.toLocaleString(locale())],[tr('tickrate'),m.tickRate.toFixed(3)],[tr('frames'),m.frames.toLocaleString(locale())],[tr('size'),(m.size/1048576).toFixed(1)+mb]].map(item=>`<div class="stat"><small>${item[0]}</small><b title="${esc(item[1])}">${esc(item[1])}</b></div>`).join('');$('#tickAxis').innerHTML=axis(m.ticks,value=>Math.round(value).toLocaleString(locale()));$('#timeAxis').innerHTML=axis(m.duration,fmt);for(const id of ['startRange','endRange']){$('#'+id).max=m.ticks;$('#'+id).step=1}if(!state.selectionReady){$('#startRange').value=0;$('#endRange').value=m.ticks;state.selectionReady=true}renderPov();syncSelection();draw()}
function syncSelection(fromInput=false){const m=state.meta;if(!m)return;let a=+$('#startRange').value,b=+$('#endRange').value;if(a>=b){if(fromInput&&document.activeElement===$('#startRange'))a=Math.max(0,b-1);else b=Math.min(m.ticks,a+1);$('#startRange').value=a;$('#endRange').value=b}const selection=$('#selection');selection.style.setProperty('--start',(a/m.ticks*100)+'%');selection.style.setProperty('--end',(b/m.ticks*100)+'%');$('#startTime').value=(a/m.tickRate).toFixed(2);$('#endTime').value=(b/m.tickRate).toFixed(2);$('#startTick').textContent=`· ${a.toLocaleString(locale())} ${tr('ticksShort')}`;$('#endTick').textContent=`· ${b.toLocaleString(locale())} ${tr('ticksShort')}`;saveSession()}
function syncTicks(){const m=state.meta;$('#startRange').value=Math.max(0,Math.min(m.ticks-1,Math.round(+$('#startTime').value*m.tickRate)));$('#endRange').value=Math.max(1,Math.min(m.ticks,Math.round(+$('#endTime').value*m.tickRate)));syncSelection(true)}
function renderSegments(){const m=state.meta;if(!m)return;const ticks=state.ranges.reduce((total,range)=>total+range[1]-range[0],0),axis=values=>values.map((value,index)=>`<span style="left:${ticks?value/ticks*100:0}%;transform:translateX(${index===0?'0':index===values.length-1?'-100%':'-50%'})">${index===0?0:value.toLocaleString(locale())}</span>`).join(''),palette=['#cf6a35','#1f8f6f','#3d6fa8','#c98a1f','#8461c9','#c0463d'];$('#segments').innerHTML=state.ranges.map((range,index)=>{const duration=range[1]-range[0],size=(m.size*duration/m.ticks/1048576).toFixed(1);return `<div class="segment" draggable="true" data-index="${index}"><span class="drag-handle" aria-hidden="true">⠿</span><span class="clip-order">#${index+1}</span><span class="clip-range">${fmt(range[0]/m.tickRate)} → ${fmt(range[1]/m.tickRate)}</span><span class="clip-meta"><span>${tr('duration')}: <b>${fmt(duration/m.tickRate)}</b></span><span>${tr('ticks')}: <b>${range[0].toLocaleString(locale())} → ${range[1].toLocaleString(locale())}</b></span><span>${tr('size')}: <b>≈${size}${language==='ru'?' МБ':' MB'}</b></span></span><button data-remove="${index}" aria-label="${tr('remove')}">${tr('remove')}</button></div>`}).join('');$('#savedRanges').innerHTML=state.ranges.map((range,index)=>`<i class="range-mark" style="left:${range[0]/m.ticks*100}%;width:${(range[1]-range[0])/m.ticks*100}%;background:${palette[index%palette.length]}"></i>`).join('');$('#outputTimeline').classList.toggle('hidden',!state.ranges.length);const points=[0];state.ranges.reduce((sum,range)=>(points.push(sum+range[1]-range[0]),sum+range[1]-range[0]),0);$('#outputTrack').innerHTML=state.ranges.map((range,index)=>`<div class="output-clip" draggable="true" data-index="${index}" style="flex:${range[1]-range[0]};--clip:${palette[index%palette.length]}"><b>#${index+1}</b><span>${fmt(range[0]/m.tickRate)} → ${fmt(range[1]/m.tickRate)}</span></div>`).join('');$('#outputTickAxis').innerHTML=axis(points);$('#outputTimeAxis').innerHTML=axis(points.map(value=>fmt(value/m.tickRate)));$('#montageSummary').textContent=state.ranges.length?tr('segments',{count:state.ranges.length,time:fmt(ticks/m.tickRate)}):tr('noSegments');saveSession()}
function renderPlayers(){if(!state.meta)return;const voice=state.playerTab==='voices',list=voice?state.players:state.allPlayers;$('#voiceOnly').classList.toggle('hidden',!voice);$('#toggleAll').classList.toggle('hidden',!voice);$('#playerTabs').querySelectorAll('button').forEach(button=>button.setAttribute('aria-selected',button.value===state.playerTab));const mic='<svg class="mic" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 1a3 3 0 0 0-3 3v8a3 3 0 0 0 6 0V4a3 3 0 0 0-3-3z"/><path d="M19 10v2a7 7 0 0 1-14 0v-2"/><path d="M12 19v4"/></svg>';$('#players').innerHTML=list.length?list.map(player=>voice?`<label class="player"><input type="checkbox" value="${player.client}"><span class="info"><span class="name">${esc(player.name)}</span><small>${esc(player.steamid)}</small></span>${mic}</label>`:`<div class="player"><span></span><span class="info"><span class="name">${esc(player.name)}</span><small>${esc(player.steamid)}</small></span></div>`).join(''):`<span class="sub">${voice?tr('noVoices'):tr('noEvents')}</span>`;renderPov()}
async function loadVoices(){try{const r=await fetch('/api/voices?id='+state.id),data=await r.json();if(!r.ok)throw new Error(data.error);state.players=data.players;state.allPlayers=data.allPlayers||[];state.events=data.events||[];state.eventsLoaded=true;renderPlayers();renderLegend();renderChat();draw()}catch(e){state.eventsLoaded=true;draw();$('#players').innerHTML=`<span class="status error">${esc(e.message)}</span>`}}
function updateAudioNote(){const format=$('#audioFormat').value,note=$('#audioNote');note.textContent=tr(format==='wav'?'audioWav':format==='mp3'?'audioMp3':'audioOgg');note.classList.toggle('warn',format==='wav')}
$('#playerTabs').onclick=e=>{const button=e.target.closest('button[value]');if(!button)return;state.playerTab=button.value;renderPlayers()};
function voiceRequest(client){return fetch('/api/voice',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({id:state.id,clients:[client],keepGaps:$('#keepGaps').checked})})}
function wavBlob(audio){const samples=audio.getChannelData(0),buffer=new ArrayBuffer(44+samples.length*2),view=new DataView(buffer),put=(offset,text)=>{for(let i=0;i<text.length;i++)view.setUint8(offset+i,text.charCodeAt(i))};put(0,'RIFF');view.setUint32(4,36+samples.length*2,true);put(8,'WAVE');put(12,'fmt ');view.setUint32(16,16,true);view.setUint16(20,1,true);view.setUint16(22,1,true);view.setUint32(24,audio.sampleRate,true);view.setUint32(28,audio.sampleRate*2,true);view.setUint16(32,2,true);view.setUint16(34,16,true);put(36,'data');view.setUint32(40,samples.length*2,true);for(let i=0;i<samples.length;i++){const value=Math.max(-1,Math.min(1,samples[i]));view.setInt16(44+i*2,value<0?value*32768:value*32767,true)}return new Blob([buffer],{type:'audio/wav'})}
async function mp3Blob(audio){if(!globalThis.lamejs)throw new Error('lamejs is unavailable');const source=audio.getChannelData(0),pcm=new Int16Array(source.length);for(let i=0;i<source.length;i++){const value=Math.max(-1,Math.min(1,source[i]));pcm[i]=value<0?value*32768:value*32767}const encoder=new lamejs.Mp3Encoder(1,audio.sampleRate,128),chunks=[];for(let offset=0;offset<pcm.length;offset+=1152){const encoded=encoder.encodeBuffer(pcm.subarray(offset,offset+1152));if(encoded.length)chunks.push(new Uint8Array(encoded));if(offset%(1152*128)===0)await new Promise(resolve=>setTimeout(resolve,0))}const tail=encoder.flush();if(tail.length)chunks.push(new Uint8Array(tail));return new Blob(chunks,{type:'audio/mpeg'})}
async function convertVoice(response,format){const context=new (globalThis.AudioContext||globalThis.webkitAudioContext)();try{const audio=await context.decodeAudioData(await (await response.blob()).arrayBuffer());return format==='wav'?wavBlob(audio):await mp3Blob(audio)}finally{await context.close()}}
async function downloadVoices(button){const clients=[...document.querySelectorAll('#players input:checked')].map(input=>+input.value);if(!clients.length)return say(tr('selectPlayer'),true);const format=$('#audioFormat').value;if(format==='ogg')return post('/api/voice',{id:state.id,clients,keepGaps:$('#keepGaps').checked},button);const old=button.innerHTML;button.disabled=true;try{for(let index=0;index<clients.length;index++){button.innerHTML=`<i class="spinner"></i> ${tr('converting',{current:index+1,total:clients.length})}`;const response=await voiceRequest(clients[index]);if(!response.ok)throw new Error((await response.json()).error||response.statusText);const base=filename(response).replace(/\.ogg$/i,''),blob=await convertVoice(response,format);downloadBlob(blob,`${base}.${format}`);await new Promise(resolve=>setTimeout(resolve,180))}say(tr('audioReady'))}catch(error){say(tr('audioError',{error:error.message}),true)}finally{button.disabled=false;button.innerHTML=old}}
$('#pick').onclick=()=>$('#file').click();$('#file').onchange=e=>upload(e.target.files[0]);const drop=$('#drop');for(const e of ['dragenter','dragover'])drop.addEventListener(e,x=>{x.preventDefault();drop.classList.add('drag')});for(const e of ['dragleave','drop'])drop.addEventListener(e,x=>{x.preventDefault();drop.classList.remove('drag')});drop.ondrop=e=>upload(e.dataTransfer.files[0]);drop.onkeydown=e=>{if(e.key==='Enter'||e.key===' '){e.preventDefault();$('#file').click()}};
$('#reset').onclick=()=>{localStorage.removeItem('tf2DemoToolsSession');location.reload()};$('#startRange').oninput=$('#endRange').oninput=()=>syncSelection(true);$('#startTime').oninput=$('#endTime').oninput=syncTicks;window.onresize=()=>state.meta&&draw();
let rangeDrag;$('#selection').onpointerdown=e=>{if(e.button!==0||!state.meta)return;e.preventDefault();const a=+$('#startRange').value,b=+$('#endRange').value;rangeDrag={id:e.pointerId,x:e.clientX,a,b,width:$('#track').getBoundingClientRect().width};e.currentTarget.setPointerCapture(e.pointerId);e.currentTarget.classList.add('is-dragging')};$('#selection').onpointermove=e=>{if(!rangeDrag||e.pointerId!==rangeDrag.id)return;const m=state.meta,span=rangeDrag.b-rangeDrag.a,delta=Math.round((e.clientX-rangeDrag.x)/rangeDrag.width*m.ticks),start=Math.max(0,Math.min(m.ticks-span,rangeDrag.a+delta));$('#startRange').value=start;$('#endRange').value=start+span;syncSelection()};$('#selection').onpointerup=$('#selection').onpointercancel=e=>{if(!rangeDrag||e.pointerId!==rangeDrag.id)return;rangeDrag=undefined;e.currentTarget.classList.remove('is-dragging')};
$('#track').onmousemove=e=>{const tip=$('#eventTip'),rect=e.currentTarget.getBoundingClientRect(),near=state.hit.reduce((best,item)=>!best||Math.abs(item.x-(e.clientX-rect.left))<Math.abs(best.x-(e.clientX-rect.left))?item:best,null);if(!near||Math.abs(near.x-(e.clientX-rect.left))>Math.max(8,near.width*2)){tip.style.display='none';return}const event=near.event,spec=EVENT_STYLE[event.kind];tip.innerHTML=`<b>${tr(spec[0])}</b><small>${tr('eventAt',{time:fmt(event.tick/state.meta.tickRate),tick:(+event.tick).toLocaleString(locale())})}</small><p>${esc(eventText(event)||tr(spec[0]))}</p>`;tip.style.display='block';tip.style.left=`${Math.min(window.innerWidth-260,e.clientX+12)}px`;tip.style.top=`${Math.max(8,e.clientY-12)}px`};$('#track').onmouseleave=()=>$('#eventTip').style.display='none';
let dragIndex=-1;function moveRange(from,to){if(from===to||from<0||to<0)return;const [range]=state.ranges.splice(from,1);state.ranges.splice(to,0,range);renderSegments()}$('#addSegment').onclick=()=>{state.ranges.push([+$('#startRange').value,+$('#endRange').value]);renderSegments()};$('#clearSegments').onclick=()=>{state.ranges=[];renderSegments()};$('#segments').onclick=e=>{if(e.target.dataset.remove!==undefined){state.ranges.splice(+e.target.dataset.remove,1);renderSegments()}};$('#segments').ondragstart=e=>{const row=e.target.closest('.segment');if(!row)return;dragIndex=+row.dataset.index;row.classList.add('dragging');e.dataTransfer.effectAllowed='move'};$('#segments').ondragend=()=>{dragIndex=-1;document.querySelectorAll('.segment').forEach(row=>row.classList.remove('dragging','drop-target'))};$('#segments').ondragover=e=>{const row=e.target.closest('.segment');if(!row||dragIndex<0)return;e.preventDefault();row.classList.add('drop-target')};$('#segments').ondragleave=e=>e.target.closest('.segment')?.classList.remove('drop-target');$('#segments').ondrop=e=>{const row=e.target.closest('.segment');if(!row)return;e.preventDefault();moveRange(dragIndex,+row.dataset.index)};$('#outputTrack').ondragover=e=>e.preventDefault();$('#outputTrack').ondrop=e=>{e.preventDefault();if(dragIndex<0)return;const rect=e.currentTarget.getBoundingClientRect(),total=state.ranges.reduce((sum,range)=>sum+range[1]-range[0],0),point=(e.clientX-rect.left)/rect.width*total;let sum=0,to=state.ranges.findIndex(range=>(sum+=range[1]-range[0])>=point);moveRange(dragIndex,to<0?state.ranges.length:to)};
$('#outputTrack').addEventListener('dragstart',e=>{const clip=e.target.closest('.output-clip');if(!clip)return;dragIndex=+clip.dataset.index;clip.classList.add('dragging');e.dataTransfer.effectAllowed='move'});$('#outputTrack').addEventListener('dragend',()=>{dragIndex=-1;document.querySelectorAll('.output-clip').forEach(clip=>clip.classList.remove('dragging'))});
let outputPointerIndex=-1;function dropOutputRange(clientX){if(outputPointerIndex<0)return;const track=$('#outputTrack'),rect=track.getBoundingClientRect(),total=state.ranges.reduce((sum,range)=>sum+range[1]-range[0],0),point=(clientX-rect.left)/rect.width*total;let sum=0,to=state.ranges.findIndex(range=>(sum+=range[1]-range[0])>=point);moveRange(outputPointerIndex,to<0?state.ranges.length:to);outputPointerIndex=-1;document.querySelectorAll('.output-clip').forEach(clip=>clip.classList.remove('dragging'))}$('#outputTrack').addEventListener('pointerdown',e=>{const clip=e.target.closest('.output-clip');if(!clip)return;e.preventDefault();outputPointerIndex=+clip.dataset.index;clip.classList.add('dragging');e.currentTarget.setPointerCapture?.(e.pointerId)});$('#outputTrack').addEventListener('pointerup',e=>dropOutputRange(e.clientX));$('#outputTrack').addEventListener('pointercancel',()=>{outputPointerIndex=-1;document.querySelectorAll('.output-clip').forEach(clip=>clip.classList.remove('dragging'))});
$('#downloadMontage').onclick=e=>{if(!state.ranges.length)return say(tr('addFirst'),true);post('/api/edit',{id:state.id,ranges:state.ranges},e.currentTarget)};
$('#toggleAll').onclick=()=>{const boxes=[...document.querySelectorAll('#players input')],on=boxes.some(input=>!input.checked);boxes.forEach(input=>input.checked=on)};$('#playerSearch').oninput=e=>{const q=e.target.value.trim().toLowerCase();document.querySelectorAll('#players .player').forEach(row=>row.hidden=!row.textContent.toLowerCase().includes(q))};$('#downloadVoices').onclick=e=>downloadVoices(e.currentTarget);$('#downloadAllVoices').onclick=e=>{const clients=state.players.map(player=>player.client);if(clients.length)post('/api/voice',{id:state.id,clients,keepGaps:$('#keepGaps').checked},e.currentTarget)};$('#audioFormat').onchange=updateAudioNote;$('#languageButton').onclick=()=>{const menu=$('#languageMenu'),open=!menu.classList.contains('open');menu.classList.toggle('open',open);$('#languageButton').setAttribute('aria-expanded',open);$('#themeMenu').classList.remove('open');$('#themeButton').setAttribute('aria-expanded','false')};$('#languageOptions').onclick=e=>{const button=e.target.closest('button[value]');if(!button)return;applyLanguage(button.value);$('#languageMenu').classList.remove('open');$('#languageButton').setAttribute('aria-expanded','false')};$('#themeButton').onclick=()=>{const menu=$('#themeMenu'),open=!menu.classList.contains('open');menu.classList.toggle('open',open);$('#themeButton').setAttribute('aria-expanded',open);$('#languageMenu').classList.remove('open');$('#languageButton').setAttribute('aria-expanded','false')};$('#theme').onclick=e=>{const button=e.target.closest('button[value]');if(!button)return;applyTheme(button.value);$('#themeMenu').classList.remove('open');$('#themeButton').setAttribute('aria-expanded','false')};document.addEventListener('click',e=>{if(!e.target.closest('#languageMenu')){$('#languageMenu').classList.remove('open');$('#languageButton').setAttribute('aria-expanded','false')}if(!e.target.closest('#themeMenu')){$('#themeMenu').classList.remove('open');$('#themeButton').setAttribute('aria-expanded','false')}});window.addEventListener('languagechange',()=>{languageSetting==='system'&&applyLanguage('system')});matchMedia('(prefers-color-scheme: dark)').addEventListener('change',()=>{choiceValue('theme')==='system'&&state.meta&&draw()});applyTheme(localStorage.getItem('demoToolsTheme')||'system');applyLanguage(localStorage.getItem('demoToolsLanguage')||'system');
async function restoreSession(){let saved;try{const shared=new URLSearchParams(location.search).get('session');saved=shared?{id:shared}:JSON.parse(localStorage.getItem('tf2DemoToolsSession')||'null')}catch(_){localStorage.removeItem('tf2DemoToolsSession');return}if(!saved?.id)return;try{const r=await fetch('/api/session?id='+saved.id),data=await r.json();if(!r.ok)throw new Error(data.error);state={id:data.id,meta:data.meta,ranges:Array.isArray(saved.ranges)?saved.ranges:[],players:[],allPlayers:[],playerTab:'voices',events:[],eventsLoaded:false,hit:[],selectionReady:true};render();$('#startRange').value=saved.selection?.[0]??0;$('#endRange').value=saved.selection?.[1]??data.meta.ticks;syncSelection();renderSegments();intro.classList.add('hidden');workspace.classList.remove('hidden');$('#reset').classList.remove('hidden');say(tr('ready'));loadVoices()}catch(_){localStorage.removeItem('tf2DemoToolsSession')}}restoreSession();
</script></body></html>'''

HTML = HTML.replace(
    "</head>",
    """<style>
.players-card .panel-head{display:grid;gap:8px;margin-bottom:10px}.players-card .player-tabs{min-width:0;width:100%}.players-card #toggleAll{justify-self:start;padding:5px 9px;white-space:nowrap}.output-clip{flex:0 0 var(--clip-width);width:var(--clip-width);transition:transform .12s ease,box-shadow .12s ease,opacity .12s ease}.output-clip.dragging{opacity:.35;transform:scale(.985)}.output-clip.drop-target{box-shadow:inset 0 0 0 2px #fffd}
.theme-menu{position:relative}.theme-button,.theme-options{padding:4px;border:1px solid var(--line);border-radius:10px;background:var(--panel)}.theme-button{display:grid;width:46px;height:44px;place-items:center;color:var(--accent2);font-size:16px}.theme-button:hover,.theme-button[aria-expanded=true]{border-color:var(--line-strong)}.theme-button>span{display:grid;width:36px;height:34px;place-items:center;border-radius:7px;background:var(--accent-soft)}.theme-options{position:absolute;top:calc(100% + 6px);right:0;z-index:12;display:none;gap:3px}.theme-menu.open .theme-options{display:grid}.theme-options button{display:grid;width:36px;height:34px;place-items:center;border-radius:7px;background:transparent;color:var(--muted);font-size:16px}.theme-options button:hover,.theme-options button[aria-checked=true]{background:var(--accent-soft);color:var(--accent2)}
</style></head>""",
)
HTML = HTML.replace(
    "</body>",
    """<script>
state.playerTab='all';
const loadVoicesWithDemoPlayers=loadVoices;
loadVoices=async()=>{if(!state.eventsLoaded)state.playerTab='all';return loadVoicesWithDemoPlayers()};
const renderSegmentsWithFlexibleWidths=renderSegments;
renderSegments=()=>{renderSegmentsWithFlexibleWidths();const total=state.ranges.reduce((sum,range)=>sum+range[1]-range[0],0)||1;document.querySelectorAll('#outputTrack .output-clip').forEach((clip,index)=>{const range=state.ranges[index],width=Math.max(0,(range[1]-range[0])/total*100)+'%';clip.style.flex='0 0 '+width;clip.style.width=width;clip.style.setProperty('--clip-width',width)})};
const clearOutputDropTargets=()=>document.querySelectorAll('.output-clip').forEach(clip=>clip.classList.remove('drop-target'));
$('#outputTrack').addEventListener('pointermove',event=>{if(outputPointerIndex<0)return;clearOutputDropTargets();const target=event.target.closest('.output-clip');if(target&&+target.dataset.index!==outputPointerIndex)target.classList.add('drop-target')});
$('#outputTrack').addEventListener('pointerup',clearOutputDropTargets);$('#outputTrack').addEventListener('pointercancel',clearOutputDropTargets);
$('#outputTrack').addEventListener('dragover',event=>{if(dragIndex<0)return;clearOutputDropTargets();const target=event.target.closest('.output-clip');if(target&&+target.dataset.index!==dragIndex)target.classList.add('drop-target')});
$('#outputTrack').addEventListener('dragend',clearOutputDropTargets);
document.head.insertAdjacentHTML('beforeend',`<style>.player-tabs button[value=all]{order:-1}.player .name a,.event-player{color:inherit;text-decoration:none}.player .name a:hover,.event-player:hover{text-decoration:underline}.team-red{color:#df726a!important}.team-blu{color:#6d9bd0!important}.team-spectator{color:var(--muted)!important}.chat-row{cursor:pointer}.chat-row:hover{background:var(--panel)}.track.event-focus{outline:2px solid var(--accent);outline-offset:2px}.event-tip{background:var(--panel)!important;opacity:1}#languageOptions{left:0;right:auto}.flag{border:1px solid #0003!important;box-shadow:none!important}.flag::after{content:none!important}.flag-ru{background:linear-gradient(#fff 0 33.333%,#1d57a6 33.333% 66.666%)!important}.flag-en{background:#012169 url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 60 30'%3E%3Cpath fill='%23012169' d='M0 0h60v30H0z'/%3E%3Cpath stroke='%23fff' stroke-width='6' d='m0 0 60 30m0-30L0 30'/%3E%3Cpath stroke='%23c8102e' stroke-width='2' d='m0 0 60 30m0-30L0 30'/%3E%3Cpath stroke='%23fff' stroke-width='10' d='M30 0v30M0 15h60'/%3E%3Cpath stroke='%23c8102e' stroke-width='6' d='M30 0v30M0 15h60'/%3E%3C/svg%3E") center/cover no-repeat!important}</style>`);
document.head.insertAdjacentHTML('beforeend',`<style>#track.tip-open{z-index:8;overflow:visible}.event-tip{z-index:9999!important}.selection{z-index:1!important;left:0!important;right:0!important;border:0!important;box-shadow:none!important;background:linear-gradient(to right,#0005 0 var(--start),transparent var(--start) var(--end),#0005 var(--end) 100%)!important}#chatRows .chat-row[hidden]{display:none!important}.montage-inline .actions{display:flex;gap:8px;align-items:center}.montage-name{flex:1;min-width:180px}.event-legend button{display:inline-flex;align-items:center;gap:6px;padding:3px 6px;border:1px solid transparent;border-radius:5px;background:transparent;color:inherit;cursor:pointer}.event-legend button[aria-pressed=false]{opacity:.38;text-decoration:line-through}.flag-ru{background:linear-gradient(to bottom,#fff 0 33.333%,#0039a6 33.333% 66.666%,#d52b1e 66.666% 100%)!important}.pov-free-camera{cursor:pointer;font:inherit}.pov-free-camera[aria-pressed=true]{background:var(--accent);border-color:var(--accent);color:#fff}#povControls,#povFreeCameraButton{display:none!important}.montage-free-camera{display:flex;align-items:center;margin:12px 0 0}.montage-free-camera .switch{margin:0}.montage-free-camera[hidden]{display:none}</style>`);
document.head.insertAdjacentHTML('beforeend',`<style>
#languageButton{width:44px;height:44px;justify-content:center;padding:0}#languageButton #languageLabel,#languageButton svg{display:none}.output-timeline{padding:48px 0 29px;margin-bottom:19px}.output-axis{z-index:4}.output-axis.top{top:3px}.output-axis .axis-hidden{display:none}.output-track{position:relative;z-index:1;height:68px;min-height:68px;overflow:visible}.output-clip{position:relative;min-width:0;height:68px;padding:0;overflow:visible;background:var(--clip);border-right:1px solid #0004}.output-clip canvas{display:block;width:100%;height:68px}.output-order,.output-range{position:absolute;left:7px;right:7px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font:10.5px ui-monospace,Consolas,monospace;text-shadow:0 1px 1px #0008}.output-order{top:-15px;color:var(--ink)}.output-range{bottom:-19px;color:var(--muted)}.output-clip:first-child{border-radius:7px 0 0 7px}.output-clip:last-child{border-radius:0 7px 7px 0;border-right:0}.output-clip:only-child{border-radius:7px}.hero{max-width:620px;padding:22px 0 14px}.hero .eyebrow{display:none}.hero h1{margin:0 0 7px;font-size:clamp(26px,4vw,36px);letter-spacing:-.025em;line-height:1.12}.hero p{margin:0;font-size:14px;max-width:500px}.drop{margin-top:16px;padding:34px 24px}
</style>`);
const montageName=document.createElement('input');montageName.id='montageName';montageName.className='control montage-name';montageName.type='text';montageName.maxLength=100;montageName.spellcheck=false;montageName.setAttribute('aria-label','Demo filename');$('#downloadMontage').before(montageName);
I18N.ru.eventsHistory='События · {count}';I18N.en.eventsHistory='Events · {count}';I18N.ru.heroTitle='Редактор TF2-демок';I18N.en.heroTitle='TF2 demo editor';I18N.ru.heroText='Нарезка, порядок отрезков, POV / SourceTV и голоса — локально.';I18N.en.heroText='Cut, arrange and export POV / SourceTV demos locally.';if(!state.meta){$('#heroTitle').innerHTML=tr('heroTitle');$('#intro [data-t="heroText"]').textContent=tr('heroText')}const applyLanguageWithIconTitle=applyLanguage;applyLanguage=value=>{applyLanguageWithIconTitle(value);$('#languageButton').title=tr('languageLabel');$('#languageButton').ariaLabel=tr('languageLabel')};$('#languageButton').title=tr('languageLabel');$('#languageButton').ariaLabel=tr('languageLabel');
const steamProfile=name=>{const player=[...state.allPlayers,...state.players].find(player=>player.name===name),match=player?.steamid?.match(/U:1:(\\d+)/);return match?'https://steamcommunity.com/profiles/'+(76561197960265728n+BigInt(match[1])).toString():''};
const playerLink=(name,team='')=>{const href=steamProfile(name),className=/spectator/i.test(team)?' team-spectator':/\\bRED\\b/i.test(team)?' team-red':/\\bBLU\\b/i.test(team)?' team-blu':'';return href?`<a class="event-player${className}" href="${href}" target="_blank" rel="noopener">${esc(name)}</a>`:`<span class="event-player${className}">${esc(name)}</span>`};
const chatLine=event=>{const actor=String(event.actor||'').replace(/^:\\s*/,''),message=String(event.detail||'').replace(/^:\\s*/,''),head=actor?playerLink(actor,event.team||event.detail)+': ':'';return head+esc(message)};
let showEventTip=(event,left,top)=>{const tip=$('#eventTip'),spec=EVENT_STYLE[event.kind],body=event.kind==='chat'?chatLine(event):event.actor?playerLink(event.actor,event.detail)+(event.detail?' · '+esc(event.detail):''):esc(eventText(event)||tr(spec[0]));tip.innerHTML=`<b>${tr(spec[0])}</b><small>${tr('eventAt',{time:fmt(event.tick/state.meta.tickRate),tick:(+event.tick).toLocaleString(locale())})}</small><p>${body}</p>`;tip.style.display='block';tip.style.left=`${Math.max(8,Math.min(window.innerWidth-260,left+12))}px`;tip.style.top=`${Math.max(8,top-12)}px`};
renderPlayers=()=>{if(!state.meta)return;const voice=state.playerTab==='voices',list=voice?state.players:state.allPlayers;$('#voiceOnly').classList.toggle('hidden',!voice);$('#toggleAll').classList.toggle('hidden',!voice);$('#playerTabs').querySelectorAll('button').forEach(button=>button.setAttribute('aria-selected',button.value===state.playerTab));const mic='<svg class="mic" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 1a3 3 0 0 0-3 3v8a3 3 0 0 0 6 0V4a3 3 0 0 0-3-3z"/><path d="M19 10v2a7 7 0 0 1-14 0v-2"/><path d="M12 19v4"/></svg>';$('#players').innerHTML=list.length?list.map(player=>voice?`<label class="player"><input type="checkbox" value="${player.client}"><span class="info"><span class="name">${playerLink(player.name)}</span><small>${esc(player.steamid)}</small></span>${mic}</label>`:`<div class="player"><span></span><span class="info"><span class="name">${playerLink(player.name)}</span><small>${esc(player.steamid)}</small></span></div>`).join(''):`<span class="sub">${voice?tr('noVoices'):tr('noEvents')}</span>`;renderPov()};
renderChat=()=>{const teams={};for(const event of state.events){const detail=String(event.detail||''),team=/\\bRED\\b/i.test(detail)?'RED':/\\bBLU\\b/i.test(detail)?'BLU':/spectator/i.test(detail)?'Spectator':'';if(team&&event.actor)teams[event.actor]=team;event.team=teams[event.actor]||'';event.targetTeam=teams[event.target]||''}const events=state.events.filter(event=>EVENT_STYLE[event.kind]),box=$('#chatHistory'),line=item=>item.kind==='chat'?chatLine(item):item.kind==='death'?`${playerLink(item.actor,item.team)} → ${playerLink(item.target,item.targetTeam)}${item.detail?' · '+esc(item.detail):''}`:item.actor?playerLink(item.actor,item.team||item.detail)+(item.detail?' · '+esc(item.detail):''):esc(eventText(item));box.classList.toggle('hidden',!events.length);if(!events.length)return;$('#chatSummary').textContent=tr('eventsHistory',{count:events.length});$('#chatRows').innerHTML=events.map((event,index)=>`<div class="chat-row" data-event="${index}"><time>${fmt(event.tick/state.meta.tickRate)}</time><span><b>${esc(tr(EVENT_STYLE[event.kind][0]))}</b> · ${line(event)}</span></div>`).join('');$('#chatRows').onclick=event=>{const row=event.target.closest('[data-event]'),item=events[+row?.dataset.event];if(!item)return;state.focusTick=+item.tick;$('#track').scrollIntoView({behavior:'auto',block:'center'});$('#track').classList.add('event-focus');draw();setTimeout(()=>{const rect=$('#track').getBoundingClientRect();showEventTip(item,rect.left+(+item.tick/state.meta.ticks)*rect.width,rect.top+rect.height/2)},50);setTimeout(()=>{if(state.focusTick!==+item.tick)return;$('#track').classList.remove('event-focus');$('#eventTip').style.display='none';state.focusTick=null;draw()},2000)}};
draw=()=>{if(!state.meta)return;const c=$('#events'),box=c.getBoundingClientRect(),d=devicePixelRatio||1;c.width=Math.max(1,Math.round(box.width*d));c.height=Math.max(1,Math.round(box.height*d));const x=c.getContext('2d'),style=getComputedStyle(document.documentElement),density=state.meta.density||[],sorted=[...density].sort((a,b)=>a-b),peak=Math.max(1,sorted[Math.floor(sorted.length*.9)]||1),unit=box.width/Math.max(1,density.length);x.setTransform(d,0,0,d,0,0);x.clearRect(0,0,box.width,box.height);x.fillStyle=style.getPropertyValue('--timeline');density.forEach((value,index)=>{const height=box.height*(.12+.76*Math.min(1,value/peak));x.fillRect(index*unit+1,box.height-height,Math.max(1,unit-2),height)});x.fillStyle=style.getPropertyValue('--timeline-ok');x.fillRect(0,8,box.width,7);state.hit=[];for(const event of state.events){const spec=EVENT_STYLE[event.kind];if(!spec||!Number.isFinite(+event.tick))continue;const width=Math.max(3,Math.min(6,box.width/160)),position=Math.max(0,Math.min(box.width,+event.tick/state.meta.ticks*box.width)),height=Math.max(16,box.height*spec[2]);x.fillStyle=style.getPropertyValue(spec[1]);x.fillRect(position-width/2,box.height-height,width,height);state.hit.push({event,x:position,width});if(+event.tick===state.focusTick){x.strokeStyle=style.getPropertyValue('--accent');x.lineWidth=2;x.strokeRect(position-6,box.height-height-6,12,height+10)}}};
$('#track').onmousemove=e=>{const tip=$('#eventTip'),rect=e.currentTarget.getBoundingClientRect(),near=state.hit.reduce((best,item)=>!best||Math.abs(item.x-(e.clientX-rect.left))<Math.abs(best.x-(e.clientX-rect.left))?item:best,null);if(!near||Math.abs(near.x-(e.clientX-rect.left))>Math.max(8,near.width*2)){tip.style.display='none';return}showEventTip(near.event,e.clientX,e.clientY)};
const rawRenderChat=renderChat;
renderChat=()=>{rawRenderChat();if(!state.eventFilters)return;const events=state.events.filter(event=>EVENT_STYLE[event.kind]),visible=events.filter(event=>state.eventFilters.has(event.kind));$('#chatSummary').textContent=tr('eventsHistory',{count:visible.length});document.querySelectorAll('#chatRows [data-event]').forEach(row=>row.hidden=!state.eventFilters.has(events[+row.dataset.event]?.kind))};
const rawDraw=draw;
draw=()=>{const saved=state.events;if(state.eventFilters)state.events=saved.filter(event=>state.eventFilters.has(event.kind));rawDraw();state.events=saved};
renderLegend=()=>{const groups={};state.events.forEach(event=>{const style=EVENT_STYLE[event.kind];if(style)(groups[event.kind]??={style,count:0}).count++});state.eventFilters??=new Set(Object.keys(groups));$('#eventLegend').innerHTML=Object.entries(groups).map(([kind,{style,count}])=>`<button type="button" value="${kind}" aria-pressed="${state.eventFilters.has(kind)}" style="--dot:var(${style[1]})"><i></i>${tr(style[0])} · ${count}</button>`).join('');$('#eventLegend').onclick=event=>{const button=event.target.closest('button[value]');if(!button)return;state.eventFilters.has(button.value)?state.eventFilters.delete(button.value):state.eventFilters.add(button.value);renderLegend();renderChat();draw()}};
const rawRenderSegments=renderSegments;
renderSegments=()=>{rawRenderSegments();const ticks=state.ranges.reduce((sum,range)=>sum+range[1]-range[0],0)||1,points=[0];state.ranges.reduce((sum,range)=>(points.push(sum+range[1]-range[0]),sum+range[1]-range[0]),0);$('#outputTimeAxis').innerHTML=points.map((value,index)=>`<span style="left:${value/ticks*100}%;transform:translateX(${index===0?'0':index===points.length-1?'-100%':'-50%'})">${fmt(value/state.meta.tickRate)}</span>`).join('')};
const rawRender=render;
render=()=>{rawRender();if(state.meta)montageName.value=state.meta.name.replace(/\\.dem$/i,'')+'-edit'};
const renderWithOriginalPov=render;
renderPov=()=>{};
render=()=>{renderWithOriginalPov()};
const rawShowEventTip=showEventTip;
showEventTip=(...args)=>{$('#track').classList.add('tip-open');rawShowEventTip(...args)};
document.body.append($('#eventTip'));
$('#track').addEventListener('mouseleave',()=>$('#track').classList.remove('tip-open'));
const rawTrackMove=$('#track').onmousemove;
$('#track').onmousemove=e=>{rawTrackMove(e);if($('#eventTip').style.display==='none')$('#track').classList.remove('tip-open')};
const CLIP_PALETTE=['#cf6a35','#1f8f6f','#3d6fa8','#c98a1f','#8461c9','#c0463d'];
const clipColor=(range,index)=>{if(!range[2]){const used=new Set(state.ranges.map(item=>item[2]).filter(Boolean));range[2]=CLIP_PALETTE.find(color=>!used.has(color))||CLIP_PALETTE[index%CLIP_PALETTE.length]}return range[2]};
const hexAlpha=(color,alpha)=>{const value=color.replace('#','');const rgb=value.length===3?[...value].map(x=>parseInt(x+x,16)):[value.slice(0,2),value.slice(2,4),value.slice(4,6)].map(x=>parseInt(x,16));return `rgba(${rgb.join(',')},${alpha})`};
function drawOutputMini(){if(!state.meta)return;const meta=state.meta,density=meta.density||[],peak=Math.max(1,[...density].sort((a,b)=>a-b)[Math.floor(density.length*.9)]||1),style=getComputedStyle(document.documentElement);document.querySelectorAll('#outputTrack .output-clip').forEach(clip=>{const range=state.ranges[+clip.dataset.index],canvas=clip.querySelector('canvas'),box=canvas.getBoundingClientRect(),d=devicePixelRatio||1;if(!range||!box.width)return;canvas.width=Math.round(box.width*d);canvas.height=Math.round(box.height*d);const x=canvas.getContext('2d'),color=clipColor(range,+clip.dataset.index),from=Math.floor(range[0]/meta.ticks*density.length),to=Math.max(from+1,Math.ceil(range[1]/meta.ticks*density.length)),span=Math.max(1,to-from),unit=box.width/span;x.setTransform(d,0,0,d,0,0);x.clearRect(0,0,box.width,box.height);x.fillStyle=hexAlpha(color,.88);x.fillRect(0,0,box.width,box.height);x.fillStyle='rgba(0,0,0,.46)';for(let bin=from;bin<to;bin++){const h=box.height*(.14+.7*Math.min(1,(density[bin]||0)/peak));x.fillRect((bin-from)*unit+1,box.height-h,Math.max(1,unit-1),h)}for(const event of state.events){const spec=EVENT_STYLE[event.kind];if(!spec||event.tick<range[0]||event.tick>range[1]||(state.eventFilters&&!state.eventFilters.has(event.kind)))continue;const px=(event.tick-range[0])/(range[1]-range[0])*box.width,h=Math.max(12,box.height*spec[2]);x.fillStyle=style.getPropertyValue(spec[1]);x.fillRect(px-1.5,box.height-h,3,h)}})}
const renderSegmentsRich=renderSegments;
renderSegments=()=>{state.ranges.forEach(clipColor);renderSegmentsRich();const meta=state.meta,total=state.ranges.reduce((sum,range)=>sum+range[1]-range[0],0)||1;$('#savedRanges').innerHTML=state.ranges.map((range,index)=>`<i class="range-mark" style="left:${range[0]/meta.ticks*100}%;width:${(range[1]-range[0])/meta.ticks*100}%;background:${clipColor(range,index)}"></i>`).join('');$('#outputTrack').innerHTML=state.ranges.map((range,index)=>{const width=(range[1]-range[0])/total*100;return `<div class="output-clip" draggable="true" data-index="${index}" style="flex:0 0 ${width}%;width:${width}%;--clip:${clipColor(range,index)}"><b class="output-order">#${index+1}</b><canvas aria-hidden="true"></canvas><span class="output-range">${fmt(range[0]/meta.tickRate)} → ${fmt(range[1]/meta.tickRate)}</span></div>`}).join('');const points=[0];state.ranges.reduce((sum,range)=>(points.push(sum+range[1]-range[0]),sum+range[1]-range[0]),0);const axisWidth=$('#outputTrack').clientWidth||600;$('#outputTickAxis').innerHTML=points.map((value,index)=>{const previous=points[index-1]??value,next=points[index+1]??value,room=Math.min(value-previous,next-value)/total*axisWidth;const hidden=index>0&&index<points.length-1&&room<52;return `<span class="${hidden?'axis-hidden':''}" style="left:${value/total*100}%;transform:translateX(${index===0?'0':index===points.length-1?'-100%':'-50%'})">${value.toLocaleString(locale())}</span>`}).join('');$('#outputTimeAxis').innerHTML='';drawOutputMini()};
const drawWithOutputMini=draw;draw=()=>{drawWithOutputMini();drawOutputMini()};
$('#outputTrack').addEventListener('mousemove',event=>{const clip=event.target.closest('.output-clip'),meta=state.meta;if(!clip||!meta)return;const range=state.ranges[+clip.dataset.index],rect=clip.getBoundingClientRect(),tick=range[0]+Math.max(0,Math.min(1,(event.clientX-rect.left)/rect.width))*(range[1]-range[0]),near=state.events.filter(item=>EVENT_STYLE[item.kind]&&item.tick>=range[0]&&item.tick<=range[1]&&(!state.eventFilters||state.eventFilters.has(item.kind))).reduce((best,item)=>!best||Math.abs(item.tick-tick)<Math.abs(best.tick-tick)?item:best,null);if(!near||Math.abs(near.tick-tick)>(range[1]-range[0])/Math.max(20,rect.width/5)){$('#eventTip').style.display='none';return}showEventTip(near,event.clientX,event.clientY)});
$('#outputTrack').addEventListener('mouseleave',()=>$('#eventTip').style.display='none');
$('#downloadMontage').onclick=e=>{if(!state.ranges.length)return say(tr('addFirst'),true);post('/api/edit',{id:state.id,ranges:state.ranges.map(range=>range.slice(0,2)),name:montageName.value},e.currentTarget)};
</script></body>""",
)


class DemoServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address, workspace):
        super().__init__(address, DemoHandler)
        self.workspace = Path(workspace)
        self.sessions = {}  # ponytail: local single-user process; add expiry only for remote hosting.
        self.lock = threading.Lock()
        for directory in self.workspace.rglob("*"):
            if not directory.is_dir() or not re.fullmatch(r"[0-9a-f]{32}", directory.name):
                continue
            demos = sorted(directory.glob("*.dem"), key=lambda path: path.stat().st_mtime)
            if not demos:
                continue
            try:
                self.sessions[directory.name] = {
                    "dir": directory,
                    "info": read_demo(demos[0]),
                    "voice": directory / "voice",
                }
            except (OSError, ValueError):
                continue


class DemoHandler(BaseHTTPRequestHandler):
    server: DemoServer

    def log_message(self, fmt, *args):
        print(f"[{self.log_date_time_string()}] {fmt % args}")

    def reply(self, status, body, content_type="application/json; charset=utf-8", headers=None):
        if isinstance(body, str):
            body = body.encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        for key, value in headers or []:
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(body)

    def json(self, status, value):
        self.reply(status, json.dumps(value, ensure_ascii=False).encode("utf-8"))

    def fail(self, error, status=400):
        self.json(status, {"error": str(error)})

    def session(self, query=None, payload=None):
        session_id = (payload or {}).get("id") or urllib.parse.parse_qs(query or "").get("id", [""])[0]
        if not re.fullmatch(r"[0-9a-f]{32}", session_id):
            raise ValueError("invalid session")
        with self.server.lock:
            session = self.server.sessions.get(session_id)
        if not session:
            raise ValueError("demo session expired")
        return session_id, session

    def do_GET(self):
        parsed = urllib.parse.urlsplit(self.path)
        try:
            if parsed.path == "/":
                return self.reply(200, HTML, "text/html; charset=utf-8")
            if parsed.path == "/lame.min.js":
                return self.reply(
                    200,
                    Path(__file__).with_name("lame.min.js").read_bytes(),
                    "application/javascript; charset=utf-8",
                )
            if parsed.path == "/api/session":
                session_id, session = self.session(query=parsed.query)
                return self.json(200, {"id": session_id, "meta": demo_meta(session["info"])})
            if parsed.path == "/api/voices":
                _id, session = self.session(query=parsed.query)
                players, all_players, events = extract_demo_index(session["info"]["path"], session["voice"])
                return self.json(200, {"players": players, "allPlayers": all_players, "events": events})
            self.fail("not found", 404)
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            self.fail(error)

    def read_json(self):
        length = int(self.headers.get("Content-Length", "0"))
        if not 0 < length < 2_000_000:
            raise ValueError("invalid request size")
        return json.loads(self.rfile.read(length))

    def do_POST(self):
        parsed = urllib.parse.urlsplit(self.path)
        try:
            if parsed.path == "/api/upload":
                return self.upload(parsed.query)
            payload = self.read_json()
            session_id, session = self.session(payload=payload)
            if parsed.path == "/api/edit":
                ranges = payload.get("ranges", [])
                default = session["info"]["path"].stem + "-edit"
                name = safe_name(str(payload.get("name") or default), default)
                name = name.removesuffix(".dem").replace(".", "-").strip(" -") or default
                target = session["dir"] / f"{name}.dem"
                write_edit(session["info"], ranges, target)
                return self.send_file(target)
            if parsed.path == "/api/voice":
                return self.send_voices(session_id, session, payload)
            self.fail("not found", 404)
        except (OSError, TypeError, ValueError, KeyError, json.JSONDecodeError, subprocess.SubprocessError) as error:
            self.fail(error)

    def upload(self, query):
        length = int(self.headers.get("Content-Length", "0"))
        if not 0 < length <= MAX_UPLOAD:
            raise ValueError("demo is empty or larger than 8 GiB")
        original = urllib.parse.parse_qs(query).get("name", ["demo.dem"])[0]
        if not original.lower().endswith(".dem"):
            raise ValueError("only .dem files are accepted")
        session_id = uuid.uuid4().hex
        directory = self.server.workspace / session_id
        directory.mkdir()
        path = directory / (safe_name(Path(original).name, "demo.dem") + ".part")
        remaining = length
        with path.open("wb") as output:
            while remaining:
                chunk = self.rfile.read(min(1024 * 1024, remaining))
                if not chunk:
                    raise ValueError("upload ended early")
                output.write(chunk)
                remaining -= len(chunk)
        demo_path = path.with_suffix("")
        os.replace(path, demo_path)
        try:
            info = read_demo(demo_path)
        except Exception:
            demo_path.unlink(missing_ok=True)
            raise
        session = {"dir": directory, "info": info, "voice": directory / "voice"}
        with self.server.lock:
            self.server.sessions[session_id] = session
        self.json(200, {"id": session_id, "meta": demo_meta(info)})

    def send_file(self, path: Path):
        size = path.stat().st_size
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(size))
        quoted = urllib.parse.quote(path.name)
        self.send_header("Content-Disposition", f"attachment; filename*=UTF-8''{quoted}")
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        with path.open("rb") as source:
            while chunk := source.read(1024 * 1024):
                self.wfile.write(chunk)

    def send_voices(self, session_id, session, payload):
        players = extract_voice_index(session["info"]["path"], session["voice"])
        known = {player["client"]: player for player in players}
        clients = list(dict.fromkeys(int(value) for value in payload.get("clients", [])))
        if not clients or any(client not in known for client in clients):
            raise ValueError("invalid player selection")
        keep = bool(payload.get("keepGaps", True))
        outputs = []
        for client in clients:
            player = known[client]
            name = safe_name(player["name"], f"client-{client}")
            target = session["dir"] / f"{name}{'.with-pauses' if keep else '.compact'}.ogg"
            build_player_ogg(
                session["voice"] / "frames" / f"{client}.txt",
                target,
                session["info"]["tick_rate"],
                keep,
            )
            outputs.append(target)
        if len(outputs) == 1:
            return self.send_file(outputs[0])
        archive = session["dir"] / f"voices-{session_id[:8]}.zip"
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as bundle:
            for path in outputs:
                bundle.write(path, path.name)
        self.send_file(archive)


def split_demo(source: Path, output: Path, parts=None, seconds=None):
    info = read_demo(source)
    if seconds is not None:
        if seconds <= 0:
            raise ValueError("--seconds must be positive")
        span = max(1, round(seconds * info["tick_rate"]))
    else:
        if not parts or parts < 2 or parts > info["ticks"]:
            raise ValueError("--parts must be between 2 and the tick count")
        span = math.ceil(info["ticks"] / parts)
    ranges = [(start, min(start + span, info["ticks"])) for start in range(0, info["ticks"], span)]
    output.mkdir(parents=True, exist_ok=True)
    targets = []
    for index, current in enumerate(ranges, 1):
        target = output / f"{source.stem}.part{index:0{len(str(len(ranges)))}d}.dem"
        if target.exists():
            raise ValueError(f"output exists: {target}")
        targets.append(write_edit(info, [current], target))
    return targets


def export_voices(source: Path, output: Path, queries, all_players: bool, keep_gaps: bool, audio_format="ogg"):
    info = read_demo(source)
    output.mkdir(parents=True, exist_ok=True)
    with temporary_directory("tf2_voice_") as temporary:
        dump = Path(temporary)
        players = extract_voice_index(source, dump)
        selected = players if all_players else [
            player
            for player in players
            if any(
                (query.isdigit() and int(query) == player["client"])
                or (not query.isdigit() and query.casefold() in player["name"].casefold())
                for query in queries
            )
        ]
        if not selected:
            raise ValueError("no matching players with voice data")
        targets = []
        for player in selected:
            name = safe_name(player["name"], f"client-{player['client']}")
            ogg = output / f"{name}.ogg"
            build_player_ogg(
                dump / "frames" / f"{player['client']}.txt",
                ogg,
                info["tick_rate"],
                keep_gaps,
            )
            if audio_format == "ogg":
                target = ogg
            else:
                ffmpeg = shutil.which("ffmpeg")
                if not ffmpeg:
                    raise ValueError(f"--format {audio_format} needs ffmpeg in PATH")
                target = ogg.with_suffix("." + audio_format)
                subprocess.run([ffmpeg, "-y", "-i", str(ogg), str(target)], check=True)
                ogg.unlink(missing_ok=True)
            targets.append(target)
        return targets


def parse_time_range(value: str):
    try:
        start, end = (float(part) for part in value.split(":", 1))
    except ValueError as error:
        raise ValueError("--range must use START:END seconds") from error
    if start < 0 or end <= start:
        raise ValueError("--range end must be greater than start")
    return start, end


def self_check():
    assert steam_crc(b"abc123") == 3473062748
    assert opus_samples_48k(SILENCE_OPUS) == 960
    command = bytearray(
        struct.pack("<BiII", CMD_USER, 0, 7, 5) + ((7 << 1) | 1).to_bytes(5, "little")
    )
    rewrite_user_sequence(command, 11)
    assert struct.unpack_from("<I", command, 5)[0] == 11
    assert (int.from_bytes(command[13:18], "little") >> 1) & 0xFFFFFFFF == 11
    assert sequence_delta(1, 0xFFFFFFFF) == 2
    assert normalize_ranges([(5, 9), (1, 3), (3, 6)], 10) == [(1, 9)]
    assert normalize_ranges([(5, 9), (1, 3)], 10, ordered=True) == [(5, 9), (1, 3)]
    assert [bridge_tick(10, index, 5, 3) for index in range(5)] == [10, 10, 11, 11, 12]
    page = ogg_page(b"x", 1, 0, 0, 2)
    stored, clean = struct.unpack_from("<I", page, 22)[0], bytearray(page)
    struct.pack_into("<I", clean, 22, 0)
    assert page[:4] == b"OggS" and ogg_crc(clean) == stored
    HTML.encode("utf-8").decode("utf-8")
    print("self-check: OK")


def main():
    parser = argparse.ArgumentParser(description="TF2 POV/SourceTV demo editor and voice exporter")
    commands = parser.add_subparsers(dest="command")
    serve = commands.add_parser("serve", help="start the local web editor")
    serve.add_argument("--host", default="127.0.0.1")
    serve.add_argument("--port", type=int, default=8765)
    serve.add_argument(
        "--workspace",
        type=Path,
        default=DEFAULT_WORKSPACE,
        help="directory for temporary web sessions (default: .work next to the script)",
    )
    serve.add_argument("--no-browser", action="store_true")
    info_cmd = commands.add_parser("info", help="show demo metadata")
    info_cmd.add_argument("demo", type=Path)
    cut = commands.add_parser("cut", help="extract one time range")
    cut.add_argument("demo", type=Path)
    cut.add_argument("--from", dest="start", type=float, required=True)
    cut.add_argument("--to", dest="end", type=float, required=True)
    cut.add_argument("-o", "--output", type=Path)
    montage = commands.add_parser(
        "montage", help="join ordered time ranges from one POV/SourceTV demo"
    )
    montage.add_argument("demo", type=Path)
    montage.add_argument(
        "--range",
        action="append",
        required=True,
        help="START:END seconds; repeat in output order",
    )
    montage.add_argument("-o", "--output", type=Path)
    source_test = commands.add_parser(
        "source-montage-test", help="experimental SourceTV raw-replay montage (CLI only)"
    )
    source_test.add_argument("demo", type=Path)
    source_test.add_argument(
        "--range", action="append", required=True, help="START:END seconds; repeat in output order"
    )
    source_test.add_argument("-o", "--output", type=Path)
    split = commands.add_parser("split", help="split into independent parts")
    split.add_argument("demo", type=Path)
    group = split.add_mutually_exclusive_group()
    group.add_argument("--parts", type=int, default=5)
    group.add_argument("--seconds", type=float)
    split.add_argument("-o", "--output-dir", type=Path)
    voice = commands.add_parser("voice", help="export player voices")
    voice.add_argument("demo", type=Path)
    voice.add_argument("--player", action="append", default=[], help="client id or name fragment")
    voice.add_argument("--all", action="store_true", help="export every player with voice data")
    voice.add_argument("--no-gaps", action="store_true", help="remove pauses between voice packets")
    voice.add_argument("--format", choices=("ogg", "wav", "mp3"), default="ogg")
    voice.add_argument("--archive", action="store_true", help="also create voices.zip")
    voice.add_argument("-o", "--output-dir", type=Path)
    commands.add_parser("build-helper", help="build the Rust POV and voice helpers")
    commands.add_parser("self-test", help="run the built-in check")
    args = parser.parse_args()
    try:
        if args.command in (None, "serve"):
            host, port = getattr(args, "host", "127.0.0.1"), getattr(args, "port", 8765)
            workspace_root = Path(getattr(args, "workspace", DEFAULT_WORKSPACE)).resolve()
            workspace_root.mkdir(parents=True, exist_ok=True)
            server = DemoServer((host, port), workspace_root)
            url = f"http://{host}:{port}"
            print(f"TF2 Demo Tools: {url} (Ctrl+C to stop)")
            if not getattr(args, "no_browser", False):
                threading.Timer(0.4, webbrowser.open, args=(url,)).start()
            server.serve_forever()
        elif args.command == "self-test":
            self_check()
        elif args.command == "info":
            print(json.dumps(demo_meta(read_demo(args.demo)), ensure_ascii=False, indent=2))
        elif args.command == "cut":
            info = read_demo(args.demo)
            target = args.output or args.demo.with_name(args.demo.stem + ".cut.dem")
            start, end = round(args.start * info["tick_rate"]), round(args.end * info["tick_rate"])
            write_edit(info, [(start, end)], target)
            print(target)
        elif args.command == "montage":
            info = read_demo(args.demo)
            target = args.output or args.demo.with_name(args.demo.stem + ".montage.dem")
            ranges = [(round(start * info["tick_rate"]), round(end * info["tick_rate"])) for start, end in map(parse_time_range, args.range)]
            if info["kind"] == "SourceTV":
                write_source_experiment(
                    info,
                    normalize_ranges(ranges, info["ticks"], ordered=True),
                    target,
                )
            else:
                write_edit(info, ranges, target)
            print(target)
        elif args.command == "source-montage-test":
            info = read_demo(args.demo)
            if info["kind"] != "SourceTV":
                raise ValueError("source-montage-test accepts SourceTV demos only")
            target = args.output or args.demo.with_name(args.demo.stem + ".source-test.dem")
            ranges = [(round(start * info["tick_rate"]), round(end * info["tick_rate"])) for start, end in map(parse_time_range, args.range)]
            write_source_experiment(
                info,
                normalize_ranges(ranges, info["ticks"], ordered=True),
                target,
            )
            print(target)
        elif args.command == "split":
            output = args.output_dir or args.demo.with_name(args.demo.stem + "_parts")
            targets = split_demo(args.demo, output, args.parts, args.seconds)
            print(f"Created {len(targets)} parts in {output}")
        elif args.command == "voice":
            if not args.all and not args.player:
                raise ValueError("use --player NAME/ID or --all")
            output = args.output_dir or args.demo.with_name(args.demo.stem + "_voices")
            targets = export_voices(args.demo, output, args.player, args.all, not args.no_gaps, args.format)
            if args.archive:
                archive = output / "voices.zip"
                with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as bundle:
                    for target in targets:
                        bundle.write(target, target.name)
                print(archive)
            print(f"Created {len(targets)} voice tracks in {output}")
        elif args.command == "build-helper":
            helper = Path(__file__).parent / "helper"
            if helper.is_dir():
                subprocess.run(["cargo", "build", "--release"], cwd=helper, check=True)
            print(voice_helper())
            print(helper_binary("pov_cut"))
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
