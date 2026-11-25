//! mpdvu: reads the audio MPD writes to a `fifo` output and sends its levels
//! over UDP to whoever asks, so a VU meter can run on another machine
//! without the audio itself travelling.
//!
//! Protocol, one text datagram each way:
//!
//! - a client sends `mpdvu 1 subscribe` and keeps sending it every few
//!   seconds; it is forgotten when it stops;
//! - while it is subscribed it receives, many times a second,
//!   `mpdvu 1 <sequence> <peak> <rms> [<peak> <rms>...]`: a pair per channel,
//!   linear, from 0 to 1. Silence is sent as zeros, and nothing at all is
//!   sent when the levels have been zero for a while.
use std::collections::HashMap;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::net::{SocketAddr, UdpSocket};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A client is dropped when it has not asked again for this long.
const SUBSCRIPTION: Duration = Duration::from_secs(10);
const GREETING: &str = "mpdvu 1";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Sample {
    S16,
    /// 24 bits in the low three bytes of four, as MPD writes them.
    S24,
    S32,
    F32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct Format {
    rate: u32,
    sample: Sample,
    channels: usize,
}

impl Format {
    /// "44100:16:2", as in MPD's `format` setting.
    fn parse(text: &str) -> Option<Format> {
        let parts: Vec<&str> = text.split(':').collect();
        let [rate, bits, channels] = parts.as_slice() else { return None };
        let sample = match *bits {
            "16" => Sample::S16,
            "24" => Sample::S24,
            "32" => Sample::S32,
            "f" => Sample::F32,
            _ => return None,
        };
        let format = Format { rate: rate.parse().ok()?, sample, channels: channels.parse().ok()? };
        (format.rate > 0 && (1..=8).contains(&format.channels)).then_some(format)
    }

    fn bytes_per_sample(&self) -> usize {
        match self.sample {
            Sample::S16 => 2,
            _ => 4,
        }
    }

    /// One sample from its bytes, from -1 to 1.
    fn value(&self, bytes: &[u8]) -> f32 {
        match self.sample {
            Sample::S16 => i16::from_le_bytes([bytes[0], bytes[1]]) as f32 / 32768.0,
            Sample::S24 => {
                // Sign-extend the three bytes that carry the sample.
                let raw = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]);
                ((raw << 8) >> 8) as f32 / 8_388_608.0
            }
            Sample::S32 => i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f32 / 2_147_483_648.0,
            Sample::F32 => f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        }
    }
}

/// Peak and RMS of each channel in a block of interleaved samples.
fn levels(format: &Format, block: &[u8]) -> Vec<(f32, f32)> {
    let width = format.bytes_per_sample();
    let mut peak = vec![0f32; format.channels];
    let mut squares = vec![0f64; format.channels];
    let mut frames = 0usize;
    for frame in block.chunks_exact(width * format.channels) {
        for (channel, bytes) in frame.chunks_exact(width).enumerate() {
            let v = format.value(bytes);
            let v = if v.is_finite() { v.clamp(-1.0, 1.0) } else { 0.0 };
            peak[channel] = peak[channel].max(v.abs());
            squares[channel] += (v as f64) * (v as f64);
        }
        frames += 1;
    }
    (0..format.channels)
        .map(|c| (peak[c], if frames > 0 { (squares[c] / frames as f64).sqrt() as f32 } else { 0.0 }))
        .collect()
}

struct Options {
    fifo: String,
    format: Format,
    listen: String,
    /// Messages a second.
    rate: u32,
}

const USAGE: &str = "\
mpdvu: levels of what MPD is playing, over UDP, for a VU meter

Usage: mpdvu [options]

  --fifo PATH      the fifo MPD writes to (default /tmp/mpd.fifo)
  --format F       its format, as in mpd.conf: rate:bits:channels, with bits
                   16, 24, 32 or f (default 44100:16:2)
  --listen ADDR    where to wait for clients (default 0.0.0.0:6601)
  --rate N         messages a second, 5 to 60 (default 25)
  --version
";

