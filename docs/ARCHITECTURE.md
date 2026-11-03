# Architecture

This crate takes the coded frames `rust-airplay` hands over and turns them into a picture
and a sound. It is the backend half of the pair; [docs/BACKEND.md](https://github.com/rust-airplay/rust-airplay/blob/main/docs/BACKEND.md)
in the core repository is the contract it answers.

## Two media, two shapes

The receiver stops at coded frames: an audio payload or a video access unit, encrypted on
the wire and decrypted before it arrives. Neither is decoded here, and neither is played by
the core.

**Video is a byte stream.** H.264 and HEVC carry start codes and parameter sets inside the
stream, so a decoder finds its own picture boundaries without being told where they are. The
frames are written to a program's standard input in the order they were pushed, and that is
the whole delivery.

**Audio is not.** A raw AAC-ELD frame and a raw ALAC frame have no sync word and no length
field of their own: the only thing that says where a frame ends is the buffer the session
handed over, and a byte stream loses exactly that information. The core's GStreamer backend
solves this by linking the library and using `appsrc`, one buffer per frame. This backend
cannot: it starts programs, and a program reads a file, not a series of buffers. So the
frames are wrapped in a container that carries both the boundaries and the codec
configuration the frames do not carry.

## An audio frame's path

1. `AudioFrame { data, timestamp }` arrives at `push`.
2. `data` becomes a Matroska `SimpleBlock`: a track number, a millisecond offset, the
   frame's length, and the frame. The stream's header, written once when the stream opens,
   declares the track, its codec (`A_AAC` or `A_ALAC`) and the codec configuration the
   decoder needs before it can read a frame. `docs/BACKEND.md` calls this out, and it is
   why the configuration lives in `src/codec.rs` rather than being read from the stream.
3. `timestamp` counts **samples**, not seconds: it is divided by the sample rate to get the
   millisecond the block declares. The value orders the blocks; it is not a schedule.
4. The bytes go to the standard input of `ffmpeg -f matroska -i pipe:0 -f s16le ...`.
5. The PCM that comes out of the decoder's standard output is scaled by the session's volume
   and written to the standard input of the sink.

Why Matroska rather than MP4: a live stream has to be written forwards. A Matroska segment
and cluster may declare an unknown length, so nothing has to be patched once the end is
known, and a reader can decode the first block without waiting for a complete file. The
header is a few hundred bytes; the block carries its own length, which is the thing a raw
frame stream could not.

## A video frame's path

1. `VideoFrame { data, timestamp, keyframe }` arrives at `push`.
2. If a pace was asked for through `set_pace`, the pacer holds the frame until its deadline
   on a cumulative timeline: each frame's deadline is the previous deadline plus the
   interval, so a sleep that returns late does not push every later frame back with it, and
   a stall longer than one interval re-anchors the grid instead of being repaid as a burst.
3. `data` -- an Annex-B access unit, with its parameter sets already in front -- is written
   to the sink's standard input, in order.
4. The player is capped in how much of the stream it may read before showing any of it
   (`-probesize 65536 -analyzeduration 0`). FFmpeg's default is five megabytes or five
   seconds, and a live mirror reaches it: measured through the same command line, the player
   took **3401 ms** to decide what it was reading before it began, which is a picture
   permanently three seconds behind however fast the rest of the path is. The same
   measurement at 64 KiB is **259 ms**.
5. The sink is told to stamp each unit with the moment it arrived
   (`-use_wallclock_as_timestamps 1`), because an elementary stream carries no timestamps and
   the program reading one invents them. On an invented grid -- FFmpeg's guess is 25 frames a
   second, a mirror sends 30 or 60 -- the picture falls behind by the difference, the player's
   queue grows, and once it is full the player drops packets rather than frames, taking the
   references the rest of the stream is built on with them. Stamping arrival is also what
   keeps a client that encodes a little ahead from buying that lead back as latency: the
   picture follows the device rather than a schedule.

The sink decodes: `ffplay` by default, or any program started with `-f h264` (or `-f hevc`)
and `-i pipe:0` in front of its own arguments. Timestamps are NTP fixed point in the
client's own epoch, which is not this machine's clock, so nothing here schedules against
them; the receiver paces the stream already, and the pipe's backpressure is what is left.

## The gain

`set_volume` is called before any client connects and again whenever a client moves its
slider, and the backend has to do two things: apply the gain to the stream that is playing,
and remember it for streams opened later.

That is why audio is decoded to PCM here rather than played by the decoder. A volume inside
the decoder's argument list is fixed when the process starts: changing it means replacing
the process, which drops the queue and makes an audible gap. A gain applied to the PCM on
its way past costs one multiply per sample and takes effect on the next chunk. The gain is
an `AtomicU32` holding the bits of an `f32`, shared by the renderer and by every audio
stream it opens, so a change reaches a stream that is playing without a lock held across a
read of PCM.

The value is linear amplitude -- `Volume::level` -- and it is applied to 16-bit samples with
rounding and clamping: a gain above one clips rather than wrapping a loud sample into a
quiet one.

## Processes

Every stage is a child process. The writer thread is what makes a push non-blocking in the
sense the contract asks for: it drains a queue four buffers deep, so a player that stops
reading becomes a refused frame within two seconds rather than a session that never returns.
Closing the queue closes the pipe, which is the end of stream a program needs to drain what
it has.

What a program says is drained by a thread of its own into the last few kilobytes in memory.
It is not written to a file: a backend should not have to choose a path, and the file would
be read exactly when the stream ends. A pipe nobody reads would fill and block the program,
which is why the thread exists at all.

Each program gets 150 milliseconds to say it was refused before the stream counts as
started, because what takes the time is the process starting rather than the stream opening.
Every wait after that has a deadline: five seconds for a program to drain and exit on close,
two seconds for a kill to be reaped.

## Flush

A flush means the media already handed over is stale. Neither a decoder nor a player can be
told to forget what it has queued, so both are replaced:

- Video: the sink process is replaced, and the pacer's timeline restarts. The client's next
  frame is a key frame, which is what makes a fresh decoder correct rather than merely empty.
- Audio: both processes are replaced, the Matroska header is written again, and the stream
  starts over at the client's next timestamp. The pump thread that carried PCM to the old
  sink is left to end on its own: it reads from a pipe that is about to close.

## Failure

`Unavailable` is the environment: a program that is not on this machine, a display that
cannot be opened, an input format the FFmpeg build does not have. `Failed` is the request:
a program that refused its arguments, or one that stopped while a session was using it.
The classification happens in `src/process/error.rs`, where the evidence is.

A program that stops mid-session does not take the session with it: the next push reports
it, the receiver logs it, and the other medium keeps playing. Neither a retry loop nor a
silent restart belongs inside a backend.

## The coding decides a 4K mirror's delay

Measured on one machine with an iPhone sending to this backend, a stopwatch in the mirror
against the same stopwatch on the device:

| Offer | Coding | Result |
| --- | --- | --- |
| 3840x2160@60, H.264 only | H.264 High, 3108x2160 | **1.48 s behind**, smooth but late |
| 3840x2160@60, HEVC offered | H.265, 4K | no delay measurable with the stopwatch |

Nothing in this backend changed between those two rows except which coding the receiver
advertised: every stage measured fast in both (decode 185 fps, display 178 fps, a push of
0.1 ms, no dropped frames, and the arrival report showed the windows keeping up with the
client's own timestamps). The delay was in the client's H.264 mirroring path at 4K, and
the fix was to offer HEVC -- `Config::features` with the HEVC bit set, which is how a receiver offers it.
A client that supports HEVC encodes the mirror in it, and the picture follows the
device.

The arrival report is still the first thing to read when a mirror is late: it carries the
client's stamp beside this machine's clock and says how far each window of frames fell
behind. What it separates is a client that cannot keep up (windows growing) from a client
that is keeping up and a pipeline that is not (windows equal, delay elsewhere).

## What is not here

- **Lip sync.** The two media arrive with timestamps from two different clocks. A backend
  that plays both has to build one time base from them, and this one does not: each stream
  is played by its own program, at the rate that program reads it.
- **A window of its own.** The sink owns the display. Nothing in this crate draws.
- **Seeking.** The sink gets a stream, not a file it can move around in; a flush is the only
  thing that changes where playback is.
- **A linked decoder.** No C library, no build-time FFmpeg, no `-sys` crate. The trade is
  the one at the top of this document: a container for audio, and a program boundary between
  the backend and the codecs.