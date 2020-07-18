#![recursion_limit = "4096"]

#[macro_use]
extern crate cpp;

use std::sync::{Arc, Mutex, RwLock};
use std::thread;

use std::ffi::{CStr, CString};
use std::os::raw::{c_int, c_uchar};

use std::path::PathBuf;

use async_std::task::JoinHandle;
use futures::channel::oneshot;

const CODEC_NAME: &str = "bin/vaudio_celt";
const CODEC_INTERFACE_NAME: &str = "vaudio_celt";
const CODEC_QUALITY: i32 = 3;

#[cfg(target_os = "windows")]
const CODEC_EXTENSION: &str = ".dll";
#[cfg(target_os = "linux")]
const CODEC_EXTENSION: &str = "_client.so";

#[cfg(target_os = "windows")]
const FFMPEG_FILENAME: &str = "ffmpeg.exe";
#[cfg(target_os = "linux")]
const FFMPEG_FILENAME: &str = "ffmpeg";

#[cfg(target_os = "windows")]
const YOUTUBE_DL_FILENAME: &str = "youtube-dl.exe";
#[cfg(target_os = "linux")]
const YOUTUBE_DL_FILENAME: &str = "youtube-dl";

const SAMPLERATE: usize = 22050;
const FRAMESIZE: usize = 512;

const MAXSLOT: usize = 64;

fn cstrncpy(dst: &mut [u8], src: &[u8]) -> usize {
    let dst_len = dst.len();
    if dst_len == 0 {
        return 0;
    }

    if src.len() >= dst_len {
        dst[..dst_len].copy_from_slice(&src[..dst_len]);
        dst[dst_len - 1] = 0;
        dst_len
    } else {
        dst[..src.len()].copy_from_slice(src);
        src.len()
    }
}

fn get_sm_path<'a>() -> &'a CStr {
    unsafe {
        CStr::from_ptr(cpp!(unsafe [] -> *const i8 as "const char *" {
           return smutils->GetSourceModPath();
        }))
    }
}

fn queue_game_frame(func: impl FnOnce() + 'static) {
    let ext = unsafe { EXT.as_ref().unwrap() };
    let mut queue_funcs = ext.queue_funcs.lock().unwrap();
    queue_funcs.push(Box::new(func));
}

fn send_voicedata_as_slot(slot: c_int, data: &[u8], hearself: bool) {
    let size = data.len();
    let data = data.as_ptr();
    cpp!(unsafe [slot as "int", data as "const char *", size as "size_t", hearself as "bool"] {
        IClient* iclient = iserver->GetClient(slot);
        if (iclient == nullptr) {
            return;
        }

        CCLCMsg_VoiceData msg;
        msg.set_data(data, size);
        if (hearself) {
            SV_BroadcastVoiceData_AllowHearSelf(iclient, &msg, false);
        } else {
            SV_BroadcastVoiceData(iclient, &msg, false);
        }
    });
}

cpp! {{
    #include "smsdk_ext.h"
    #include <CDetour/detours.h>
    #include <extensions/ISDKTools.h>

    #include <iserver.h>
    #include <iclient.h>
    #include <protobuf/netmessages.pb.h>

    class IVoiceCodec
    {
    protected:
        virtual			~IVoiceCodec() {}

    public:
        // Initialize the object. The uncompressed format is always 8-bit signed mono.
        virtual bool	Init(int quality) = 0;

        // Use this to delete the object.
        virtual void	Release() = 0;

        // Compress the voice data.
        // pUncompressed		-	16-bit signed mono voice data.
        // maxCompressedBytes	-	The length of the pCompressed buffer. Don't exceed this.
        // bFinal        		-	Set to true on the last call to Compress (the user stopped talking).
        //							Some codecs like big block sizes and will hang onto data you give them in Compress calls.
        //							When you call with bFinal, the codec will give you compressed data no matter what.
        // Return the number of bytes you filled into pCompressed.
        virtual int		Compress(const char *pUncompressed, int nSamples, char *pCompressed, int maxCompressedBytes, bool bFinal) = 0;

        // Decompress voice data. pUncompressed is 16-bit signed mono.
        virtual int		Decompress(const char *pCompressed, int compressedBytes, char *pUncompressed, int maxUncompressedBytes) = 0;

        // Some codecs maintain state between Compress and Decompress calls. This should clear that state.
        virtual bool	ResetState() = 0;
    };

    typedef void* (*CreateInterfaceFn)(const char *pName, int *pReturnCode);
}}

