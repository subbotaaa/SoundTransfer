//! Wire-формат: контракт между отправителем (Windows) и приёмником (Mac).
//!
//! UDP-датаграмма = заголовок 24 байта (little-endian) + PCM-payload
//! (interleaved-фреймы; их число выводится из длины датаграммы).

pub const MAGIC: [u8; 4] = *b"SND1";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 24;
pub const DEFAULT_PORT: u16 = 48100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketType {
    Audio,
    Hello,
    Bye,
}

impl PacketType {
    pub fn as_u8(self) -> u8 {
        match self {
            PacketType::Audio => 0,
            PacketType::Hello => 1,
            PacketType::Bye => 2,
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(PacketType::Audio),
            1 => Some(PacketType::Hello),
            2 => Some(PacketType::Bye),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireFormat {
    S16le,
    F32le,
}

impl WireFormat {
    pub fn bytes_per_sample(self) -> usize {
        match self {
            WireFormat::S16le => 2,
            WireFormat::F32le => 4,
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            WireFormat::S16le => 0,
            WireFormat::F32le => 1,
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(WireFormat::S16le),
            1 => Some(WireFormat::F32le),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub ptype: PacketType,
    pub channels: u8,
    pub format: WireFormat,
    pub sample_rate: u32,
    /// +1 на каждый AUDIO-пакет, с переполнением по кругу.
    pub seq: u32,
    /// Номер первого фрейма пакета с начала потока — переживает wrap seq.
    pub sample_pos: u64,
}

impl Header {
    pub fn write(&self, buf: &mut [u8]) {
        buf[0..4].copy_from_slice(&MAGIC);
        buf[4] = VERSION;
        buf[5] = self.ptype.as_u8();
        buf[6] = self.channels;
        buf[7] = self.format.as_u8();
        buf[8..12].copy_from_slice(&self.sample_rate.to_le_bytes());
        buf[12..16].copy_from_slice(&self.seq.to_le_bytes());
        buf[16..24].copy_from_slice(&self.sample_pos.to_le_bytes());
    }

    pub fn parse(buf: &[u8]) -> Option<Header> {
        if buf.len() < HEADER_LEN || buf[0..4] != MAGIC || buf[4] != VERSION {
            return None;
        }
        Some(Header {
            ptype: PacketType::from_u8(buf[5])?,
            channels: buf[6],
            format: WireFormat::from_u8(buf[7])?,
            sample_rate: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            seq: u32::from_le_bytes(buf[12..16].try_into().unwrap()),
            sample_pos: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_header() -> Header {
        Header {
            ptype: PacketType::Audio,
            channels: 2,
            format: WireFormat::S16le,
            sample_rate: 48000,
            seq: 0xDEAD_BEEF,
            sample_pos: 0x1122_3344_5566_7788,
        }
    }

    #[test]
    fn round_trip() {
        let h = sample_header();
        let mut buf = [0u8; HEADER_LEN];
        h.write(&mut buf);
        assert_eq!(Header::parse(&buf), Some(h));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = [0u8; HEADER_LEN];
        sample_header().write(&mut buf);
        buf[0] = b'X';
        assert_eq!(Header::parse(&buf), None);
    }

    #[test]
    fn rejects_bad_version() {
        let mut buf = [0u8; HEADER_LEN];
        sample_header().write(&mut buf);
        buf[4] = 99;
        assert_eq!(Header::parse(&buf), None);
    }

    #[test]
    fn rejects_short_buffer() {
        let mut buf = [0u8; HEADER_LEN];
        sample_header().write(&mut buf);
        assert_eq!(Header::parse(&buf[..HEADER_LEN - 1]), None);
    }

    #[test]
    fn rejects_unknown_ptype_and_format() {
        let mut buf = [0u8; HEADER_LEN];
        sample_header().write(&mut buf);
        buf[5] = 42;
        assert_eq!(Header::parse(&buf), None);
        sample_header().write(&mut buf);
        buf[7] = 42;
        assert_eq!(Header::parse(&buf), None);
    }
}
