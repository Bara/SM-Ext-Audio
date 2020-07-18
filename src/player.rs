use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use std::os::raw::c_int;

use std::num::Wrapping;
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use futures::prelude::*;

use crate::codec::VoiceCodec;
use crate::reader::BytesPipe;

pub(crate) struct FFmpeg {
    cmd: Command,
    in_args: Vec<String>,
    args: Vec<String>,
    ss: f64,
}

impl FFmpeg {
    pub(crate) fn new() -> FFmpeg {
        let mut path = PathBuf::new();
        path.push(crate::get_sm_path().to_str().unwrap());
        path.push("data/audio_ext");
        path.push(crate::FFMPEG_FILENAME);

        let mut cmd = Command::new(&path);
        cmd.stdin(Stdio::null());
        cmd.stderr(Stdio::null());
        cmd.stdout(Stdio::piped());

        FFmpeg {
            cmd,
            in_args: Vec::new(),
            args: Vec::new(),
            ss: 0.0,
        }
    }

    pub(crate) fn in_arg<S: AsRef<str>>(&mut self, arg: S) -> &Self {
        self.in_args.push(arg.as_ref().to_owned());
        self
    }

    pub(crate) fn arg<S: AsRef<str>>(&mut self, arg: S) -> &Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub(crate) fn ss(&mut self, sec: f64) -> &Self {
        self.ss = sec;
        self
    }

    pub(crate) fn start<S: AsRef<str>>(mut self, uri: S) -> io::Result<Child> {
        let ss = self.ss.to_string();
        let samplerate = crate::SAMPLERATE.to_string();

        self.cmd.args(&self.in_args);
        let args = vec!["-i", uri.as_ref()];
        self.cmd.args(args);

        self.cmd.args(&self.args);
        let args = vec![
            "-ss",
            &ss,
            "-acodec",
            "pcm_s16le",
            "-ac",
            "1",
            "-ar",
            &samplerate,
            "-f",
            "s16le",
            "-",
        ];
        self.cmd.args(args);

        self.cmd.spawn()
    }
}

pub(crate) type BoxedAsyncRead = Pin<Box<dyn AsyncRead + Unpin + Send>>;

#[derive(Clone)]
pub(crate) struct Player(Arc<Mutex<PlayerInner>>);

impl Player {
    pub(crate) fn new(reader: BoxedAsyncRead) -> Player {
        Player(Arc::new(Mutex::new(PlayerInner {
            reader,
            read_size: Wrapping(0),
            finished: false,
        })))
    }

    pub(crate) fn read_size(&self) -> usize {
        self.0.lock().unwrap().read_size.0
    }

    pub(crate) fn dropped(&self) -> bool {
        Arc::strong_count(&self.0) <= 1
    }

    pub(crate) fn finished(&self) -> bool {
        self.0.lock().unwrap().finished
    }
}

struct PlayerInner {
    reader: BoxedAsyncRead,
    read_size: Wrapping<usize>,
    finished: bool,
}

#[derive(Clone)]
pub(crate) struct Mixer(Arc<Mutex<MixerInner>>);

struct MixerInner {
    players: Vec<Player>,
    pipe: BytesPipe,

    shutdown_tx: Option<oneshot::Sender<()>>,

    slot: c_int,
    hearself: bool,

    encode: VoiceCodec,
}

impl Mixer {
    pub(crate) fn new(slot: c_int) -> Mixer {
        let mut encode = VoiceCodec::new();
        encode.init(crate::CODEC_QUALITY);

        Mixer(Arc::new(Mutex::new(MixerInner {
            players: Vec::new(),
            pipe: BytesPipe::new(),
            shutdown_tx: None,
            slot,
            encode,
            hearself: false,
        })))
    }

    pub(crate) fn write(&self, data: &[u8]) {
        let self_ = self.0.clone();
        async_std::task::block_on(async move {
            let _ = self_.lock().unwrap().pipe.write(data).await;
        });
    }

    pub(crate) fn push(&self, out_rx: BoxedAsyncRead) -> Player {
        let player = Player::new(out_rx);
        let mut self_ = self.0.lock().unwrap();
        self_.players.push(player.clone());
        player
    }

    pub(crate) fn shutdown(&self) {
        let mut self_ = self.0.lock().unwrap();
        self_.shutdown_tx.take().unwrap().send(()).unwrap();
    }