mod codec;
mod player;
mod reader;

use codec::VoiceCodec;
use player::{FFmpeg, Mixer, Player};
use reader::BufferBlockingRead;

pub(crate) struct Extension {
    mixers: Vec<(Mixer, Mutex<VoiceCodec>)>,
    spawned_tasks: Mutex<Vec<JoinHandle<()>>>,
    drop_tx: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
    queue_funcs: Mutex<Vec<Box<dyn FnOnce()>>>,
}

impl Extension {
    fn init() {
        let mut mixers = Vec::new();
        for slot in 0..MAXSLOT + 1 {
            let mut decode = VoiceCodec::new();
            decode.init(CODEC_QUALITY);
            mixers.push((Mixer::new(slot as c_int), Mutex::new(decode)));
        }

        unsafe {
            EXT.replace(Extension {
                mixers,
                spawned_tasks: Mutex::new(Vec::new()),
                drop_tx: None,
                thread: None,
                queue_funcs: Mutex::new(Vec::new()),
            });
        }
    }

    fn run() {
        let (drop_tx, drop_rx) = oneshot::channel();

        let thread = thread::spawn(move || {
            async_std::task::block_on(async move {
                let ext = unsafe { EXT.as_ref().unwrap() };
                {
                    let mut spawned_tasks = ext.spawned_tasks.lock().unwrap();
                    for entry in ext.mixers.iter() {
                        let mixer = entry.0.clone();
                        spawned_tasks.push(
                            async_std::task::Builder::new()
                                .name("Mixer".to_string())
                                .spawn(async move {
                                    mixer.run().await;
                                })
                                .unwrap(),
                        );
                    }
                }

                drop_rx.await.unwrap();

                {
                    for entry in ext.mixers.iter() {
                        entry.0.shutdown();
                    }

                    let mut spawned_tasks = ext.spawned_tasks.lock().unwrap();
                    while let Some(task) = spawned_tasks.pop() {
                        task.await;
                    }
                }
            })
        });

        let ext = unsafe { EXT.as_mut().unwrap() };
        ext.drop_tx.replace(drop_tx);
        ext.thread.replace(thread);
    }

    pub(crate) fn shutdown() {
        {
            let ext = unsafe { EXT.as_mut().unwrap() };
            if let Some(drop_tx) = ext.drop_tx.take() {
                drop_tx.send(()).unwrap()
            };

            if let Some(thread) = ext.thread.take() {
                thread.join().unwrap()
            };
        }
        unsafe {
            drop(EXT.take());
        }
    }

    pub(crate) fn report_error(msg: &str) {
        let msg = CString::new(msg).unwrap();
        let cmsg = msg.as_ptr();
        cpp!(unsafe [cmsg as "const char*"] {
            smutils->LogError(myself, "%s", cmsg);
        });
    }

    /*
    pub(crate) fn report_msg(msg: &str) {
        let msg = CString::new(msg).unwrap();
        let cmsg = msg.as_ptr();
        cpp!(unsafe [cmsg as "const char*"] {
            smutils->LogMessage(myself, "%s", cmsg);
        });
    }
    */
}

pub struct AudioPlayerHandle {
    pub(crate) ffmpeg: Option<FFmpeg>,
    pub(crate) player: Arc<RwLock<Option<Player>>>,
    task: Option<JoinHandle<()>>,
    slot: Option<usize>,
}

