//! The GL contexts virglrenderer renders with on macOS: ANGLE on Metal,
//! created by the caller.

use super::VirglRendererGlCtxParam;
use std::{
    ffi::{c_int, c_uint, c_void},
    sync::OnceLock,
};

type EglDisplay = *mut c_void;
type EglContext = *mut c_void;
type EglSurface = *mut c_void;
type EglConfig = *mut c_void;
type EglBoolean = c_uint;
type EglInt = c_int;
type EglEnum = c_uint;

const EGL_FALSE: EglBoolean = 0;
const EGL_NONE: EglInt = 0x3038;
const EGL_OPENGL_ES_API: EglEnum = 0x30A0;
const EGL_DEFAULT_DISPLAY: *mut c_void = std::ptr::null_mut();

const EGL_PLATFORM_ANGLE_ANGLE: EglEnum = 0x3202;
const EGL_PLATFORM_ANGLE_TYPE_ANGLE: EglInt = 0x3203;
const EGL_PLATFORM_ANGLE_TYPE_METAL_ANGLE: EglInt = 0x3489;

const EGL_SURFACE_TYPE: EglInt = 0x3033;
const EGL_PBUFFER_BIT: EglInt = 0x0001;
const EGL_RENDERABLE_TYPE: EglInt = 0x3040;
const EGL_OPENGL_ES2_BIT: EglInt = 0x0004;
const EGL_RED_SIZE: EglInt = 0x3024;
const EGL_GREEN_SIZE: EglInt = 0x3023;
const EGL_BLUE_SIZE: EglInt = 0x3022;
const EGL_ALPHA_SIZE: EglInt = 0x3021;
const EGL_WIDTH: EglInt = 0x3057;
const EGL_HEIGHT: EglInt = 0x3056;
const EGL_CONTEXT_CLIENT_VERSION: EglInt = 0x3098;

type EglGetPlatformDisplayExt = unsafe extern "C" fn(EglEnum, *mut c_void, *const EglInt) -> EglDisplay;

#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetProcAddress(name: *const c_char) -> *const c_void;
    fn eglGetError() -> EglInt;
    fn eglInitialize(dpy: EglDisplay, major: *mut EglInt, minor: *mut EglInt) -> EglBoolean;
    fn eglBindAPI(api: EglEnum) -> EglBoolean;
    fn eglChooseConfig(
        dpy: EglDisplay,
        attribs: *const EglInt,
        configs: *mut EglConfig,
        config_size: EglInt,
        num_config: *mut EglInt,
    ) -> EglBoolean;
    fn eglCreatePbufferSurface(dpy: EglDisplay, config: EglConfig, attribs: *const EglInt) -> EglSurface;
    fn eglCreateContext(
        dpy: EglDisplay,
        config: EglConfig,
        share_context: EglContext,
        attribs: *const EglInt,
    ) -> EglContext;
    fn eglDestroyContext(dpy: EglDisplay, ctx: EglContext) -> EglBoolean;
    fn eglMakeCurrent(dpy: EglDisplay, draw: EglSurface, read: EglSurface, ctx: EglContext) -> EglBoolean;
}

#[derive(Clone, Copy)]
struct AngleGlobals {
    display: EglDisplay,
    config: EglConfig,
    surface: EglSurface,
    share_context: EglContext,
}

unsafe impl Send for AngleGlobals {}
unsafe impl Sync for AngleGlobals {}

static ANGLE_GLOBAL: OnceLock<Result<AngleGlobals, EglInt>> = OnceLock::new();

fn angle_globals() -> Option<AngleGlobals> {
    ANGLE_GLOBAL.get().and_then(|r| r.ok())
}

fn angle_display() -> EglDisplay {
    angle_globals().map_or(std::ptr::null_mut(), |g| g.display)
}

fn angle_config() -> EglConfig {
    angle_globals().map_or(std::ptr::null_mut(), |g| g.config)
}

fn angle_surface() -> EglSurface {
    angle_globals().map_or(std::ptr::null_mut(), |g| g.surface)
}

fn angle_share_context() -> EglContext {
    angle_globals().map_or(std::ptr::null_mut(), |g| g.share_context)
}

pub(super) fn angle_init_once() -> Result<(), EglInt> {
    match ANGLE_GLOBAL.get_or_init(|| angle_init_impl()) {
        Ok(_) => Ok(()),
        Err(e) => Err(*e),
    }
}

