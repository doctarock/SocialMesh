//! Byte encoding of [`Command`] and [`Event`], for host drivers written in
//! other languages (Kotlin on Android, Swift on iOS). One message per call;
//! the caller knows where it ends.
//!
//! Integers are big-endian. `str` is a u16 length then UTF-8. A final
//! `bytes` field runs to the end of the message.
//!
//! ```text
//! Commands                                Events
//! 1 Listen                                1 Listening      psm u16
//! 2 Advertise     bytes                   2 Advert         rssi i16 (0x7fff: none) | device str | bytes
//! 3 StopAdvertising                       3 Incoming       link u64 | mtu u32 | device str
//! 4 Scan                                  4 Connected      link u64 | mtu u32
//! 5 StopScan                              5 ConnectFailed  link u64 | reason str
//! 6 Connect       link u64 | psm u16 | device str
//! 7 Send          link u64 | bytes        6 Data           link u64 | bytes
//! 8 Close         link u64                7 Closed         link u64
//! 9 RotateAddress
//! ```

use crate::{Command, Event};

const NO_RSSI: i16 = 0x7fff;

struct W(Vec<u8>);

impl W {
    fn new(tag: u8) -> W {
        W(vec![tag])
    }
    fn u16(mut self, v: u16) -> W {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u32(mut self, v: u32) -> W {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u64(mut self, v: u64) -> W {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn str(self, s: &str) -> W {
        let b = &s.as_bytes()[..s.len().min(u16::MAX as usize)];
        let mut w = self.u16(b.len() as u16);
        w.0.extend_from_slice(b);
        w
    }
    fn rest(mut self, b: &[u8]) -> Vec<u8> {
        self.0.extend_from_slice(b);
        self.0
    }
}

struct R<'a>(&'a [u8]);

impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u16()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
    fn rest(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0).to_vec()
    }
    /// Only if nothing is left over.
    fn end<T>(&self, v: T) -> Option<T> {
        self.0.is_empty().then_some(v)
    }
}

pub fn encode_command(c: &Command) -> Vec<u8> {
    match c {
        Command::Listen => vec![1],
        Command::Advertise { service_data } => W::new(2).rest(service_data),
        Command::StopAdvertising => vec![3],
        Command::Scan => vec![4],
        Command::StopScan => vec![5],
        Command::Connect { link, device, psm } => W::new(6).u64(*link).u16(*psm).str(device).0,
        Command::Send { link, data } => W::new(7).u64(*link).rest(data),
        Command::Close { link } => W::new(8).u64(*link).0,
        Command::RotateAddress => vec![9],
    }
}

pub fn decode_command(b: &[u8]) -> Option<Command> {
    let (&tag, body) = b.split_first()?;
    let mut r = R(body);
    match tag {
        1 => r.end(Command::Listen),
        2 => Some(Command::Advertise { service_data: r.rest() }),
        3 => r.end(Command::StopAdvertising),
        4 => r.end(Command::Scan),
        5 => r.end(Command::StopScan),
        6 => {
            let (link, psm, device) = (r.u64()?, r.u16()?, r.str()?);
            r.end(Command::Connect { link, device, psm })
        }
        7 => {
            let link = r.u64()?;
            Some(Command::Send { link, data: r.rest() })
        }
        8 => {
            let link = r.u64()?;
            r.end(Command::Close { link })
        }
        9 => r.end(Command::RotateAddress),
        _ => None,
    }
}

pub fn encode_event(e: &Event) -> Vec<u8> {
    match e {
        Event::Listening { psm } => W::new(1).u16(*psm).0,
        Event::Advert { device, service_data, rssi } => {
            W::new(2).u16(rssi.unwrap_or(NO_RSSI) as u16).str(device).rest(service_data)
        }
        Event::Incoming { link, device, mtu } => W::new(3).u64(*link).u32(*mtu as u32).str(device).0,
        Event::Connected { link, mtu } => W::new(4).u64(*link).u32(*mtu as u32).0,
        Event::ConnectFailed { link, reason } => W::new(5).u64(*link).str(reason).0,
        Event::Data { link, data } => W::new(6).u64(*link).rest(data),
        Event::Closed { link } => W::new(7).u64(*link).0,
    }
}