impl AudioPlayerHandle {
    fn new() -> AudioPlayerHandle {
        AudioPlayerHandle {
            ffmpeg: Some(FFmpeg::new()),
            player: Arc::new(RwLock::new(None)),
            task: None,
            slot: None,
        }
    }

    fn secs(&self) -> f32 {
        let player = self.player.read().unwrap();
        if let Some(player) = player.as_ref() {
            (player.read_size() as f32) / (SAMPLERATE as f32) / 2.0
        } else {
            0.0
        }
    }

    fn finished(&self) -> bool {
        let player = self.player.read().unwrap();
        if let Some(player) = player.as_ref() {
            player.finished()
        } else {
            false
        }
    }

    fn slot(&self) -> Option<usize> {
        self.slot
    }

    fn play(&mut self, slot: usize, uri: &str) {
        let ffmpeg = self.ffmpeg.take();
        let mut url = uri.to_owned();
        let player = self.player.clone();

        self.slot.replace(slot);
        self.task.replace(async_std::task::spawn(async move {
            use futures::AsyncReadExt;
            use std::collections::BTreeMap;
            use std::process::{Command, Stdio};

            let mut http_headers = BTreeMap::new();
            if url.starts_with("http://") || url.starts_with("https://") {
                let mut path = PathBuf::new();
                path.push(crate::get_sm_path().to_str().unwrap());
                path.push("data/audio_ext");
                path.push(crate::YOUTUBE_DL_FILENAME);

                let mut yt = Command::new(&path);
                yt.stdin(Stdio::null());
                yt.stdout(Stdio::piped());
                yt.stderr(Stdio::null());

                yt.args(&["--socket-timeout", "5", "-f", "bestaudio/worst", "-J", &url]);

                if let Ok(child) = yt.spawn() {
                    let mut json = Vec::new();
                    let mut reader = BufferBlockingRead::new(child.stdout.unwrap());
                    reader.read_to_end(&mut json).await.unwrap();
                    let value: serde_json::Value = match serde_json::from_slice(&json) {
                        Ok(value) => value,
                        Err(_) => return,
                    };

                    let is_playlist = value["_type"] == serde_json::json!("playlist");
                    if is_playlist {
                        return;
                    }
                    url = match value["url"].as_str() {
                        Some(url) => url.to_string(),
                        None => return,
                    };
                    if let Some(val_http_headers) = value["http_headers"].as_object() {
                        for (field, value) in val_http_headers.iter() {
                            http_headers
                                .insert(field.to_string(), value.as_str().map(|s| s.to_string()));
                        }
                    }
                }
            }

            if let Some(mut ffmpeg) = ffmpeg {
                let mut headers = String::new();
                for (field, val) in http_headers.iter() {
                    if let Some(val) = val {
                        headers.push_str(&format!("{}: {}\r\n", field, val));
                    } else {
                        headers.push_str(&format!("{}: \r\n", field));
                    }
                }

                if !headers.is_empty() {
                    ffmpeg.in_arg("-headers");
                    ffmpeg.in_arg(&headers);
                }

                match ffmpeg.start(url) {
                    Ok(child) => {
                        let ext = unsafe { EXT.as_ref().unwrap() };
                        let entry = ext.mixers.get(slot);
                        match entry {
                            Some(entry) => {
                                let mut player = player.write().unwrap();
                                player.replace(entry.0.push(Box::pin(BufferBlockingRead::new(
                                    child.stdout.unwrap(),
                                ))));
                            }
                            None => {
                                Extension::report_error(&format!("Out of slot: {}", slot));
                            }
                        }
                    }
                    Err(err) => {
                        Extension::report_error(&format!("Play error: {}", err));
                    }
                }
            }
        }));
    }
}

impl Drop for AudioPlayerHandle {
    fn drop(&mut self) {
        async_std::task::block_on(async {
            if let Some(task) = self.task.take() {
                task.await;
            }
        })
    }
}

