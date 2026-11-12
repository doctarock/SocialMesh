//! A simulated Bluetooth environment: a host driver for any number of
//! radios placed on a plane. Adverts are heard within range, links carry at
//! most `mtu` bytes per packet, and links break when devices move apart.
//! Used by the tests, and handy for trying out a mesh on one machine.

use crate::{BleRadio, Command, Event, HOST_LINK};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub struct SimConfig {
    /// Devices further apart than this can't hear each other (metres, say).
    pub range: f64,
    /// Largest packet a link carries.
    pub mtu: usize,
    pub advert_interval: Duration,
    /// Throughput per radio, if limited.
    pub bytes_per_second: Option<u64>,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig { range: 30.0, mtu: 244, advert_interval: Duration::from_millis(100), bytes_per_second: None }
    }
}

struct Slot {
    address: String,
    pos: (f64, f64),
    advert: Option<Vec<u8>>,
    scanning: bool,
    psm: Option<u16>,
    events: mpsc::Sender<Event>,
    /// Commands seen, for tests that check what a radio did.
    log: Vec<Command>,
}

struct Air {
    config: SimConfig,
    slots: Vec<Option<Slot>>,
    /// (radio, link) → (other radio, its link)
    links: HashMap<(usize, u64), (usize, u64)>,
    next_host_link: u64,
}

impl Air {
    fn in_range(&self, a: usize, b: usize) -> bool {
        match (&self.slots[a], &self.slots[b]) {
            (Some(x), Some(y)) => distance(x.pos, y.pos) <= self.config.range,
            _ => false,
        }
    }

    fn events(&self, i: usize) -> Option<mpsc::Sender<Event>> {
        self.slots[i].as_ref().map(|s| s.events.clone())
    }

    /// Remove a link from both ends; the far end is told.
    fn cut(&mut self, end: (usize, u64), out: &mut Vec<(mpsc::Sender<Event>, Event)>, tell_near: bool) {
        if let Some(far) = self.links.remove(&end) {
            self.links.remove(&far);
            if let Some(tx) = self.events(far.0) {
                out.push((tx, Event::Closed { link: far.1 }));
            }
            if tell_near {
                if let Some(tx) = self.events(end.0) {
                    out.push((tx, Event::Closed { link: end.1 }));
                }
            }
        }
    }
}

fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn random_address() -> String {
    let b: [u8; 6] = rand::random();
    b.iter().map(|x| format!("{x:02X}")).collect::<Vec<_>>().join(":")
}

/// The shared air. Create inside a tokio runtime.
#[derive(Clone)]
pub struct SimAir(Arc<Mutex<Air>>);

/// One simulated device, for moving it around and looking at its radio.
#[derive(Clone)]
pub struct SimDevice {
    air: Arc<Mutex<Air>>,
    idx: usize,
}

impl SimAir {
    pub fn new(config: SimConfig) -> SimAir {
        let every = config.advert_interval;
        let air = Arc::new(Mutex::new(Air { config, slots: Vec::new(), links: HashMap::new(), next_host_link: 1 }));
        let weak = Arc::downgrade(&air);
        tokio::spawn(ticker(weak, every));
        SimAir(air)
    }

    /// Put a new device with a radio at `(x, y)`.
    pub fn add(&self, x: f64, y: f64) -> (BleRadio, SimDevice) {
        let (radio, host) = BleRadio::new();
        let idx = {
            let mut air = self.0.lock().unwrap();
            air.slots.push(Some(Slot {
                address: random_address(),
                pos: (x, y),
                advert: None,
                scanning: false,
                psm: None,
                events: host.events,
                log: Vec::new(),
            }));
            air.slots.len() - 1
        };
        tokio::spawn(drive(self.0.clone(), idx, host.commands));
        (radio, SimDevice { air: self.0.clone(), idx })
    }
}

impl SimDevice {
    pub fn move_to(&self, x: f64, y: f64) {
        if let Some(s) = self.air.lock().unwrap().slots[self.idx].as_mut() {
            s.pos = (x, y);
        }
    }

    /// The radio's current hardware address.
    pub fn address(&self) -> String {
        self.air.lock().unwrap().slots[self.idx].as_ref().map(|s| s.address.clone()).unwrap_or_default()
    }

    /// The service data being advertised, if any.
    pub fn advert(&self) -> Option<Vec<u8>> {
        self.air.lock().unwrap().slots[self.idx].as_ref().and_then(|s| s.advert.clone())
    }

    /// Every command the radio was given, oldest first.
    pub fn commands(&self) -> Vec<Command> {
        self.air.lock().unwrap().slots[self.idx].as_ref().map(|s| s.log.clone()).unwrap_or_default()
    }

    /// Open links on this radio.
    pub fn link_count(&self) -> usize {
        self.air.lock().unwrap().links.keys().filter(|(i, _)| *i == self.idx).count()
    }
}

