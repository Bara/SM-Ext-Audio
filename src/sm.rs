mod sm_interface {
    use std::ffi::c_void;

    #[no_mangle]
    pub unsafe extern "C" fn GetSMExtAPI() -> *mut c_void {
        cpp!([] -> *mut c_void as "IExtensionInterface*" {
            return g_pExtensionIface;
        })
    }
}

#[cfg(feature = "metamod")]
mod metamod {
    use std::ffi::c_void;
    use std::os::raw::{c_int, c_uchar};

    #[no_mangle]
    pub unsafe extern "C" fn CreateInterface(
        name: *const c_uchar,
        code: *mut c_int,
    ) -> *mut c_void {
        cpp!([name as "const char*", code as "int*"] -> *mut c_void as "void *" {
            #if defined SMEXT_CONF_METAMOD
                #if defined METAMOD_PLAPI_VERSION
                    if (name && !strcmp(name, METAMOD_PLAPI_NAME))
                #else
                    if (name && !strcmp(name, PLAPI_NAME))
                #endif
                    {
                        if (code)
                        {
                            *code = META_IFACE_OK;
                        }
                        return static_cast<void *>(g_pExtensionIface);
                    }

                    if (code)
                    {
                        *code = META_IFACE_FAILED;
                    }
            #endif
            return NULL;
        })
    }
}