pub fn decode_event(b: &[u8]) -> Option<Event> {
    let (&tag, body) = b.split_first()?;
    let mut r = R(body);
    match tag {
        1 => {
            let psm = r.u16()?;
            r.end(Event::Listening { psm })
        }
        2 => {
            let rssi = r.u16()? as i16;
            let device = r.str()?;
            Some(Event::Advert { device, service_data: r.rest(), rssi: (rssi != NO_RSSI).then_some(rssi) })
        }
        3 => {
            let (link, mtu, device) = (r.u64()?, r.u32()?, r.str()?);
            r.end(Event::Incoming { link, device, mtu: mtu as usize })
        }
        4 => {
            let (link, mtu) = (r.u64()?, r.u32()?);
            r.end(Event::Connected { link, mtu: mtu as usize })
        }
        5 => {
            let (link, reason) = (r.u64()?, r.str()?);
            r.end(Event::ConnectFailed { link, reason })
        }
        6 => {
            let link = r.u64()?;
            Some(Event::Data { link, data: r.rest() })
        }
        7 => {
            let link = r.u64()?;
            r.end(Event::Closed { link })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HOST_LINK;

    #[test]
    fn every_message_roundtrips() {
        let commands = [
            Command::Listen,
            Command::Advertise { service_data: vec![1, 0, 0, 0x81, 1, 2, 3, 4, 5, 6] },
            Command::StopAdvertising,
            Command::Scan,
            Command::StopScan,
            Command::Connect { link: 7, device: "AA:BB:CC:DD:EE:FF".into(), psm: 0x0081 },
            Command::Send { link: 7, data: vec![9; 300] },
            Command::Send { link: 7, data: vec![] },
            Command::Close { link: 7 },
            Command::RotateAddress,
        ];
        for c in commands {
            assert_eq!(decode_command(&encode_command(&c)), Some(c));
        }
        let events = [
            Event::Listening { psm: 0x0083 },
            Event::Advert { device: "01:02:03:04:05:06".into(), service_data: vec![1; 10], rssi: Some(-71) },
            Event::Advert { device: "iOS-uuid-ish".into(), service_data: vec![], rssi: None },
            Event::Incoming { link: HOST_LINK | 3, device: "x".into(), mtu: 2048 },
            Event::Connected { link: 7, mtu: 244 },
            Event::ConnectFailed { link: 7, reason: "timeout é".into() },
            Event::Data { link: HOST_LINK | 3, data: vec![0, 1, 2] },
            Event::Closed { link: 7 },
        ];
        for e in events {
            assert_eq!(decode_event(&encode_event(&e)), Some(e));
        }
    }

    /// Fixed vectors, shared with the Android driver's unit test
    /// (android/app/src/test/.../BleCodecTest.kt). Change both together.
    #[test]
    fn golden_vectors() {
        let h = |s: &str| hex::decode(s.replace(' ', "")).unwrap();
        let events = [
            (Event::Listening { psm: 0x81 }, "01 0081"),
            (Event::Advert { device: "AB".into(), service_data: vec![1, 2], rssi: Some(-60) }, "02 ffc4 0002 4142 0102"),
            (Event::Incoming { link: HOST_LINK | 1, device: "AB".into(), mtu: 2048 }, "03 8000000000000001 00000800 0002 4142"),
            (Event::Connected { link: 5, mtu: 244 }, "04 0000000000000005 000000f4"),
            (Event::ConnectFailed { link: 5, reason: "no".into() }, "05 0000000000000005 0002 6e6f"),
            (Event::Data { link: 5, data: vec![9, 8] }, "06 0000000000000005 0908"),
            (Event::Closed { link: 5 }, "07 0000000000000005"),
        ];
        for (e, v) in events {
            assert_eq!(encode_event(&e), h(v), "{e:?}");
        }
        let commands = [
            (Command::Listen, "01"),
            (Command::Advertise { service_data: vec![1, 2] }, "02 0102"),
            (Command::Scan, "04"),
            (Command::Connect { link: 5, device: "AB".into(), psm: 0x81 }, "06 0000000000000005 0081 0002 4142"),
            (Command::Send { link: 5, data: vec![1, 2, 3] }, "07 0000000000000005 010203"),
            (Command::Close { link: 5 }, "08 0000000000000005"),
            (Command::RotateAddress, "09"),
        ];
        for (c, v) in commands {
            assert_eq!(encode_command(&c), h(v), "{c:?}");
        }
    }

    #[test]
    fn junk_is_rejected() {
        assert_eq!(decode_command(&[]), None);
        assert_eq!(decode_command(&[42]), None);
        assert_eq!(decode_command(&[1, 0]), None); // trailing byte
        assert_eq!(decode_command(&[6, 0, 0]), None); // truncated
        assert_eq!(decode_event(&[3, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0, 5, b'a']), None); // short string
        assert_eq!(decode_event(&[5, 0, 0, 0, 0, 0, 0, 0, 1, 0, 2, 0xff, 0xfe]), None); // not UTF-8
    }
}