async fn drive(air: Arc<Mutex<Air>>, idx: usize, mut commands: mpsc::Receiver<Command>) {
    // Airtime owed. Paid in sleeps of at least 20 ms: timers are coarse (15 ms
    // on Windows), so sleeping per packet would be far slower than intended.
    let mut owed = Duration::ZERO;
    while let Some(c) = commands.recv().await {
        let mut out = Vec::new();
        let mut pace = None;
        {
            let mut a = air.lock().unwrap();
            let Some(me) = a.slots[idx].as_mut() else { break };
            if !matches!(c, Command::Send { .. }) {
                me.log.push(c.clone());
            }
            match c {
                Command::Listen => {
                    let psm = 0x80 + (idx as u16 % 0x7f);
                    me.psm = Some(psm);
                    out.push((me.events.clone(), Event::Listening { psm }));
                }
                Command::Advertise { service_data } => me.advert = Some(service_data),
                Command::StopAdvertising => me.advert = None,
                Command::Scan => me.scanning = true,
                Command::StopScan => me.scanning = false,
                Command::RotateAddress => me.address = random_address(),
                Command::Connect { link, device, psm } => {
                    let mtu = a.config.mtu;
                    let target = (0..a.slots.len()).find(|&j| {
                        j != idx
                            && a.slots[j].as_ref().is_some_and(|s| {
                                s.address == device && s.psm.is_some_and(|p| psm == 0 || psm == p)
                            })
                            && a.in_range(idx, j)
                    });
                    let my_events = a.events(idx).expect("present");
                    match target {
                        Some(j) => {
                            let far_link = HOST_LINK | a.next_host_link;
                            a.next_host_link += 1;
                            a.links.insert((idx, link), (j, far_link));
                            a.links.insert((j, far_link), (idx, link));
                            let my_addr = a.slots[idx].as_ref().expect("present").address.clone();
                            out.push((a.events(j).expect("present"), Event::Incoming { link: far_link, device: my_addr, mtu }));
                            out.push((my_events, Event::Connected { link, mtu }));
                        }
                        None => out.push((my_events, Event::ConnectFailed { link, reason: "no answer".into() })),
                    }
                }
                Command::Send { link, data } => {
                    let far = a.links.get(&(idx, link)).copied();
                    match far {
                        Some((j, far_link)) if data.len() <= a.config.mtu && a.in_range(idx, j) => {
                            pace = a.config.bytes_per_second.map(|r| Duration::from_secs_f64(data.len() as f64 / r as f64));
                            out.push((a.events(j).expect("present"), Event::Data { link: far_link, data }));
                        }
                        // Oversized packet or out of range: the link fails.
                        Some(_) => a.cut((idx, link), &mut out, true),
                        None => {}
                    }
                }
                Command::Close { link } => a.cut((idx, link), &mut out, false),
            }
        }
        for (tx, ev) in out {
            let _ = tx.send(ev).await;
        }
        if let Some(d) = pace {
            owed += d;
            if owed >= Duration::from_millis(20) {
                let start = tokio::time::Instant::now();
                tokio::time::sleep(owed).await;
                owed = owed.saturating_sub(start.elapsed());
            }
        }
    }
    // The radio was dropped: it leaves the air.
    let mut out = Vec::new();
    {
        let mut a = air.lock().unwrap();
        let mine: Vec<_> = a.links.keys().filter(|(i, _)| *i == idx).copied().collect();
        for end in mine {
            a.cut(end, &mut out, false);
        }
        a.slots[idx] = None;
    }
    for (tx, ev) in out {
        let _ = tx.send(ev).await;
    }
}

/// Deliver adverts to scanners in range, and break links that went out of range.
async fn ticker(air: Weak<Mutex<Air>>, every: Duration) {
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        let Some(air) = air.upgrade() else { return };
        let mut heard = Vec::new();
        let mut closed = Vec::new();
        {
            let mut a = air.lock().unwrap();
            let n = a.slots.len();
            for i in 0..n {
                let Some(listener) = a.slots[i].as_ref().filter(|s| s.scanning) else { continue };
                for j in (0..n).filter(|&j| j != i) {
                    let Some(talker) = a.slots[j].as_ref() else { continue };
                    let Some(data) = &talker.advert else { continue };
                    let d = distance(listener.pos, talker.pos);
                    if d <= a.config.range {
                        let rssi = (-40.0 - 20.0 * (d + 1.0).log10()) as i16;
                        let ev = Event::Advert { device: talker.address.clone(), service_data: data.clone(), rssi: Some(rssi) };
                        heard.push((listener.events.clone(), ev));
                    }
                }
            }
            let gone: Vec<_> = a.links.iter().filter(|(x, y)| x.0 < y.0 && !a.in_range(x.0, y.0)).map(|(x, _)| *x).collect();
            for end in gone {
                a.cut(end, &mut closed, true);
            }
        }
        for (tx, ev) in heard {
            let _ = tx.try_send(ev); // a busy scanner misses adverts, as in life
        }
        for (tx, ev) in closed {
            let _ = tx.send(ev).await;
        }
    }
}