    pub(crate) fn hearself(&self) -> bool {
        let self_ = self.0.lock().unwrap();
        self_.hearself
    }

    pub(crate) fn set_hearself(&self, hearself: bool) {
        let mut self_ = self.0.lock().unwrap();
        self_.hearself = hearself;
    }

    pub(crate) async fn run(&self) {
        use byteorder::LittleEndian;
        use byteorder::{ReadBytesExt, WriteBytesExt};

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        {
            let mut self_ = self.0.lock().unwrap();
            self_.shutdown_tx.replace(shutdown_tx);
        }
        let mut shutdown_rx = shutdown_rx.fuse();

        let mut mixed = vec![0; crate::FRAMESIZE * 2 * 2];
        let mut buf = vec![0; crate::FRAMESIZE * 2 * 2];
        let mut comp = vec![0; crate::FRAMESIZE * 2 * 2];

        let delay =
            Duration::from_secs_f64(1.0 / crate::SAMPLERATE as f64 * crate::FRAMESIZE as f64);
        let mut last_pull = Instant::now();
        loop {
            for m in &mut mixed {
                *m = 0;
            }

            let now = Instant::now();
            let diff = now.duration_since(last_pull);
            if diff.checked_sub(delay).is_some() {
                last_pull = now;

                let factor = diff.as_secs_f64() / delay.as_secs_f64();
                let mut poll_size = std::cmp::min(
                    std::cmp::max(
                        crate::FRAMESIZE * 2,
                        (crate::FRAMESIZE as f64 * 2.0 * factor) as usize,
                    ),
                    crate::FRAMESIZE * 2 * 2,
                );
                if poll_size % 2 == 1 {
                    poll_size += 1;
                }

                let mut mixed_size = 0;

                let mut mix = |size, buf: &[u8], mixed: &mut [u8]| {
                    mixed_size = std::cmp::max(mixed_size, size);
                    let mut m = 0;
                    while m < mixed.len() && m < size {
                        let bi = (&buf[m..m + 2]).read_i16::<LittleEndian>().unwrap() as i32;
                        let mi = (&mixed[m..m + 2]).read_i16::<LittleEndian>().unwrap() as i32;

                        let mut sum = bi + mi;
                        if sum < i16::MIN as i32 {
                            sum = i16::MIN as i32;
                        } else if sum > i16::MAX as i32 {
                            sum = i16::MAX as i32;
                        }
                        (&mut mixed[m..m + 2])
                            .write_i16::<LittleEndian>(sum as i16)
                            .unwrap();
                        m += 2;
                    }
                };

                let mut self_ = self.0.lock().unwrap();
                let mut i = 0;
                while i != self_.players.len() {
                    let should_drop = self_.players[i].dropped() || self_.players[i].finished();
                    if should_drop {
                        drop(self_.players.remove(i));
                    } else {
                        i += 1;
                    }
                }

                futures::select! {
                    res = self_.pipe.read(&mut buf[..poll_size]).fuse() => {
                        let size = res.unwrap();
                        mix(size, &buf, &mut mixed);
                    }
                    default => {},
                };

                for player in self_.players.iter() {
                    let mut player = player.0.lock().unwrap();
                    futures::select! {
                        res = player.reader.read(&mut buf[..poll_size]).fuse() => {
                            let size = match res {
                                Ok(size) => size,
                                Err(_) => {
                                    player.finished = true;
                                    continue;
                                }
                            };
                            if size == 0 || size % 2 == 1 {
                                player.finished = true;
                                continue;
                            }

                            player.read_size += Wrapping(size);

                            mix(size, &buf, &mut mixed);
                        }
                        default => continue,
                    };
                }

                if mixed_size != 0 {
                    let size = self_.encode.compress(
                        &mixed[..mixed_size],
                        mixed_size / 2,
                        &mut comp,
                        false,
                    );
                    if size != 0 {
                        let slot = self_.slot;
                        let hearself = self_.hearself;
                        let comp = (&comp[..size]).to_vec();
                        crate::queue_game_frame(move || {
                            crate::send_voicedata_as_slot(slot, &comp, hearself);
                        })
                    }
                }
            }

            let sleep = async_std::task::sleep(Duration::from_millis(1));
            futures::select! {
                _ = sleep.fuse() => {}
                _ = shutdown_rx => {
                    break;
                }
            }
        }
    }
}
