# mpdvu

Levels of what [MPD](https://www.musicpd.org) is playing, sent over the
network, so a VU meter can run on another machine.

MPD's audio for visualizers comes out of a `fifo` output: a pipe on the
server's own disk, which only a program on that machine can read. mpdvu is
that program. It reads the pipe, works out the peak and the RMS level of each
channel some 25 times a second, and sends just those numbers over UDP to
whoever asks; and, for those who want a spectrum instead of a VU meter, how
loud each band of frequencies is. The audio itself never leaves the server.

Made for the MPD card of [hasklets](https://github.com/prietus/hasklets).



https://github.com/user-attachments/assets/b80cb952-6afe-42d2-a0f4-decdcc947ec3



## Setting it up

On the machine MPD runs on, add an output to `mpd.conf` and restart MPD:

```
audio_output {
    type    "fifo"
    name    "VU meter"
    path    "/tmp/mpd.fifo"
    format  "44100:16:2"
}
```

The `format` fixes what is written to the pipe whatever is being played, so
mpdvu always knows how to read it. MPD converts to it, DSD included, which
costs the server some CPU; what goes to the other outputs does not change.

Then build and run mpdvu there:

```sh
cargo build --release
sudo install -m755 target/release/mpdvu /usr/local/bin/
sudo install -m644 contrib/mpdvu.service /etc/systemd/system/
sudo systemctl enable --now mpdvu
```

It has no dependencies besides Rust's standard library.

Where MPD runs as a user service (`systemctl --user`), use the user unit
instead:

```sh
install -Dm644 contrib/mpdvu.user.service ~/.config/systemd/user/mpdvu.service
systemctl --user enable --now mpdvu
```

To build on another machine for an x86_64 server, as a static binary:

```sh
rustup target add x86_64-unknown-linux-musl
RUSTFLAGS="-C linker=rust-lld" cargo build --release --target x86_64-unknown-linux-musl
```

## Options

| Option | Default | |
|---|---|---|
| `--fifo PATH` | `/tmp/mpd.fifo` | the pipe MPD writes to |
| `--format F` | `44100:16:2` | its format, as in `mpd.conf`: rate, bits (`16`, `24`, `32` or `f`) and channels |
| `--listen ADDR` | `0.0.0.0:6601` | where to wait for clients |
| `--rate N` | `25` | messages a second, 5 to 60 |

While MPD is paused or stopped it closes the pipe; mpdvu sends one message of
silence and waits for it to open again.

## Protocol

One text datagram each way, over UDP.

A client sends `mpdvu 1 subscribe` and keeps sending it every few seconds; a
client that has not asked for 10 seconds is forgotten. While subscribed it
receives

```
mpdvu 1 <sequence> <peak> <rms> [<peak> <rms> ...]
```

with a pair per channel, linear, from 0 to 1.

A client that asks with `mpdvu 1 subscribe bands` also receives the spectrum
(since 0.2.0):

```
mpdvu-bands 1 <sequence> <level> <level> ...
```

28 bands, low notes first, as wide as each other to the ear, from 45 Hz to
16 kHz; each from 0 to 1 on a scale of decibels (-54 dB to -6 dB), ready to
be drawn as bars. The spectrum is only worked out while somebody asks for it.

To try it by hand:

```sh
echo 'mpdvu 1 subscribe' | nc -u server 6601
```

Anyone who can reach the port can read the levels (and nothing else): there
is no password. Keep it to your own network.

## License

MIT
