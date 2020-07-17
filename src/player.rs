use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use std::os::raw::c_int;

use std::num::Wrapping;
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use futures::lock::Mutex;
use futures::prelude::*;

use std::thread::JoinHandle;

use crate::codec::VoiceCodec;

pub(crate) struct BufferBlockingRead<T: io::Read + Send + 'static> {
    reader: Arc<Mutex<T>>,
    buf: VecDeque<u8>,
    buf_fin: bool,
    rx: Option<oneshot::Receiver<io::Result<Vec<u8>>>>,
    thread: Option<JoinHandle<()>>,
}

impl<T: io::Read + Send + 'static> BufferBlockingRead<T> {
    pub(crate) fn new(reader: T) -> BufferBlockingRead<T> {
        BufferBlockingRead {
            reader: Arc::new(Mutex::new(reader)),
            buf: VecDeque::new(),
            buf_fin: false,
            rx: None,
            thread: None,
        }
    }
}

impl<T: io::Read + Send + 'static> AsyncRead for BufferBlockingRead<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if self.buf_fin && self.buf.len() <= 0 {
            return Poll::Ready(Ok(0));
        }

        let buf_size = 1024 * 64;

        if !self.buf_fin && self.buf.len() < buf_size {
            // Take the existing task or create it.
            let mut rx = match self.rx.take() {
                Some(rx) => rx,
                None => {
                    let reader = self.reader.clone();
                    let (tx, rx) = oneshot::channel();
                    self.thread.replace(std::thread::spawn(move || {
                        let mut buf = vec![0; buf_size];
                        let mut reader = async_std::task::block_on(reader.lock());
                        let _ = tx.send(reader.read(&mut buf).map(|size| {
                            buf.resize(size, 0);
                            buf
                        }));
                    }));
                    rx
                }
            };

            // Poll the receiver.
            let poll = Pin::new(&mut rx).poll(cx);
            match poll {
                Poll::Ready(res) => {
                    // Drop the thread handle.
                    drop(self.thread.take());

                    let res = res.unwrap();
                    if let Ok(buf) = res {
                        if buf.len() == 0 {
                            self.buf_fin = true;
                        }
                        for b in buf.iter() {
                            self.buf.push_back(*b);
                        }
                    } else {
                        self.buf_fin = true;
                    }
                }
                _ => {
                    // If reading is not finished, store the task.
                    if !self.buf_fin {
                        self.rx.replace(rx);
                    }
                }
            }
        }

        let size = std::cmp::min(self.buf.len(), buf.len());
        if !self.buf_fin && size == 0 {
            return Poll::Pending;
        }

        // Timer
        for m in 0..size {
            buf[m] = self.buf.pop_front().unwrap();
        }
        Poll::Ready(Ok(size))
    }
}

impl<T: io::Read + Send + 'static> Drop for BufferBlockingRead<T> {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

pub(crate) struct FFmpeg {
    cmd: Command,
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
            args: Vec::new(),
            ss: 0.0,
        }
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
        let args = vec![
            "-i",
            uri.as_ref(),
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

        self.cmd.args(&self.args);
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
        let self_ = self.clone();
        async_std::task::block_on(async move { self_.0.lock().await.read_size.0 })
    }

    pub(crate) fn dropped(&self) -> bool {
        Arc::strong_count(&self.0) <= 1
    }

    pub(crate) fn finished(&self) -> bool {
        let self_ = self.clone();
        async_std::task::block_on(async move { self_.0.lock().await.finished })
    }
}

struct PlayerInner {
    reader: BoxedAsyncRead,
    read_size: Wrapping<usize>,
    finished: bool,
}

pub(crate) struct Mixer {
    recvs: Arc<Mutex<Vec<Player>>>,

    shutdown_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,

    slot: c_int,

    pub encode: Arc<Mutex<VoiceCodec>>,
    pub decode: Arc<Mutex<VoiceCodec>>,
}

impl Mixer {
    pub(crate) fn new(slot: c_int) -> Mixer {
        let mut encode = VoiceCodec::new();
        encode.init(crate::CODEC_QUALITY);
        let mut decode = VoiceCodec::new();
        decode.init(crate::CODEC_QUALITY);

        Mixer {
            recvs: Arc::new(Mutex::new(Vec::new())),
            shutdown_tx: Arc::new(Mutex::new(None)),
            slot,
            encode: Arc::new(Mutex::new(encode)),
            decode: Arc::new(Mutex::new(decode)),
        }
    }

    /*
    pub(crate) async fn reset(&self) {
        self.recvs.lock().await.clear();
    }
    */

    pub(crate) async fn push(&self, out_rx: BoxedAsyncRead) -> Player {
        let player = Player::new(out_rx);
        self.recvs.lock().await.push(player.clone());
        player
    }

    pub(crate) async fn shutdown(&self) {
        drop(self.shutdown_tx.lock().await.take());
    }

    pub(crate) async fn run(&self) {
        use byteorder::LittleEndian;
        use byteorder::{ReadBytesExt, WriteBytesExt};

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        {
            self.shutdown_tx.lock().await.replace(shutdown_tx);
        }
        let mut shutdown_rx = shutdown_rx.fuse();

        let recvs = self.recvs.clone();

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
            if let Some(_) = diff.checked_sub(delay) {
                last_pull = now;

                let factor = diff.as_secs_f64() / delay.as_secs_f64();
                let mut poll_size = std::cmp::max(
                    crate::FRAMESIZE * 2,
                    (crate::FRAMESIZE as f64 * 2.0 * factor) as usize,
                );
                if poll_size % 2 == 1 {
                    poll_size += 1;
                }

                let mut mixed_size = 0;
                {
                    let mut recvs = recvs.lock().await;

                    let mut i = 0;
                    while i != recvs.len() {
                        let should_drop = {
                            let player_inner = recvs[i].0.lock().await;
                            recvs[i].dropped() || player_inner.finished
                        };
                        if should_drop {
                            drop(recvs.remove(i));
                        } else {
                            i += 1;
                        }
                    }

                    for player in recvs.iter() {
                        let mut player = player.0.lock().await;
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

                                mixed_size = std::cmp::max(mixed_size, size);
                                player.read_size += Wrapping(size);

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
                            }
                            default => continue,
                        };
                    }
                }

                if mixed_size != 0 {
                    let size = {
                        let mut encode = self.encode.lock().await;
                        encode.compress(&mixed[..mixed_size], mixed_size / 2, &mut comp, false)
                    };
                    if size != 0 {
                        crate::send_voicedata_as_slot(self.slot, &comp[..size]);
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
