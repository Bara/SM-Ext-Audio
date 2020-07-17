use std::ffi::CString;
use std::os::raw::{c_int, c_void};

#[repr(transparent)]
pub(crate) struct VoiceCodec {
    ptr: *mut c_void,
}

unsafe impl Send for VoiceCodec {}

impl VoiceCodec {
    pub unsafe fn from_ptr(ptr: *mut c_void) -> VoiceCodec {
        VoiceCodec { ptr }
    }

    pub fn new() -> VoiceCodec {
        let interface_name = CString::new(crate::CODEC_INTERFACE_NAME).unwrap();
        let interface_name_ptr = interface_name.as_ptr();
        unsafe {
            VoiceCodec::from_ptr(
                cpp!([interface_name_ptr as "const char*"] -> *mut c_void as "void*" {
                    extern CreateInterfaceFn g_pCodecCreateInterface;
                    return g_pCodecCreateInterface(interface_name_ptr, nullptr);
                }),
            )
        }
    }

    pub fn init(&mut self, quality: i32) -> bool {
        let ptr = self.ptr;
        cpp!(unsafe [ptr as "IVoiceCodec*", quality as "int"] -> bool as "bool" {
            return ptr->Init(quality);
        })
    }

    pub fn compress(&mut self, uncomp: &[u8], samples: usize, comp: &mut [u8], fin: bool) -> usize {
        let ptr = self.ptr;

        let uncomp_ptr = uncomp.as_ptr();
        let comp_ptr = comp.as_mut_ptr();
        let comp_len = comp.len() as c_int;
        let samples = samples as c_int;

        cpp!(unsafe [ptr as "IVoiceCodec*", uncomp_ptr as "const char*", samples as "int", comp_ptr as "char*", comp_len as "int", fin as "bool"] -> usize as "size_t" {
            return (size_t)ptr->Compress(uncomp_ptr, samples, comp_ptr, comp_len, fin);
        })
    }

    pub fn decompress(&mut self, comp: &[u8], uncomp: &mut [u8]) -> usize {
        let ptr = self.ptr;

        let comp_ptr = comp.as_ptr();
        let comp_len = comp.len() as c_int;
        let uncomp_ptr = uncomp.as_mut_ptr();
        let uncomp_len = uncomp.len() as c_int;

        cpp!(unsafe [ptr as "IVoiceCodec*", comp_ptr as "const char*", comp_len as "int", uncomp_ptr as "char*", uncomp_len as "int"] -> usize as "size_t" {
            return (size_t)ptr->Decompress(comp_ptr, comp_len, uncomp_ptr, uncomp_len);
        })
    }

    /*
    pub fn reset_state(&mut self) -> bool {
        let ptr = self.ptr;
        cpp!(unsafe [ptr as "IVoiceCodec*"] -> bool as "bool" {
            return ptr->ResetState();
        })
    }
    */
}

impl Drop for VoiceCodec {
    fn drop(&mut self) {
        let ptr = self.ptr;
        cpp!(unsafe [ptr as "IVoiceCodec*"] {
            ptr->Release();
        })
    }
}