fn options() -> Result<Options, String> {
    let mut o = Options {
        fifo: "/tmp/mpd.fifo".into(),
        format: Format { rate: 44100, sample: Sample::S16, channels: 2 },
        listen: "0.0.0.0:6601".into(),
        rate: 25,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--fifo" => o.fifo = value()?,
            "--format" => {
                let text = value()?;
                o.format = Format::parse(&text).ok_or(format!("not a format: {text}"))?;
            }
            "--listen" => o.listen = value()?,
            "--rate" => {
                let text = value()?;
                o.rate = text.parse().ok().filter(|n| (5..=60).contains(n)).ok_or(format!("not a rate from 5 to 60: {text}"))?;
            }
            "--version" | "-V" => {
                println!("mpdvu {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(o)
}

type Clients = Arc<Mutex<HashMap<SocketAddr, Instant>>>;

/// Remembers who asks, on its own thread.
fn listen(socket: UdpSocket, clients: Clients) {
    let mut buffer = [0u8; 64];
    loop {
        match socket.recv_from(&mut buffer) {
            Ok((n, from)) if buffer[..n].starts_with(GREETING.as_bytes()) => {
                clients.lock().unwrap().insert(from, Instant::now());
            }
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => {
                eprintln!("mpdvu: receiving: {e}");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

/// Sends one message to every client that is still subscribed.
fn send(socket: &UdpSocket, clients: &Clients, sequence: u64, levels: &[(f32, f32)]) {
    let mut clients = clients.lock().unwrap();
    clients.retain(|_, seen| seen.elapsed() < SUBSCRIPTION);
    if clients.is_empty() {
        return;
    }
    let mut message = format!("{GREETING} {sequence}");
    for (peak, rms) in levels {
        message += &format!(" {peak:.4} {rms:.4}");
    }
    for client in clients.keys() {
        let _ = socket.send_to(message.as_bytes(), client);
    }
}

fn main() -> ExitCode {
    let o = match options() {
        Ok(o) => o,
        Err(problem) => {
            eprintln!("mpdvu: {problem}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let socket = match UdpSocket::bind(&o.listen) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mpdvu: cannot listen on {}: {e}", o.listen);
            return ExitCode::FAILURE;
        }
    };
    let clients: Clients = Arc::default();
    {
        let (socket, clients) = (socket.try_clone().expect("socket"), clients.clone());
        std::thread::spawn(move || listen(socket, clients));
    }

    // As much audio as one message covers.
    let frame = o.format.bytes_per_sample() * o.format.channels;
    let mut block = vec![0u8; (o.format.rate / o.rate).max(1) as usize * frame];
    let silence = vec![(0.0, 0.0); o.format.channels];
    let mut sequence = 0u64;
    let mut complained = false;
    loop {
        // Opening waits until MPD has the fifo open, which it does while the
        // output is enabled and something plays.
        let mut fifo = match File::open(&o.fifo) {
            Ok(f) => f,
            Err(e) => {
                if !complained {
                    eprintln!("mpdvu: {}: {e}; waiting for it", o.fifo);
                    complained = true;
                }
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        complained = false;
        loop {
            match fifo.read_exact(&mut block) {
                Ok(()) => {
                    sequence += 1;
                    send(&socket, &clients, sequence, &levels(&o.format, &block));
                }
                // MPD closed it: paused, stopped or the output was disabled.
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    eprintln!("mpdvu: reading {}: {e}", o.fifo);
                    break;
                }
            }
        }
        // The needles fall instead of freezing where they were.
        sequence += 1;
        send(&socket, &clients, sequence, &silence);
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(Format::parse("44100:16:2"), Some(Format { rate: 44100, sample: Sample::S16, channels: 2 }));
        assert_eq!(Format::parse("96000:24:2").map(|f| f.sample), Some(Sample::S24));
        assert_eq!(Format::parse("48000:f:1").map(|f| f.channels), Some(1));
        assert_eq!(Format::parse("dsd64:2"), None);
        assert_eq!(Format::parse("44100:8:2"), None);
        assert_eq!(Format::parse("0:16:2"), None);
    }

    #[test]
    fn full_scale_square_wave() {
        let format = Format::parse("44100:16:2").unwrap();
        // Left: full scale, alternating sign. Right: silence.
        let mut block = vec![];
        for i in 0..100 {
            let left: i16 = if i % 2 == 0 { i16::MAX } else { i16::MIN };
            block.extend_from_slice(&left.to_le_bytes());
            block.extend_from_slice(&0i16.to_le_bytes());
        }
        let l = levels(&format, &block);
        assert!((l[0].0 - 1.0).abs() < 0.001 && (l[0].1 - 1.0).abs() < 0.001, "{l:?}");
        assert_eq!(l[1], (0.0, 0.0));
    }

    #[test]
    fn half_scale_in_24_bits() {
        let format = Format::parse("96000:24:1").unwrap();
        let mut block = vec![];
        for v in [4_194_304i32, -4_194_304] {
            block.extend_from_slice(&v.to_le_bytes());
        }
        let l = levels(&format, &block);
        assert!((l[0].0 - 0.5).abs() < 0.001 && (l[0].1 - 0.5).abs() < 0.001, "{l:?}");
    }
}
