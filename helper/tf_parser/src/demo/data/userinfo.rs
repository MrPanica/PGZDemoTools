use crate::demo::message::packetentities::EntityId;
use crate::demo::packet::stringtable::{ExtraData, StringTableEntry};
use crate::demo::parser::analyser::UserId;
use crate::{ReadResult, Stream};
use bitbuffer::{BitRead, BitReadBuffer, BitReadStream, BitWrite, BitWriteStream, LittleEndian};
use encoding_rs::WINDOWS_1251;

#[derive(BitRead, Debug)]
struct RawPlayerInfo {
    pub name_bytes: [u8; 32],
    pub user_id: u32,
    #[size = 32]
    pub steam_id: String,
    pub extra: u32, // all my sources say these 4 bytes don't exist
    pub friends_id: u32,
    pub friends_name_bytes: [u8; 32], // seem to all be 0 now
    pub is_fake_player: u8,
    pub is_hl_tv: u8,
    pub is_replay: u8,
    pub custom_file: [u32; 4],
    pub files_downloaded: u32,
    pub more_extra: u8,
}

#[derive(BitWrite, Debug, Clone, Default)]
pub struct PlayerInfo {
    #[size = 32]
    pub name: String,
    pub user_id: UserId,
    #[size = 32]
    pub steam_id: String,
    pub extra: u32, // all my sources say these 4 bytes don't exist
    pub friends_id: u32,
    pub friends_name_bytes: [u8; 32], // seem to all be 0 now
    pub is_fake_player: u8,
    pub is_hl_tv: u8,
    pub is_replay: u8,
    pub custom_file: [u32; 4],
    pub files_downloaded: u32,
    pub more_extra: u8,
}

impl From<RawPlayerInfo> for PlayerInfo {
    fn from(raw: RawPlayerInfo) -> Self {
        PlayerInfo {
            name: decode_name(&raw.name_bytes),
            user_id: raw.user_id.into(),
            steam_id: raw.steam_id,
            extra: raw.extra,
            friends_id: raw.friends_id,
            friends_name_bytes: raw.friends_name_bytes,
            is_fake_player: raw.is_fake_player,
            is_hl_tv: raw.is_hl_tv,
            is_replay: raw.is_replay,
            custom_file: raw.custom_file,
            files_downloaded: raw.files_downloaded,
            more_extra: raw.more_extra,
        }
    }
}

fn decode_name(bytes: &[u8]) -> String {
    let bytes = bytes.split(|byte| *byte == 0).next().unwrap_or_default();
    match std::str::from_utf8(bytes) {
        Ok(name) => name.to_owned(),
        Err(error) if error.error_len().is_none() => {
            String::from_utf8_lossy(bytes.get(..error.valid_up_to()).unwrap_or_default())
                .into_owned()
        }
        Err(_) => WINDOWS_1251.decode(bytes).0.replace('\u{fffd}', "?"),
    }
}

#[derive(Clone, Debug, Default)]
pub struct UserInfo {
    pub entity_id: EntityId,
    pub player_info: PlayerInfo,
}

impl UserInfo {
    pub fn parse_from_string_table(
        index: u16,
        text: Option<&str>,
        data: Option<Stream>,
    ) -> ReadResult<Option<Self>> {
        if let Some(mut data) = data {
            // extra decode step to gracefully handle malformed utf8 names
            let raw_info: RawPlayerInfo = data.read()?;

            match text
                .map(|text| text.parse::<u32>().map(|id| (id + 1).into()))
                .unwrap_or_else(|| Ok((index as u32 + 1).into()))
            {
                Ok(entity_id) if !raw_info.steam_id.is_empty() => Ok(Some(UserInfo {
                    player_info: raw_info.into(),
                    entity_id,
                })),
                _ => Ok(None),
            }
        } else {
            Ok(None)
        }
    }

    pub fn encode_to_string_table(&self) -> ReadResult<StringTableEntry<'static>> {
        let text = format!("{}", self.entity_id);
        let mut extra_data = Vec::with_capacity(132);
        {
            let mut stream = BitWriteStream::new(&mut extra_data, LittleEndian);
            self.player_info.write(&mut stream)?;
        }

        Ok(StringTableEntry {
            text: Some(text.into()),
            extra_data: Some(ExtraData::new(BitReadStream::new(
                BitReadBuffer::new_owned(extra_data, LittleEndian),
            ))),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::decode_name;

    #[test]
    fn decodes_legacy_and_truncated_names_without_replacement_characters() {
        assert_eq!(
            decode_name(&[0xcf, 0xf0, 0xe8, 0xe2, 0xe5, 0xf2, 0]),
            "Привет"
        );
        assert_eq!(decode_name(&[b'A', 0xf0, 0x9d, 0x94]), "A");
    }
}