fn angle_init_impl() -> Result<AngleGlobals, EglInt> {
    let proc_name = b"eglGetPlatformDisplayEXT\0";
    let proc = unsafe { eglGetProcAddress(proc_name.as_ptr() as *const c_char) };
    if proc.is_null() {
        eprintln!("ANGLE: eglGetPlatformDisplayEXT not found");
        return Err(-1);
    }

    let get_platform_display: EglGetPlatformDisplayExt = unsafe { std::mem::transmute(proc) };

    let display_attribs = [
        EGL_PLATFORM_ANGLE_TYPE_ANGLE,
        EGL_PLATFORM_ANGLE_TYPE_METAL_ANGLE,
        EGL_NONE,
    ];

    let dpy = unsafe { get_platform_display(EGL_PLATFORM_ANGLE_ANGLE, EGL_DEFAULT_DISPLAY, display_attribs.as_ptr()) };
    if dpy.is_null() {
        let err = unsafe { eglGetError() };
        eprintln!("ANGLE: eglGetPlatformDisplayEXT failed error={:#x}", err);
        return Err(err);
    }

    let mut major = 0;
    let mut minor = 0;
    if unsafe { eglInitialize(dpy, &mut major, &mut minor) } == EGL_FALSE {
        let err = unsafe { eglGetError() };
        eprintln!("ANGLE: eglInitialize failed error={:#x}", err);
        return Err(err);
    }

    if unsafe { eglBindAPI(EGL_OPENGL_ES_API) } == EGL_FALSE {
        let err = unsafe { eglGetError() };
        eprintln!("ANGLE: eglBindAPI failed error={:#x}", err);
        return Err(err);
    }

    let cfg_attribs = [
        EGL_SURFACE_TYPE,
        EGL_PBUFFER_BIT,
        EGL_RENDERABLE_TYPE,
        EGL_OPENGL_ES2_BIT,
        EGL_RED_SIZE,
        8,
        EGL_GREEN_SIZE,
        8,
        EGL_BLUE_SIZE,
        8,
        EGL_ALPHA_SIZE,
        8,
        EGL_NONE,
    ];

    let mut cfg: EglConfig = std::ptr::null_mut();
    let mut ncfg = 0;
    if unsafe { eglChooseConfig(dpy, cfg_attribs.as_ptr(), &mut cfg, 1, &mut ncfg) } == EGL_FALSE
        || ncfg < 1
        || cfg.is_null()
    {
        let err = unsafe { eglGetError() };
        eprintln!("ANGLE: eglChooseConfig failed error={:#x}", err);
        return Err(err);
    }

    let surf_attribs = [EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE];
    let surf = unsafe { eglCreatePbufferSurface(dpy, cfg, surf_attribs.as_ptr()) };
    if surf.is_null() {
        let err = unsafe { eglGetError() };
        eprintln!("ANGLE: eglCreatePbufferSurface failed error={:#x}", err);
        return Err(err);
    }

    let ctx_attribs = [EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE];
    let share = unsafe { eglCreateContext(dpy, cfg, std::ptr::null_mut(), ctx_attribs.as_ptr()) };
    if share.is_null() {
        let err = unsafe { eglGetError() };
        eprintln!("ANGLE: eglCreateContext(share) failed error={:#x}", err);
        return Err(err);
    }

    eprintln!(
        "ANGLE: initialized EGL {}.{} backend=metal display={:?} surface={:?} share_ctx={:?}",
        major, minor, dpy, surf, share
    );

    Ok(AngleGlobals {
        display: dpy,
        config: cfg,
        surface: surf,
        share_context: share,
    })
}

pub(super) extern "C" fn create_gl_context_trampoline(
    _cookie: *mut c_void,
    scanout_idx: c_int,
    param: *mut VirglRendererGlCtxParam,
) -> *mut c_void {
    if let Err(err) = angle_init_once() {
        eprintln!("ANGLE: init failed in create_gl_context err={:#x}", err);
        return std::ptr::null_mut();
    }

    let (major, requested_shared) = unsafe {
        if param.is_null() {
            (3, true)
        } else {
            let p = &*param;
            let major = if p.major_ver > 0 { p.major_ver.min(3) } else { 3 };
            (major, p.shared)
        }
    };

    let dpy = angle_display();
    let cfg = angle_config();

    // shared=true is mandatory: an isolated context breaks
    // transfer/sync with "Wait sync failed: illegal fence object".
    let share = angle_share_context();
    let shared = true;

    let ctx_attribs = [EGL_CONTEXT_CLIENT_VERSION, major, EGL_NONE];

    let ctx = unsafe { eglCreateContext(dpy, cfg, share, ctx_attribs.as_ptr()) };
    if ctx.is_null() {
        let err = unsafe { eglGetError() };
        eprintln!(
            "ANGLE: create_gl_context failed scanout_idx={} major={} shared={} requested_shared={} error={:#x}",
            scanout_idx, major, shared, requested_shared, err
        );
    } else {
        eprintln!(
            "ANGLE: create_gl_context scanout_idx={} major={} shared={} requested_shared={} ctx={:?}",
            scanout_idx, major, shared, requested_shared, ctx
        );
    }

    ctx
}

pub(super) extern "C" fn destroy_gl_context_trampoline(_cookie: *mut c_void, ctx: *mut c_void) {
    if ctx.is_null() {
        return;
    }

    let dpy = angle_display();
    let ret = unsafe { eglDestroyContext(dpy, ctx as EglContext) };
    if ret == EGL_FALSE {
        let err = unsafe { eglGetError() };
        eprintln!("ANGLE: destroy_gl_context {:?} failed error={:#x}", ctx, err);
    } else {
        eprintln!("ANGLE: destroy_gl_context {:?}", ctx);
    }
}

pub(super) extern "C" fn make_current_trampoline(_cookie: *mut c_void, scanout_idx: c_int, ctx: *mut c_void) -> c_int {
    let dpy = angle_display();
    if dpy.is_null() {
        eprintln!("ANGLE: make_current with uninitialised display");
        return -1;
    }

    let ok = if ctx.is_null() {
        unsafe { eglMakeCurrent(dpy, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) }
    } else {
        unsafe { eglMakeCurrent(dpy, angle_surface(), angle_surface(), ctx as EglContext) }
    };

    if ok == EGL_FALSE {
        let err = unsafe { eglGetError() };
        eprintln!(
            "ANGLE: make_current failed scanout_idx={} ctx={:?} error={:#x}",
            scanout_idx, ctx, err
        );
        -1
    } else {
        0
    }
}

pub(super) extern "C" fn get_egl_display_trampoline(_cookie: *mut c_void) -> *mut c_void {
    angle_display()
}