static mut EXT: Option<Extension> = None;

cpp! {{
    ISDKTools *sdktools = nullptr;
    IServer *iserver = nullptr;

    SH_DECL_MANUALHOOK1(CGameClient__IsHearingClient, 0, 0, 0, bool, int);

    #if defined(WIN32)
    void (__cdecl *SV_BroadcastVoiceData_Actual)(bool) = nullptr;
    #else
    void (*SV_BroadcastVoiceData_Actual)(IClient *, const CCLCMsg_VoiceData *, bool) = nullptr;
    #endif

    static inline void SV_BroadcastVoiceData(IClient *iclient, const CCLCMsg_VoiceData *msg, bool drop)
    {
    #if defined(WIN32)
        __asm {
            mov ecx, iclient;
            mov edx, msg;
        }
        SV_BroadcastVoiceData_Actual(drop);
    #else
        SV_BroadcastVoiceData_Actual(iclient, msg, drop);
    #endif
    }

    bool Hook_IsHearingClient(int slot) {
        IClient *iclient = META_IFACEPTR(IClient);
        if (slot == iclient->GetPlayerSlot()) {
            RETURN_META_VALUE(MRES_SUPERCEDE, true);
        }
        RETURN_META_VALUE(MRES_IGNORED, false);
    }

    static void SV_BroadcastVoiceData_AllowHearSelf(IClient *iclient, const CCLCMsg_VoiceData *msg, bool drop)
    {
        SH_ADD_MANUALHOOK(CGameClient__IsHearingClient, iclient, SH_STATIC(Hook_IsHearingClient), false);
        SV_BroadcastVoiceData(iclient, msg, drop);
        SH_REMOVE_MANUALHOOK(CGameClient__IsHearingClient, iclient, SH_STATIC(Hook_IsHearingClient), false);
    }

    #if defined(WIN32)
    void __cdecl SV_BroadcastVoiceData_Callback(bool drop)
    #else
    void SV_BroadcastVoiceData_Callback(IClient *iclient, const CCLCMsg_VoiceData *msg, bool drop)
    #endif
    {
    #if defined(WIN32)
            IClient *iclient;
        const CCLCMsg_VoiceData *msg;
        __asm {
            mov iclient, ecx;
            mov msg, edx;
        }
    #endif
        size_t slot = (size_t)iclient->GetPlayerSlot();
        const char *data = msg->data().data();
        size_t data_size = msg->data().size();

        rust!(SV_BroadcastVoiceData_Callback__Internal [slot : usize as "size_t", data : *const c_uchar as "const char*", data_size : usize as "size_t"] {
            let comp = unsafe { std::slice::from_raw_parts(data, data_size) };
            let ext = unsafe { EXT.as_ref().unwrap() };
            if slot >= ext.mixers.len() {
                return;
            }

            async_std::task::block_on(async move {
                let mut decomp = vec![0; 22050];
                let entry = ext.mixers.get(slot).unwrap();
                let size = {
                    let mut decode = entry.1.lock().unwrap();
                    decode.decompress(comp, &mut decomp) * 2
                };
                entry.0.write(&decomp[..size]);
            });
        });
        //SV_BroadcastVoiceData(iclient, msg, drop);
    }

    void *g_pCodecLib;
    CreateInterfaceFn g_pCodecCreateInterface;

    class AudioPlayerTypeHandler : public IHandleTypeDispatch
    {
    public:
        void OnHandleDestroy(HandleType_t type, void *object)
        {
            rust!(AudioPlayerTypeHandler__OnHandleDestory [object : *mut AudioPlayerHandle as "void*"] {
                drop(Box::from_raw(object));
            });
        }
    };

    HandleType_t g_AudioPlayerType = 0;
    AudioPlayerTypeHandler g_AudioPlayerTypeHandler;

    IGameConfig *g_pGameConf = nullptr;
    CDetour *g_pDetourSVBroadcastVoiceData = nullptr;
    int g_iIsHearingClientOffset = 0;

    extern sp_nativeinfo_t g_Natives[];

    static void OnGameFrame(bool simulating) {
        rust!(OnGameFrame__Rust [] {
            let ext = unsafe { EXT.as_ref().unwrap() };
            let mut queue_funcs = ext.queue_funcs.lock().unwrap();
            while let Some(func) = queue_funcs.pop() {
                func();
            }
        });
    }

    class Ext : public SDKExtension
    {
    public:
        virtual bool SDK_OnLoad(char *error, size_t maxlen, bool late) {
            auto codecStore = &g_pCodecLib;
            auto interfaceStore = &g_pCodecCreateInterface;
            bool res = rust!(Ext__SDK_OnLoad__LoadCodecLib [error : *mut c_uchar as "char*", maxlen : usize as "size_t", codecStore: *mut *mut libloading::Library as "void*", interfaceStore : *mut isize as "CreateInterfaceFn*"] -> bool as "bool" {
                let error = std::slice::from_raw_parts_mut(error, maxlen);

                // dlopen vcodec library
                let mut vcodec_path = std::env::current_dir().unwrap();
                let mut vcodec_relpath = CODEC_NAME.to_owned();
                vcodec_relpath.push_str(CODEC_EXTENSION);
                vcodec_path.push(vcodec_relpath);

                let lib = libloading::Library::new(vcodec_path);
                let lib = match lib {
                    Ok(lib) => Box::new(lib),
                    Err(err) => {
                        let err = CString::new(format!("cannot load vaudio library: {}", err)).unwrap();
                        cstrncpy(error, err.as_bytes_with_nul());
                        return false;
                    }
                };

                // get vcodec's CreateInterface
                unsafe {
                    let func: libloading::Symbol<unsafe extern "C" fn(*const c_uchar, *mut c_int)> = match lib.get(b"CreateInterface") {
                        Ok(func) => func,
                        Err(err) => {
                            let err = CString::new(format!("CreateInterface function not found from codec: {}", err)).unwrap();
                            cstrncpy(error, err.as_bytes_with_nul());
                            return false;
                        }
                    };

                    *interfaceStore = func.into_raw().into_raw() as _;
                    *codecStore = Box::into_raw(lib);
                }

                // initialize extension
                Extension::init();
                Extension::run();

                true
            });

            if (!res) {
                return false;
            }

            sharesys->AddDependency(myself, "sdktools.ext", true, true);
            char conf_err[256];
            if (!gameconfs->LoadGameConfigFile("audio.ext.games", &g_pGameConf, conf_err, sizeof(conf_err))) {
                smutils->Format(error, maxlen, "Cannot open audio.ext.games gamedata: %s", conf_err);
                SDK_OnUnload();
                return false;
            }

            if (!g_pGameConf->GetOffset("CGameClient::IsHearingClient", &g_iIsHearingClientOffset)) {
                smutils->Format(error, maxlen, "Offset of CGameClient::IsHearingClient not found");
                SDK_OnUnload();
                return false;
            }
            SH_MANUALHOOK_RECONFIGURE(CGameClient__IsHearingClient, g_iIsHearingClientOffset, 0, 0);

            CDetourManager::Init(smutils->GetScriptingEngine(), g_pGameConf);
            g_pDetourSVBroadcastVoiceData = CDetourManager::CreateDetour((void*)&SV_BroadcastVoiceData_Callback, (void**)&SV_BroadcastVoiceData_Actual, "SV_BroadcastVoiceData");
            if (g_pDetourSVBroadcastVoiceData == nullptr) {
                smutils->Format(error, maxlen, "Could not create detour for SV_BroadcastVoiceData");
                SDK_OnUnload();
                return false;
            }
            g_pDetourSVBroadcastVoiceData->EnableDetour();

            g_AudioPlayerType = handlesys->CreateType("AudioPlayer", &g_AudioPlayerTypeHandler, 0, NULL, NULL, myself->GetIdentity(), NULL);
            sharesys->AddNatives(myself, g_Natives);
            sharesys->RegisterLibrary(myself, "Audio");

            smutils->AddGameFrameHook(OnGameFrame);

            return true;
        }

        virtual void SDK_OnUnload() {
            smutils->RemoveGameFrameHook(OnGameFrame);

            if (g_pDetourSVBroadcastVoiceData) {
                g_pDetourSVBroadcastVoiceData->Destroy();
                g_pDetourSVBroadcastVoiceData = nullptr;
            }

            if (g_pGameConf != nullptr) {
                gameconfs->CloseGameConfigFile(g_pGameConf);
                g_pGameConf = nullptr;
            }

            // deallocate handles
            if (g_AudioPlayerType) {
                handlesys->RemoveType(g_AudioPlayerType, myself->GetIdentity());
                g_AudioPlayerType = 0;
            }

            rust!(Ext__SDK_OnUnload [g_pCodecLib : *mut libloading::Library as "void*"] {
                unsafe {
                    Extension::shutdown();
                    drop(Box::from_raw(g_pCodecLib));
                }
            });

            g_pCodecCreateInterface = nullptr;
            g_pCodecLib = nullptr;
        }

        void SDK_OnAllLoaded() {
            SM_GET_LATE_IFACE(SDKTOOLS, sdktools);
            if (sdktools == nullptr) {
                smutils->LogError(myself, "Cannot get sdktools instance.");
                return;
            }

            iserver = sdktools->GetIServer();
        }
    };

    Ext g_Ext;
    SMEXT_LINK(&g_Ext);

    static cell_t Native_AudioMixer_GetClientCanHearSelf(IPluginContext *pContext, const cell_t *params)
    {
        int client = params[1];

        bool hearself = rust!(Native_AudioMixer_GetClientCanHearSelf__Rust [client : c_int as "int"] -> bool as "bool" {
            let ext = unsafe { EXT.as_ref().unwrap() };
            if let Some(mixer) = ext.mixers.get(client as usize-1) {
                mixer.0.hearself()
            } else {
                false
            }
        });
        return hearself ? 1 : 0;
    }

    static cell_t Native_AudioMixer_SetClientCanHearSelf(IPluginContext *pContext, const cell_t *params)
    {
        int client = params[1];
        bool canhear = params[2] ? true : false;

        rust!(Native_AudioMixer_SetClientCanHearSelf__Rust [client : c_int as "int", canhear : bool as "bool"] {
            let ext = unsafe { EXT.as_ref().unwrap() };
            if let Some(mixer) = ext.mixers.get(client as usize-1) {
                mixer.0.set_hearself(canhear)
            }
        });
        return 0;
    }

    static cell_t Native_CreateAudioPlayer(IPluginContext *pContext, const cell_t *params)
    {
        void *player = rust!(Native_CreateAudioPlayer__Rust [] -> *mut AudioPlayerHandle as "void*" {
            let player = Box::new(AudioPlayerHandle::new());
            Box::into_raw(player)
        });

        HandleError he = HandleError_None;
        Handle_t handle = handlesys->CreateHandle(g_AudioPlayerType, player, pContext->GetIdentity(), myself->GetIdentity(), &he);
        if (he != HandleError_None) {
            rust!(Native_CreateAudioPlayer__ErrorFree [player : *mut AudioPlayerHandle as "void*"] {
                drop(Box::from_raw(player));
            });
            return pContext->ThrowNativeError("Failed to create AudioPlayer handle: error #%d", he);
        }
        return handle;
    }

    static cell_t Native_AudioPlayer_GetPlayedSecs(IPluginContext *pContext, const cell_t *params)
    {
        Handle_t hndl = static_cast<Handle_t>(params[1]);
        HandleError err;
        HandleSecurity sec = HandleSecurity(NULL, myself->GetIdentity());

        void *player;
        if ((err = handlesys->ReadHandle(hndl, g_AudioPlayerType, &sec, &player)) != HandleError_None) {
            return pContext->ThrowNativeError("Invalid AudioPlayer handle %x (error %d)", hndl, err);
        }

        float secs = rust!(Native_AudioPlayer_GetPlayedSecs__Rust [player : *mut AudioPlayerHandle as "void*"] -> f32 as "float" {
            let player = Box::from_raw(player);
            let secs = player.secs();
            std::mem::forget(player);
            secs
        });
        return sp_ftoc(secs);
    }

    static cell_t Native_AudioPlayer_GetFinished(IPluginContext *pContext, const cell_t *params)
    {
        Handle_t hndl = static_cast<Handle_t>(params[1]);
        HandleError err;
        HandleSecurity sec = HandleSecurity(NULL, myself->GetIdentity());

        void *player;
        if ((err = handlesys->ReadHandle(hndl, g_AudioPlayerType, &sec, &player)) != HandleError_None) {
            return pContext->ThrowNativeError("Invalid AudioPlayer handle %x (error %d)", hndl, err);
        }

        bool finished = rust!(Native_AudioPlayer_GetFinished__Rust [player : *mut AudioPlayerHandle as "void*"] -> bool as "bool" {
            let player = Box::from_raw(player);
            let finished = player.finished();
            std::mem::forget(player);
            finished
        });
        return finished ? 1 : 0;
    }

    static cell_t Native_AudioPlayer_GetClientIndex(IPluginContext *pContext, const cell_t *params)
    {
        Handle_t hndl = static_cast<Handle_t>(params[1]);
        HandleError err;
        HandleSecurity sec = HandleSecurity(NULL, myself->GetIdentity());

        void *player;
        if ((err = handlesys->ReadHandle(hndl, g_AudioPlayerType, &sec, &player)) != HandleError_None) {
            return pContext->ThrowNativeError("Invalid AudioPlayer handle %x (error %d)", hndl, err);
        }

        int client = rust!(Native_AudioPlayer_GetClientIndex__Rust [player : *mut AudioPlayerHandle as "void*"] -> c_int as "int" {
            let player = Box::from_raw(player);
            let client = match player.slot() {
                Some(slot) => slot + 1,
                None => 0,
            };
            let client = client as c_int;

            std::mem::forget(player);
            client
        });
        return client;
    }

    static cell_t Native_AudioPlayer_SetFrom(IPluginContext *pContext, const cell_t *params)
    {
        Handle_t hndl = static_cast<Handle_t>(params[1]);
        HandleError err;
        HandleSecurity sec = HandleSecurity(NULL, myself->GetIdentity());

        void *player;
        if ((err = handlesys->ReadHandle(hndl, g_AudioPlayerType, &sec, &player)) != HandleError_None) {
            return pContext->ThrowNativeError("Invalid AudioPlayer handle %x (error %d)", hndl, err);
        }

        float ss = sp_ctof(params[2]);

        rust!(Native_AudioPlayer_SetFrom__Rust [player : *mut AudioPlayerHandle as "void*", ss: f32 as "float"] {
            let mut player = Box::from_raw(player);

            if let Some(ffmpeg) = player.ffmpeg.as_mut() {
                ffmpeg.ss(ss as f64);
            }
            std::mem::forget(player);
        });

        return 0;
    }

    static cell_t Native_AudioPlayer_AddInputArg(IPluginContext *pContext, const cell_t *params)
    {
        Handle_t hndl = static_cast<Handle_t>(params[1]);
        HandleError err;
        HandleSecurity sec = HandleSecurity(NULL, myself->GetIdentity());

        void *player;
        if ((err = handlesys->ReadHandle(hndl, g_AudioPlayerType, &sec, &player)) != HandleError_None) {
            return pContext->ThrowNativeError("Invalid AudioPlayer handle %x (error %d)", hndl, err);
        }

        char *arg;
        pContext->LocalToString(params[2], &arg);

        rust!(Native_AudioPlayer_AddInputArg__Rust [player : *mut AudioPlayerHandle as "void*", arg: *const i8 as "char*"] {
            let arg = CStr::from_ptr(arg).to_str().unwrap();
            let mut player = Box::from_raw(player);

            if let Some(ffmpeg) = player.ffmpeg.as_mut() {
                ffmpeg.in_arg(arg);
            }
            std::mem::forget(player);
        });

        return 0;
    }

    static cell_t Native_AudioPlayer_AddArg(IPluginContext *pContext, const cell_t *params)
    {
        Handle_t hndl = static_cast<Handle_t>(params[1]);
        HandleError err;
        HandleSecurity sec = HandleSecurity(NULL, myself->GetIdentity());

        void *player;
        if ((err = handlesys->ReadHandle(hndl, g_AudioPlayerType, &sec, &player)) != HandleError_None) {
            return pContext->ThrowNativeError("Invalid AudioPlayer handle %x (error %d)", hndl, err);
        }

        char *arg;
        pContext->LocalToString(params[2], &arg);

        rust!(Native_AudioPlayer_AddArg__Rust [player : *mut AudioPlayerHandle as "void*", arg: *const i8 as "char*"] {
            let arg = CStr::from_ptr(arg).to_str().unwrap();
            let mut player = Box::from_raw(player);

            if let Some(ffmpeg) = player.ffmpeg.as_mut() {
                ffmpeg.arg(arg);
            }
            std::mem::forget(player);
        });

        return 0;
    }

    static cell_t Native_AudioPlayer_PlayAsClient(IPluginContext *pContext, const cell_t *params)
    {
        Handle_t hndl = static_cast<Handle_t>(params[1]);
        HandleError err;
        HandleSecurity sec = HandleSecurity(NULL, myself->GetIdentity());

        void *player;
        if ((err = handlesys->ReadHandle(hndl, g_AudioPlayerType, &sec, &player)) != HandleError_None) {
            return pContext->ThrowNativeError("Invalid AudioPlayer handle %x (error %d)", hndl, err);
        }

        int slot = params[2] - 1;

        char *uri;
        pContext->LocalToString(params[3], &uri);

        rust!(Native_AudioPlayer_PlayAsClient__Rust [player : *mut AudioPlayerHandle as "void*", uri : *const i8 as "char*", slot : c_int as "int"] {
            let ext = unsafe { EXT.as_ref().unwrap() };
            if slot < 0 || slot >= ext.mixers.len() as i32 {
                Extension::report_error(&format!("Slot out of range: {}", slot));
                return;
            }

            let slot = slot as usize;
            let uri = CStr::from_ptr(uri).to_str().unwrap();
            let mut player = Box::from_raw(player);

            player.play(slot, uri);
            std::mem::forget(player);
        });
        return 0;
    }

    sp_nativeinfo_t g_Natives[] = {
        { "AudioMixer_GetClientCanHearSelf", Native_AudioMixer_GetClientCanHearSelf },
        { "AudioMixer_SetClientCanHearSelf", Native_AudioMixer_SetClientCanHearSelf },
        { "AudioPlayer.AudioPlayer", Native_CreateAudioPlayer },
        { "AudioPlayer.PlayedSecs.get", Native_AudioPlayer_GetPlayedSecs },
        { "AudioPlayer.IsFinished.get", Native_AudioPlayer_GetFinished },
        { "AudioPlayer.ClientIndex.get", Native_AudioPlayer_GetClientIndex },
        { "AudioPlayer.SetFrom", Native_AudioPlayer_SetFrom },
        { "AudioPlayer.AddInputArg", Native_AudioPlayer_AddInputArg },
        { "AudioPlayer.AddArg", Native_AudioPlayer_AddArg },
        { "AudioPlayer.PlayAsClient", Native_AudioPlayer_PlayAsClient },
        { nullptr, nullptr },
    };
}}

pub(crate) mod sm;
